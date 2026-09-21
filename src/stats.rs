use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATS_PATH: &str = "data/stats.json";

#[derive(Clone, Serialize, Deserialize)]
pub struct Stats {
    pub checks: u64,
    pub empty_hits: u64,
    pub alerts_sent: u64,
    pub labels_empty: u64,
    pub labels_full: u64,
    pub last_train_unix: Option<u64>,
    pub last_train_accuracy: Option<f32>,
    pub last_check_unix: Option<u64>,
    pub last_check_empty: Option<bool>,
    pub last_alert_unix: Option<u64>,
    pub started_unix: u64,
    /// Confirmed-empty state, so a restart does not re-run the confirm delay.
    #[serde(default)]
    pub empty_state: bool,
    /// When the silo first looked empty, for the same reason.
    #[serde(default)]
    pub empty_since_unix: Option<u64>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            checks: 0,
            empty_hits: 0,
            alerts_sent: 0,
            labels_empty: 0,
            labels_full: 0,
            last_train_unix: None,
            last_train_accuracy: None,
            last_check_unix: None,
            last_check_empty: None,
            last_alert_unix: None,
            started_unix: now_unix(),
            empty_state: false,
            empty_since_unix: None,
        }
    }
}

impl Stats {
    pub fn load() -> Self {
        fs::read_to_string(STATS_PATH)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(parent) = Path::new(STATS_PATH).parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = fs::write(STATS_PATH, s);
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
