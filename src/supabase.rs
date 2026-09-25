//! Push and pull silo events via Supabase PostgREST (`silo_events`),
//! and upload live/alert JPEG stills to Storage (`silo-frames`).
//!
//! Writes go through a disk-backed outbox so the plant keeps running offline.
//! Reads (Stats) come from Supabase whenever the table is reachable.
//! Frame uploads run on a background thread (latest.jpg upsert).

use crate::config::SupabaseCfg;
use crate::stats::Stats;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OUTBOX_PATH: &str = "data/outbox.jsonl";
/// Keep the newest rows if the site stays offline for a very long time.
const OUTBOX_MAX: usize = 2000;
const FLUSH_EVERY: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const MISSING_LOG_EVERY: Duration = Duration::from_secs(60);
const SQL_HINT: &str = "run sql/supabase_silo.sql in the Supabase SQL Editor";
const SQL_EDITOR: &str =
    "https://supabase.com/dashboard/project/mranicltlsnjurjqwjpv/sql/new";
const FRAME_BUCKET: &str = "silo-frames";

static PENDING: AtomicUsize = AtomicUsize::new(0);
static SCHEMA_MISSING: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Rows still waiting to reach Supabase.
pub fn pending() -> usize {
    PENDING.load(Ordering::Relaxed)
}

/// True while PostgREST answers that `silo_events` is not in the schema cache.
pub fn schema_missing() -> bool {
    SCHEMA_MISSING.load(Ordering::Relaxed)
}

/// Latest send/pull problem, if any. Cleared after a successful insert.
pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().ok().and_then(|g| g.clone())
}

fn set_error(msg: Option<String>) {
    if let Ok(mut g) = LAST_ERROR.lock() {
        *g = msg;
    }
}

#[derive(Clone)]
pub struct Supabase {
    site_id: String,
    base: String,
    key: String,
    outbox: Arc<Mutex<VecDeque<String>>>,
}

impl Supabase {
    pub fn from_cfg(cfg: &SupabaseCfg) -> Option<Self> {
        let url = std::env::var("SUPABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cfg.url.clone());
        let key = std::env::var("SUPABASE_KEY")
            .ok()
            .or_else(|| std::env::var("NEXT_PUBLIC_SUPABASE_PUBLISHABLE_KEY").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cfg.key.clone());
        if url.is_empty() || key.is_empty() || !cfg.enabled {
            return None;
        }
        let site_id = if cfg.site_id.is_empty() {
            "spectr-pi".into()
        } else {
            cfg.site_id.clone()
        };
        let base = url.trim_end_matches('/').to_string();

        let outbox = Arc::new(Mutex::new(load_outbox()));
        PENDING.store(outbox.lock().map(|q| q.len()).unwrap_or(0), Ordering::Relaxed);

        let this = Self {
            site_id,
            base: base.clone(),
            key: key.clone(),
            outbox: outbox.clone(),
        };
        spawn_sender(base, key, outbox);
        Some(this)
    }

    pub fn site_id(&self) -> &str {
        &self.site_id
    }

    pub fn push_empty_alert(&self, unix_secs: u64) {
        self.enqueue(EventRow {
            site_id: &self.site_id,
            ts: Some(unix_to_rfc3339(unix_secs)),
            kind: "empty_alert",
            empty: Some(true),
            confidence: None,
            checks: None,
            empty_hits: None,
            alerts_sent: None,
            labels_empty: None,
            labels_full: None,
        });
    }

    pub fn push_event(&self, kind: &str, empty: Option<bool>, confidence: Option<f32>) {
        self.enqueue(EventRow {
            site_id: &self.site_id,
            ts: Some(now_rfc3339()),
            kind,
            empty,
            confidence,
            checks: None,
            empty_hits: None,
            alerts_sent: None,
            labels_empty: None,
            labels_full: None,
        });
    }

    pub fn push_heartbeat(&self, stats: &Stats) {
        self.enqueue(EventRow {
            site_id: &self.site_id,
            ts: Some(now_rfc3339()),
            kind: "heartbeat",
            empty: stats.last_check_empty,
            confidence: None,
            checks: Some(stats.checks),
            empty_hits: Some(stats.empty_hits),
            alerts_sent: Some(stats.alerts_sent),
            labels_empty: Some(stats.labels_empty),
            labels_full: Some(stats.labels_full),
        });
    }

    /// Upload a JPEG to the `silo-frames` bucket (e.g. `{site}/latest.jpg`).
    /// Runs on a background thread so the UI never blocks on Storage.
    pub fn upload_jpeg(&self, object_path: &str, jpeg: Vec<u8>) {
        if jpeg.is_empty() || object_path.is_empty() {
            return;
        }
        let url = format!(
            "{}/storage/v1/object/{}/{}",
            self.base,
            FRAME_BUCKET,
            object_path.trim_start_matches('/')
        );
        let key = self.key.clone();
        thread::spawn(move || {
            match ureq::put(&url)
                .set("apikey", &key)
                .set("Authorization", &format!("Bearer {key}"))
                .set("Content-Type", "image/jpeg")
                .set("x-upsert", "true")
                .timeout(Duration::from_secs(20))
                .send_bytes(&jpeg)
            {
                Ok(resp) => {
                    let status = resp.status();
                    if !(200..300).contains(&status) {
                        eprintln!("supabase storage: HTTP {status} for {url}");
                    }
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let body = resp.into_string().unwrap_or_default();
                    let snippet: String = body.chars().take(120).collect();
                    eprintln!("supabase storage: HTTP {code} {snippet}");
                }
                Err(e) => eprintln!("supabase storage: {e}"),
            }
        });
    }

    /// Live preview path for this site.
    pub fn latest_frame_path(&self) -> String {
        format!("{}/latest.jpg", self.site_id)
    }

    /// Alert evidence path for this site.
    pub fn alert_frame_path(&self, unix: u64) -> String {
        format!("{}/alerts/{unix}.jpg", self.site_id)
    }

    /// Pull empty-alert timestamps for this site (newest first, then sorted asc).
    pub fn fetch_empty_alerts(&self, limit: usize) -> Result<Vec<u64>, String> {
        let lim = limit.clamp(1, 2000);
        let url = format!(
            "{}/rest/v1/silo_events?select=ts&site_id=eq.{}&kind=eq.empty_alert&order=ts.desc&limit={}",
            self.base,
            urlencoding_site(&self.site_id),
            lim
        );
        let rows: Vec<TsRow> = get_json(&url, &self.key)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(u) = parse_rfc3339_unix(&row.ts) {
                out.push(u);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn enqueue(&self, row: EventRow<'_>) {
        let Ok(body) = serde_json::to_string(&row) else {
            return;
        };
        let Ok(mut q) = self.outbox.lock() else {
            return;
        };
        q.push_back(body);
        while q.len() > OUTBOX_MAX {
            q.pop_front();
        }
        PENDING.store(q.len(), Ordering::Relaxed);
        save_outbox(&q);
    }
}

#[derive(Deserialize)]
struct TsRow {
    ts: String,
}

fn urlencoding_site(site: &str) -> String {
    // site_id is controlled (config); keep PostgREST filter safe.
    site.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

fn get_json<T: for<'de> Deserialize<'de>>(url: &str, key: &str) -> Result<T, String> {
    match ureq::get(url)
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(12))
        .call()
    {
        Ok(resp) => {
            SCHEMA_MISSING.store(false, Ordering::Relaxed);
            let status = resp.status();
            let text = resp.into_string().map_err(|e| e.to_string())?;
            if !(200..300).contains(&status) {
                if is_missing_table(status, &text) {
                    SCHEMA_MISSING.store(true, Ordering::Relaxed);
                    return Err(format!("silo_events missing — {SQL_HINT}"));
                }
                return Err(format!("HTTP {status}"));
            }
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            if is_missing_table(code, &text) {
                SCHEMA_MISSING.store(true, Ordering::Relaxed);
                Err(format!("silo_events missing — {SQL_HINT}"))
            } else {
                Err(format!("HTTP {code}"))
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

fn parse_rfc3339_unix(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp().max(0) as u64)
}

enum SendOutcome {
    Sent,
    Drop(String),
    Missing,
    Retry(String),
}

fn spawn_sender(url: String, key: String, outbox: Arc<Mutex<VecDeque<String>>>) {
    thread::spawn(move || {
        let endpoint = format!("{url}/rest/v1/silo_events");
        probe_table(&url, &key);
        let mut backoff = FLUSH_EVERY;
        let mut last_missing_log = Instant::now()
            .checked_sub(MISSING_LOG_EVERY)
            .unwrap_or_else(Instant::now);
        loop {
            let next = outbox.lock().ok().and_then(|q| q.front().cloned());
            let Some(body) = next else {
                thread::sleep(FLUSH_EVERY);
                backoff = FLUSH_EVERY;
                continue;
            };
            match send(&endpoint, &key, &body) {
                SendOutcome::Sent => {
                    SCHEMA_MISSING.store(false, Ordering::Relaxed);
                    set_error(None);
                    if let Ok(mut q) = outbox.lock() {
                        q.pop_front();
                        PENDING.store(q.len(), Ordering::Relaxed);
                        save_outbox(&q);
                    }
                    backoff = FLUSH_EVERY;
                }
                SendOutcome::Drop(why) => {
                    eprintln!("supabase: dropping rejected row ({why})");
                    set_error(Some(format!("dropped row: {why}")));
                    if let Ok(mut q) = outbox.lock() {
                        q.pop_front();
                        PENDING.store(q.len(), Ordering::Relaxed);
                        save_outbox(&q);
                    }
                    backoff = FLUSH_EVERY;
                }
                SendOutcome::Missing => {
                    SCHEMA_MISSING.store(true, Ordering::Relaxed);
                    set_error(Some(format!("silo_events missing — {SQL_HINT}")));
                    if last_missing_log.elapsed() >= MISSING_LOG_EVERY {
                        eprintln!("supabase: silo_events missing — {SQL_HINT}");
                        eprintln!("supabase: {SQL_EDITOR}");
                        last_missing_log = Instant::now();
                    }
                    thread::sleep(BACKOFF_MAX);
                    backoff = BACKOFF_MAX;
                }
                SendOutcome::Retry(why) => {
                    set_error(Some(why.clone()));
                    eprintln!("supabase: {why} (queued, retrying)");
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    });
}

fn probe_table(url: &str, key: &str) {
    let probe = format!("{url}/rest/v1/silo_events?select=id&limit=1");
    match ureq::get(&probe)
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .timeout(Duration::from_secs(10))
        .call()
    {
        Ok(_) => {
            SCHEMA_MISSING.store(false, Ordering::Relaxed);
            eprintln!("supabase: silo_events reachable");
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            if is_missing_table(code, &body) {
                SCHEMA_MISSING.store(true, Ordering::Relaxed);
                set_error(Some(format!("silo_events missing — {SQL_HINT}")));
                eprintln!("supabase: silo_events missing — {SQL_HINT}");
                eprintln!("supabase: {SQL_EDITOR}");
            } else {
                set_error(Some(format!("HTTP {code}")));
                eprintln!("supabase: probe HTTP {code}");
            }
        }
        Err(e) => {
            set_error(Some(e.to_string()));
            eprintln!("supabase: probe {e}");
        }
    }
}

fn send(endpoint: &str, key: &str, body: &str) -> SendOutcome {
    match ureq::post(endpoint)
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .set("Prefer", "return=minimal")
        .timeout(Duration::from_secs(15))
        .send_string(body)
    {
        Ok(resp) => classify(resp.status(), ""),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            classify(code, &text)
        }
        Err(e) => SendOutcome::Retry(e.to_string()),
    }
}

fn classify(status: u16, body: &str) -> SendOutcome {
    if (200..300).contains(&status) {
        return SendOutcome::Sent;
    }
    if is_missing_table(status, body) {
        return SendOutcome::Missing;
    }
    let snippet: String = body.chars().take(160).collect();
    if status == 429 || (500..600).contains(&status) {
        return SendOutcome::Retry(format!("HTTP {status} {snippet}"));
    }
    if status == 401 || status == 403 {
        return SendOutcome::Retry(format!("HTTP {status} (auth/RLS)"));
    }
    if (400..500).contains(&status) {
        return SendOutcome::Drop(format!("HTTP {status} {snippet}"));
    }
    SendOutcome::Retry(format!("HTTP {status} {snippet}"))
}

fn is_missing_table(status: u16, body: &str) -> bool {
    status == 404
        || body.contains("PGRST205")
        || (body.contains("silo_events") && body.contains("schema cache"))
}

fn load_outbox() -> VecDeque<String> {
    let Ok(text) = fs::read_to_string(OUTBOX_PATH) else {
        return VecDeque::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

fn save_outbox(q: &VecDeque<String>) {
    if let Some(parent) = Path::new(OUTBOX_PATH).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if q.is_empty() {
        let _ = fs::remove_file(OUTBOX_PATH);
        return;
    }
    let mut out = String::new();
    for line in q {
        out.push_str(line);
        out.push('\n');
    }
    let _ = fs::write(OUTBOX_PATH, out);
}

#[derive(Serialize)]
struct EventRow<'a> {
    site_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ts: Option<String>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_hits: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alerts_sent: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels_empty: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels_full: Option<u64>,
}

fn now_rfc3339() -> String {
    unix_to_rfc3339(now_unix())
}

fn unix_to_rfc3339(unix: u64) -> String {
    chrono::DateTime::from_timestamp(unix as i64, 0)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| format!("{unix}"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
