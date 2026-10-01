//! Push and pull silo data via Supabase PostgREST.
//!
//! Each fact has its own table (`silo_empty`, `silo_full`, `silo_radio_tx`, …).
//! Heartbeat, power, camera and arming rows are deleted after 30 days.
//! Stills go to Storage (`silo-frames`).
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

/// True while PostgREST answers that the silo tables are not in the schema cache.
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
        self.enqueue(
            "silo_empty",
            &When {
                site_id: &self.site_id,
                ts: unix_to_rfc3339(unix_secs),
            },
        );
    }

    pub fn push_event(&self, kind: &str, empty: Option<bool>, confidence: Option<f32>) {
        let ts = now_rfc3339();
        match kind {
            "full" => self.enqueue(
                "silo_full",
                &FullRow {
                    site_id: &self.site_id,
                    ts,
                    confidence,
                },
            ),
            "radio_tx" | "radio_fail" | "app_start" | "armed" | "disarmed" => {
                let table = match kind {
                    "radio_tx" => "silo_radio_tx",
                    "radio_fail" => "silo_radio_fail",
                    "app_start" => "silo_app_start",
                    "armed" => "silo_armed",
                    _ => "silo_disarmed",
                };
                self.enqueue(
                    table,
                    &WhenEmpty {
                        site_id: &self.site_id,
                        ts,
                        empty,
                    },
                );
            }
            "camera_up" => self.enqueue(
                "silo_camera_up",
                &When {
                    site_id: &self.site_id,
                    ts,
                },
            ),
            "camera_down" => self.enqueue(
                "silo_camera_down",
                &When {
                    site_id: &self.site_id,
                    ts,
                },
            ),
            other => eprintln!("supabase: unknown event {other}"),
        }
    }

    /// `throttled` is the `vcgencmd get_throttled` bitmask (0 = healthy).
    pub fn push_power(&self, throttled: u32) {
        self.enqueue(
            "silo_power",
            &PowerRow {
                site_id: &self.site_id,
                ts: now_rfc3339(),
                throttled,
            },
        );
    }

    pub fn push_heartbeat(&self, stats: &Stats) {
        self.enqueue(
            "silo_heartbeat",
            &HeartbeatRow {
                site_id: &self.site_id,
                ts: now_rfc3339(),
                empty: stats.last_check_empty,
                checks: stats.checks,
                empty_hits: stats.empty_hits,
                alerts_sent: stats.alerts_sent,
                labels_empty: stats.labels_empty,
                labels_full: stats.labels_full,
            },
        );
    }

    /// Upload a JPEG to the `silo-frames` bucket (e.g. `{site}/latest.jpg`).
    /// Runs on a background thread so the UI never blocks on Storage.
    pub fn upload_jpeg(&self, object_path: &str, jpeg: Vec<u8>) {
        self.upload_bytes(object_path, jpeg, "image/jpeg");
    }

    /// Upload bytes to Storage. The Pi does not keep a second copy in memory.
    pub fn upload_bytes(&self, object_path: &str, bytes: Vec<u8>, content_type: &str) {
        if bytes.is_empty() || object_path.is_empty() {
            return;
        }
        let url = format!(
            "{}/storage/v1/object/{}/{}",
            self.base,
            FRAME_BUCKET,
            object_path.trim_start_matches('/')
        );
        let key = self.key.clone();
        let content_type = content_type.to_string();
        thread::spawn(move || {
            match ureq::put(&url)
                .set("apikey", &key)
                .set("Authorization", &format!("Bearer {key}"))
                .set("Content-Type", &content_type)
                .set("x-upsert", "true")
                .timeout(Duration::from_secs(20))
                .send_bytes(&bytes)
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

    /// Read one object back from Storage.
    pub fn download_object(&self, object_path: &str) -> Result<Vec<u8>, String> {
        let url = format!(
            "{}/storage/v1/object/{}/{}",
            self.base,
            FRAME_BUCKET,
            object_path.trim_start_matches('/')
        );
        let resp = ureq::get(&url)
            .set("apikey", &self.key)
            .set("Authorization", &format!("Bearer {}", self.key))
            .timeout(Duration::from_secs(12))
            .call()
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        use std::io::Read;
        resp.into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Err("empty object".into());
        }
        Ok(bytes)
    }

    /// Live preview path for this site.
    pub fn latest_frame_path(&self) -> String {
        format!("{}/latest.jpg", self.site_id)
    }

    /// Alert evidence path for this site.
    pub fn alert_frame_path(&self, unix: u64) -> String {
        format!("{}/alerts/{unix}.jpg", self.site_id)
    }

    /// The comparison model (feature vectors), small enough to pull back.
    pub fn reference_path(&self) -> String {
        format!("{}/reference.json", self.site_id)
    }

    pub fn empty_photo_path(&self, file_name: &str) -> String {
        format!("{}/empty/{file_name}", self.site_id)
    }

    /// Drop machine logs older than 30 days. Empty, full and radio stay.
    pub fn purge_expired_logs(&self) {
        let cutoff = now_unix().saturating_sub(30 * 24 * 60 * 60);
        let ts = query_escape(&unix_to_rfc3339(cutoff));
        let site = urlencoding_site(&self.site_id);
        let key = self.key.clone();
        let base = self.base.clone();
        thread::spawn(move || {
            for table in [
                "silo_heartbeat",
                "silo_power",
                "silo_camera_up",
                "silo_camera_down",
                "silo_app_start",
                "silo_armed",
                "silo_disarmed",
            ] {
                let url = format!("{base}/rest/v1/{table}?site_id=eq.{site}&ts=lt.{ts}");
                purge_one(&url, &key, table);
            }
        });
    }

    /// Pull empty-alert timestamps for this site (newest first, then sorted asc).
    pub fn fetch_empty_alerts(&self, limit: usize) -> Result<Vec<u64>, String> {
        let lim = limit.clamp(1, 2000);
        let url = format!(
            "{}/rest/v1/silo_empty?select=ts&site_id=eq.{}&order=ts.desc&limit={}",
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

    fn enqueue<T: Serialize>(&self, table: &str, row: &T) {
        let Ok(body) = serde_json::to_string(row) else {
            return;
        };
        let line = format!(r#"{{"table":"{table}","body":{body}}}"#);
        let Ok(mut q) = self.outbox.lock() else {
            return;
        };
        let body = line;
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
    query_escape(site)
}

fn query_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' => c.to_string(),
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
                    return Err(format!("tables missing — {SQL_HINT}"));
                }
                return Err(format!("HTTP {status}"));
            }
            serde_json::from_str(&text).map_err(|e| e.to_string())
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            if is_missing_table(code, &text) {
                SCHEMA_MISSING.store(true, Ordering::Relaxed);
                Err(format!("tables missing — {SQL_HINT}"))
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
        probe_table(&url, &key);
        let mut backoff = FLUSH_EVERY;
        let mut last_missing_log = Instant::now()
            .checked_sub(MISSING_LOG_EVERY)
            .unwrap_or_else(Instant::now);
        loop {
            let next = outbox.lock().ok().and_then(|q| q.front().cloned());
            let Some(line) = next else {
                thread::sleep(FLUSH_EVERY);
                backoff = FLUSH_EVERY;
                continue;
            };
            let (table, body) = queued_target(&line);
            let endpoint = format!("{url}/rest/v1/{table}");
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
                    set_error(Some(format!("tables missing — {SQL_HINT}")));
                    if last_missing_log.elapsed() >= MISSING_LOG_EVERY {
                        eprintln!("supabase: tables missing — {SQL_HINT}");
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
    let probe = format!("{url}/rest/v1/silo_empty?select=id&limit=1");
    match ureq::get(&probe)
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .timeout(Duration::from_secs(10))
        .call()
    {
        Ok(_) => {
            SCHEMA_MISSING.store(false, Ordering::Relaxed);
            eprintln!("supabase: silo_empty reachable");
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            if is_missing_table(code, &body) {
                SCHEMA_MISSING.store(true, Ordering::Relaxed);
                set_error(Some(format!("tables missing — {SQL_HINT}")));
                eprintln!("supabase: tables missing — {SQL_HINT}");
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
    status == 404 || body.contains("PGRST205") || body.contains("schema cache")
}

/// Outbox line is `{"table":"...","body":{...}}`. Older lines were a bare row
/// for `silo_events`.
fn queued_target(line: &str) -> (String, String) {
    #[derive(Deserialize)]
    struct Wrap {
        table: String,
        body: serde_json::Value,
    }
    if let Ok(wrap) = serde_json::from_str::<Wrap>(line) {
        if !wrap.table.is_empty() && wrap.body.is_object() {
            return (wrap.table, wrap.body.to_string());
        }
    }
    ("silo_events".into(), line.to_string())
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

fn purge_one(url: &str, key: &str, table: &str) {
    match ureq::delete(url)
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Prefer", "return=minimal")
        .timeout(Duration::from_secs(20))
        .call()
    {
        Ok(resp) => {
            let status = resp.status();
            if (200..300).contains(&status) {
                eprintln!("supabase: purged {table} older than 30 days");
            } else {
                eprintln!("supabase: {table} purge HTTP {status}");
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(160).collect();
            eprintln!("supabase: {table} purge HTTP {code} {snippet}");
        }
        Err(e) => eprintln!("supabase: {table} purge {e}"),
    }
}

#[derive(Serialize)]
struct When<'a> {
    site_id: &'a str,
    ts: String,
}

#[derive(Serialize)]
struct WhenEmpty<'a> {
    site_id: &'a str,
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty: Option<bool>,
}

#[derive(Serialize)]
struct FullRow<'a> {
    site_id: &'a str,
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f32>,
}

#[derive(Serialize)]
struct PowerRow<'a> {
    site_id: &'a str,
    ts: String,
    throttled: u32,
}

#[derive(Serialize)]
struct HeartbeatRow<'a> {
    site_id: &'a str,
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty: Option<bool>,
    checks: u64,
    empty_hits: u64,
    alerts_sent: u64,
    labels_empty: u64,
    labels_full: u64,
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
