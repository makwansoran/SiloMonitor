use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub level_roi: RoiCfg,
    pub drop_roi: RoiCfg,
    #[serde(default = "default_true")]
    pub level_full_at_top: bool,
    pub level: LevelCfg,
    pub drop: DropCfg,
    pub alarm: AlarmCfg,
    #[serde(default)]
    pub storage: StorageCfg,
    /// Path the config was loaded from (not serialized).
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct RoiCfg {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl RoiCfg {
    pub fn from_drag(ax: i32, ay: i32, bx: i32, by: i32) -> Option<Self> {
        let x1 = ax.min(bx);
        let y1 = ay.min(by);
        let x2 = ax.max(bx);
        let y2 = ay.max(by);
        if x2 - x1 < 4 || y2 - y1 < 4 {
            return None;
        }
        Some(Self { x1, y1, x2, y2 })
    }

    pub fn clamp(self, w: u32, h: u32) -> (u32, u32, u32, u32) {
        let w = w as i32;
        let h = h as i32;
        let x1 = self.x1.clamp(0, w.saturating_sub(1));
        let y1 = self.y1.clamp(0, h.saturating_sub(1));
        let x2 = self.x2.clamp(x1 + 1, w);
        let y2 = self.y2.clamp(y1 + 1, h);
        (x1 as u32, y1 as u32, x2 as u32, y2 as u32)
    }

    pub fn clamp_i32(self, w: u32, h: u32) -> Self {
        let (x1, y1, x2, y2) = self.clamp(w, h);
        Self {
            x1: x1 as i32,
            y1: y1 as i32,
            x2: x2 as i32,
            y2: y2 as i32,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LevelCfg {
    #[serde(default = "d5")]
    pub empty_threshold: f32,
    #[serde(default = "d20")]
    pub critical_threshold: f32,
    #[serde(default = "d40")]
    pub low_threshold: f32,
    #[serde(default = "d42")]
    pub low_exit: f32,
    #[serde(default = "d22")]
    pub critical_exit: f32,
    #[serde(default = "d8")]
    pub empty_exit: f32,
    #[serde(default = "d15")]
    pub smoothing_window: usize,
    #[serde(default = "d025")]
    pub ema_alpha: f32,
    #[serde(default = "d06")]
    pub min_confidence: f32,
    #[serde(default = "d10")]
    pub empty_confirmation_seconds: f32,
    #[serde(default = "d8f")]
    pub sobel_peak_min: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DropCfg {
    #[serde(default = "d500")]
    pub cooldown_ms: u64,
    #[serde(default = "d15f")]
    pub min_area: f32,
    #[serde(default = "d5000")]
    pub max_area: f32,
    #[serde(default = "d05")]
    pub min_confidence: f32,
    #[serde(default = "d25")]
    pub diff_threshold: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlarmCfg {
    #[serde(default = "d5f")]
    pub no_drop_warning_minutes: f32,
    #[serde(default = "d10f")]
    pub no_drop_critical_minutes: f32,
    #[serde(default = "d60")]
    pub no_drop_ignore_above_percent: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageCfg {
    #[serde(default = "default_db")]
    pub db_path: String,
    #[serde(default = "d5u")]
    pub level_log_interval_seconds: u64,
}

impl Default for StorageCfg {
    fn default() -> Self {
        Self {
            db_path: default_db(),
            level_log_interval_seconds: 5,
        }
    }
}

fn d5() -> f32 { 5.0 }
fn d8() -> f32 { 8.0 }
fn d8f() -> f32 { 8.0 }
fn d10() -> f32 { 10.0 }
fn d10f() -> f32 { 10.0 }
fn d15() -> usize { 15 }
fn d15f() -> f32 { 15.0 }
fn d20() -> f32 { 20.0 }
fn d22() -> f32 { 22.0 }
fn d25() -> u8 { 25 }
fn d40() -> f32 { 40.0 }
fn d42() -> f32 { 42.0 }
fn d60() -> f32 { 60.0 }
fn d025() -> f32 { 0.25 }
fn d05() -> f32 { 0.5 }
fn d06() -> f32 { 0.6 }
fn d5f() -> f32 { 5.0 }
fn d5u() -> u64 { 5 }
fn d500() -> u64 { 500 }
fn d5000() -> f32 { 5000.0 }
fn default_db() -> String { "data/silo.db".into() }

impl Config {
    pub fn load() -> Self {
        let candidates = [
            PathBuf::from("config.yaml"),
            PathBuf::from("py/config.yaml"),
            PathBuf::from("/home/spectr/silo-alert/config.yaml"),
        ];
        for p in candidates {
            if let Ok(s) = fs::read_to_string(&p) {
                if let Ok(mut c) = serde_yaml::from_str::<Config>(&s) {
                    eprintln!("config: {}", p.display());
                    c.source_path = Some(p);
                    return c;
                }
            }
        }
        eprintln!("config.yaml missing — using built-in defaults");
        Self::default_builtin()
    }

    fn default_builtin() -> Self {
        let mut c: Config = serde_yaml::from_str(include_str!("../config.yaml")).unwrap_or_else(|_| Config {
            level_roi: RoiCfg { x1: 100, y1: 50, x2: 540, y2: 450 },
            drop_roi: RoiCfg { x1: 280, y1: 40, x2: 380, y2: 160 },
            level_full_at_top: true,
            level: LevelCfg {
                empty_threshold: 5.0,
                critical_threshold: 20.0,
                low_threshold: 40.0,
                low_exit: 42.0,
                critical_exit: 22.0,
                empty_exit: 8.0,
                smoothing_window: 15,
                ema_alpha: 0.25,
                min_confidence: 0.6,
                empty_confirmation_seconds: 10.0,
                sobel_peak_min: 8.0,
            },
            drop: DropCfg {
                cooldown_ms: 500,
                min_area: 15.0,
                max_area: 5000.0,
                min_confidence: 0.5,
                diff_threshold: 25,
            },
            alarm: AlarmCfg {
                no_drop_warning_minutes: 5.0,
                no_drop_critical_minutes: 10.0,
                no_drop_ignore_above_percent: 60.0,
            },
            storage: StorageCfg::default(),
            source_path: None,
        });
        c.source_path = Some(PathBuf::from("/home/spectr/silo-alert/config.yaml"));
        c
    }

    /// Update only ROI keys in the on-disk YAML (keeps other settings).
    pub fn save_rois(&self) -> Result<(), String> {
        let path = self
            .source_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("/home/spectr/silo-alert/config.yaml"));
        let text = fs::read_to_string(&path).unwrap_or_else(|_| include_str!("../config.yaml").to_string());
        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(&text).map_err(|e| format!("parse config: {e}"))?;
        doc["level_roi"] = serde_yaml::to_value(self.level_roi)
            .map_err(|e| format!("level_roi: {e}"))?;
        doc["drop_roi"] =
            serde_yaml::to_value(self.drop_roi).map_err(|e| format!("drop_roi: {e}"))?;
        let out = serde_yaml::to_string(&doc).map_err(|e| format!("serialize: {e}"))?;
        fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(())
    }

    pub fn resolve_db_path(&self) -> PathBuf {
        let p = Path::new(&self.storage.db_path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            PathBuf::from("data").join(p.file_name().unwrap_or_default())
        }
    }
}
