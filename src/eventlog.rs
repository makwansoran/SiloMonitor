use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lightweight event log (JSONL) — no native SQLite dependency.
pub struct EventLog {
    path: PathBuf,
    last_level_log: u64,
    interval: u64,
}

impl EventLog {
    pub fn open(path: impl AsRef<Path>, interval_secs: u64) -> Self {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        Self {
            path,
            last_level_log: 0,
            interval: interval_secs.max(1),
        }
    }

    fn append(&self, line: &str) {
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn level(
        &mut self,
        unix: u64,
        pct: Option<f32>,
        y: Option<f32>,
        conf: f32,
        detected: bool,
        force: bool,
    ) {
        if !force && unix.saturating_sub(self.last_level_log) < self.interval {
            return;
        }
        self.last_level_log = unix;
        let pct_s = pct
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "null".into());
        let y_s = y.map(|v| format!("{v:.1}")).unwrap_or_else(|| "null".into());
        self.append(&format!(
            r#"{{"type":"level","ts":{unix},"pct":{pct_s},"y":{y_s},"conf":{conf:.3},"det":{}}}"#,
            detected as u8
        ));
    }

    pub fn drop_ev(&self, unix: u64, conf: f32) {
        self.append(&format!(
            r#"{{"type":"drop","ts":{unix},"conf":{conf:.3}}}"#
        ));
    }

    pub fn state(&self, unix: u64, kind: &str, old: &str, new: &str, msg: &str) {
        let msg = msg.replace('"', "'");
        self.append(&format!(
            r#"{{"type":"state","ts":{unix},"kind":"{kind}","old":"{old}","new":"{new}","msg":"{msg}"}}"#
        ));
    }

    pub fn alarm(&self, unix: u64, level: &str, msg: &str) {
        let msg = msg.replace('"', "'");
        self.append(&format!(
            r#"{{"type":"alarm","ts":{unix},"level":"{level}","msg":"{msg}"}}"#
        ));
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
