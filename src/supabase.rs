//! Push and pull silo data via Supabase PostgREST.
//!
//! Each fact has its own table (`silo_empty`, `silo_full`, `silo_radio_tx`, …).
//! Empty alerts store `ts` + `image_path` (JPEG under `silo-frames/{site}/alerts/`).
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
const SQL_HINT: &str = "run sql/silo_empty_images.sql (and sql/silo_reference_images.sql if needed) in the Supabase SQL Editor";
const SQL_EDITOR: &str =
    "https://supabase.com/dashboard/project/mranicltlsnjurjqwjpv/sql/new";
const FRAME_BUCKET: &str = "silo-frames";

static PENDING: AtomicUsize = AtomicUsize::new(0);
static SCHEMA_MISSING: AtomicBool = AtomicBool::new(false);
static REF_TABLE_WARNED: AtomicBool = AtomicBool::new(false);
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

    /// Record an empty alert: timestamp + Storage path for the evidence JPEG.
    pub fn push_empty_alert(&self, unix_secs: u64) {
        let image_path = self.alert_frame_path(unix_secs);
        self.enqueue(
            "silo_empty",
            &EmptyAlertRow {
                site_id: &self.site_id,
                ts: unix_to_rfc3339(unix_secs),
                image_path: &image_path,
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
            put_storage(&url, &key, &content_type, &bytes);
        });
    }

    /// Blocking Storage PUT — used when metadata must follow the file.
    fn upload_bytes_blocking(&self, object_path: &str, bytes: &[u8], content_type: &str) {
        if bytes.is_empty() || object_path.is_empty() {
            return;
        }
        let url = format!(
            "{}/storage/v1/object/{}/{}",
            self.base,
            FRAME_BUCKET,
            object_path.trim_start_matches('/')
        );
        put_storage(&url, &self.key, content_type, bytes);
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

    /// Colour JPEG path for a labeled reference photo.
    pub fn reference_colour_path(&self, file_name: &str, empty: bool) -> String {
        let folder = if empty { "empty" } else { "full" };
        format!("{}/{folder}/{file_name}", self.site_id)
    }

    /// B&W twin path for a reference photo.
    pub fn reference_bw_path(&self, file_name: &str) -> String {
        format!("{}/empty_bw/{file_name}", self.site_id)
    }

    /// Upload one reference photo (colour + B&W) and upsert its metadata row.
    pub fn sync_reference_image(&self, meta: &crate::vision::SampleMeta) {
        let empty = meta.label_empty != Some(false);
        let label = if empty { "empty" } else { "not_empty" };
        let colour_path = self.reference_colour_path(&meta.file_name, empty);
        let bw_path = self.reference_bw_path(&meta.file_name);
        let Ok(colour) = fs::read(&meta.path) else {
            eprintln!("supabase: missing colour {}", meta.path.display());
            return;
        };
        let (width, height) = jpeg_size(&colour).unwrap_or((0, 0));
        self.upload_bytes_blocking(&colour_path, &colour, "image/jpeg");

        let bw_file = crate::vision::bw_path_for(&meta.file_name);
        let bw_storage = if bw_file.is_file() {
            if let Ok(bytes) = fs::read(&bw_file) {
                self.upload_bytes_blocking(&bw_path, &bytes, "image/jpeg");
                Some(bw_path.clone())
            } else {
                None
            }
        } else {
            None
        };

        // Remove the colour object from the other label folder so relabel stays clean.
        let other = self.reference_colour_path(&meta.file_name, !empty);
        let _ = ureq::delete(&format!(
            "{}/storage/v1/object/{}/{}",
            self.base,
            FRAME_BUCKET,
            other.trim_start_matches('/')
        ))
        .set("apikey", &self.key)
        .set("Authorization", &format!("Bearer {}", self.key))
        .timeout(Duration::from_secs(15))
        .call();

        self.upsert_reference_image_blocking(&ReferenceImageRow {
            site_id: &self.site_id,
            file_name: &meta.file_name,
            label,
            captured_ms: meta.captured_ms,
            bytes: Some(meta.bytes as i64),
            width: if width > 0 { Some(width as i32) } else { None },
            height: if height > 0 { Some(height as i32) } else { None },
            colour_path: &colour_path,
            bw_path: bw_storage.as_deref(),
            updated_at: now_rfc3339(),
        });
    }

    /// Push every local Empty / Not empty photo + metadata to Supabase.
    pub fn sync_all_reference_images(&self) {
        let site = self.clone();
        thread::spawn(move || {
            let samples = crate::vision::list_references();
            eprintln!(
                "supabase: syncing {} reference images to storage + silo_reference_images",
                samples.len()
            );
            for meta in &samples {
                site.sync_reference_image(meta);
            }
            eprintln!("supabase: reference image sync finished ({})", samples.len());
        });
    }

    /// Drop a reference photo from Storage and the metadata table.
    pub fn delete_reference_image(&self, file_name: &str) {
        if file_name.is_empty() {
            return;
        }
        self.delete_storage_object(&self.reference_colour_path(file_name, true));
        self.delete_storage_object(&self.reference_colour_path(file_name, false));
        self.delete_storage_object(&self.reference_bw_path(file_name));
        let url = format!(
            "{}/rest/v1/silo_reference_images?site_id=eq.{}&file_name=eq.{}",
            self.base,
            urlencoding_site(&self.site_id),
            query_escape(file_name)
        );
        let key = self.key.clone();
        thread::spawn(move || {
            match ureq::delete(&url)
                .set("apikey", &key)
                .set("Authorization", &format!("Bearer {key}"))
                .set("Prefer", "return=minimal")
                .timeout(Duration::from_secs(15))
                .call()
            {
                Ok(resp) if (200..300).contains(&resp.status()) => {}
                Ok(resp) => eprintln!(
                    "supabase: delete silo_reference_images HTTP {}",
                    resp.status()
                ),
                Err(ureq::Error::Status(code, resp)) => {
                    let body = resp.into_string().unwrap_or_default();
                    if is_missing_table(code, &body) {
                        SCHEMA_MISSING.store(true, Ordering::Relaxed);
                        eprintln!("supabase: silo_reference_images missing — {SQL_HINT}");
                    } else {
                        let snippet: String = body.chars().take(120).collect();
                        eprintln!("supabase: delete silo_reference_images HTTP {code} {snippet}");
                    }
                }
                Err(e) => eprintln!("supabase: delete silo_reference_images {e}"),
            }
        });
    }

    fn delete_storage_object(&self, object_path: &str) {
        if object_path.is_empty() {
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
            let _ = ureq::delete(&url)
                .set("apikey", &key)
                .set("Authorization", &format!("Bearer {key}"))
                .timeout(Duration::from_secs(15))
                .call();
        });
    }

    fn upsert_reference_image_blocking(&self, row: &ReferenceImageRow<'_>) {
        let Ok(body) = serde_json::to_string(row) else {
            return;
        };
        let url = format!(
            "{}/rest/v1/silo_reference_images?on_conflict=site_id,file_name",
            self.base
        );
        match ureq::post(&url)
            .set("apikey", &self.key)
            .set("Authorization", &format!("Bearer {}", self.key))
            .set("Content-Type", "application/json")
            .set("Prefer", "resolution=merge-duplicates,return=minimal")
            .timeout(Duration::from_secs(20))
            .send_string(&body)
        {
            Ok(resp) if (200..300).contains(&resp.status()) => {
                SCHEMA_MISSING.store(false, Ordering::Relaxed);
            }
            Ok(resp) => {
                eprintln!(
                    "supabase: silo_reference_images upsert HTTP {}",
                    resp.status()
                );
            }
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                if is_missing_table(code, &text) {
                    SCHEMA_MISSING.store(true, Ordering::Relaxed);
                    set_error(Some(format!(
                        "silo_reference_images missing — {SQL_HINT}"
                    )));
                    if !REF_TABLE_WARNED.swap(true, Ordering::Relaxed) {
                        eprintln!("supabase: silo_reference_images missing — {SQL_HINT}");
                        eprintln!("supabase: {SQL_EDITOR}");
                    }
                } else {
                    let snippet: String = text.chars().take(160).collect();
                    eprintln!("supabase: silo_reference_images HTTP {code} {snippet}");
                }
            }
            Err(e) => eprintln!("supabase: silo_reference_images {e}"),
        }
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

    /// Fill NULL `image_path` on existing `silo_empty` rows from `{site}/alerts/{unix}.jpg`.
    /// Needs the UPDATE policy from `sql/silo_empty_images.sql`.
    pub fn backfill_empty_image_paths(&self) {
        let site = self.site_id.clone();
        let base = self.base.clone();
        let key = self.key.clone();
        thread::spawn(move || {
            let url = format!(
                "{base}/rest/v1/silo_empty?select=id,ts,image_path&site_id=eq.{}&image_path=is.null&order=ts.desc&limit=500",
                urlencoding_site(&site)
            );
            let rows: Vec<EmptyIdRow> = match get_json(&url, &key) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("supabase: image_path backfill list failed: {e}");
                    return;
                }
            };
            if rows.is_empty() {
                return;
            }
            let mut ok = 0u32;
            let mut fail = 0u32;
            for row in &rows {
                let Some(unix) = parse_rfc3339_unix(&row.ts) else {
                    fail += 1;
                    continue;
                };
                let image_path = format!("{site}/alerts/{unix}.jpg");
                let patch_url = format!("{base}/rest/v1/silo_empty?id=eq.{}", row.id);
                let body = serde_json::json!({ "image_path": image_path }).to_string();
                match ureq::request("PATCH", &patch_url)
                    .set("apikey", &key)
                    .set("Authorization", &format!("Bearer {key}"))
                    .set("Content-Type", "application/json")
                    .set("Prefer", "return=representation")
                    .timeout(Duration::from_secs(15))
                    .send_string(&body)
                {
                    Ok(resp) if (200..300).contains(&resp.status()) => {
                        let text = resp.into_string().unwrap_or_default();
                        // RLS can return 200 with [] when zero rows were updated.
                        if text.trim_start().starts_with('[') && !text.contains("\"id\"") {
                            fail += 1;
                            if fail == 1 {
                                eprintln!(
                                    "supabase: image_path backfill blocked by RLS — run sql/silo_empty_images.sql"
                                );
                            }
                        } else {
                            ok += 1;
                        }
                    }
                    Ok(resp) => {
                        fail += 1;
                        eprintln!(
                            "supabase: image_path backfill HTTP {} — run sql/silo_empty_images.sql",
                            resp.status()
                        );
                    }
                    Err(ureq::Error::Status(code, resp)) => {
                        fail += 1;
                        let text = resp.into_string().unwrap_or_default();
                        let snippet: String = text.chars().take(120).collect();
                        eprintln!(
                            "supabase: image_path backfill HTTP {code} {snippet} — run sql/silo_empty_images.sql"
                        );
                    }
                    Err(e) => {
                        fail += 1;
                        eprintln!("supabase: image_path backfill {e}");
                    }
                }
            }
            eprintln!(
                "supabase: image_path backfill done ok={ok} fail={fail} (of {} null rows)",
                rows.len()
            );
        });
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

#[derive(Deserialize)]
struct EmptyIdRow {
    id: i64,
    ts: String,
    #[serde(default)]
    #[allow(dead_code)]
    image_path: Option<String>,
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
    // Column not migrated yet — keep the row queued until SQL is applied.
    if is_missing_column(status, body) {
        return SendOutcome::Retry(format!(
            "column missing — {SQL_HINT} ({snippet})"
        ));
    }
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

fn is_missing_column(status: u16, body: &str) -> bool {
    (status == 400 || status == 404)
        && (body.contains("PGRST204")
            || body.contains("Could not find the")
            || body.contains("image_path"))
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
struct EmptyAlertRow<'a> {
    site_id: &'a str,
    ts: String,
    image_path: &'a str,
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

#[derive(Serialize)]
struct ReferenceImageRow<'a> {
    site_id: &'a str,
    file_name: &'a str,
    label: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    captured_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<i32>,
    colour_path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    bw_path: Option<&'a str>,
    updated_at: String,
}

fn put_storage(url: &str, key: &str, content_type: &str, bytes: &[u8]) {
    match ureq::put(url)
        .set("apikey", key)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", content_type)
        .set("x-upsert", "true")
        .timeout(Duration::from_secs(30))
        .send_bytes(bytes)
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
}

/// Read SOF0/SOF2 dimensions from a JPEG without a full decode.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        i += 2;
        if marker == 0xD8 || marker == 0xD9 {
            continue;
        }
        if i + 2 > bytes.len() {
            break;
        }
        let seglen = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
        if seglen < 2 || i + seglen > bytes.len() {
            break;
        }
        // SOF0 / SOF2
        if matches!(marker, 0xC0 | 0xC2) && seglen >= 7 {
            let h = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            return Some((w, h));
        }
        i += seglen;
    }
    None
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
