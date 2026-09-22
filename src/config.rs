use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub camera: CameraCfg,
    #[serde(default)]
    pub level: LevelCfg,
    #[serde(default)]
    pub storage: StorageCfg,
    #[serde(default)]
    pub radio: RadioCfg,
    #[serde(default)]
    pub supabase: SupabaseCfg,
    /// Path the config was loaded from (not serialized).
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

/// Ethernet camera on the plant network.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CameraCfg {
    #[serde(default = "default_cam_source")]
    pub source: String,
    #[serde(default)]
    pub rtsp_url: String,
    #[serde(default = "default_cam_w")]
    pub width: u32,
    #[serde(default = "default_cam_h")]
    pub height: u32,
    /// Decode rate. Low is fine: the match only runs each check interval.
    #[serde(default = "default_cam_fps")]
    pub fps: u32,
}

fn default_cam_source() -> String {
    "rtsp".into()
}
fn default_cam_w() -> u32 {
    640
}
fn default_cam_h() -> u32 {
    480
}
fn default_cam_fps() -> u32 {
    2
}

impl Default for CameraCfg {
    fn default() -> Self {
        Self {
            source: default_cam_source(),
            rtsp_url: String::new(),
            width: default_cam_w(),
            height: default_cam_h(),
            fps: default_cam_fps(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LevelCfg {
    /// The silo must look empty this long before an alert goes out.
    #[serde(default = "d15f")]
    pub empty_confirmation_seconds: f32,
    /// One comparison every N seconds.
    #[serde(default = "d15f")]
    pub check_interval_seconds: f32,
    /// How closely a frame must match the empty reference to count as empty.
    /// 0 means "derive it from how alike the reference photos are".
    #[serde(default)]
    pub empty_match_threshold: f32,
}

impl Default for LevelCfg {
    fn default() -> Self {
        Self {
            empty_confirmation_seconds: d15f(),
            check_interval_seconds: d15f(),
            empty_match_threshold: 0.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageCfg {
    #[serde(default = "default_db")]
    pub db_path: String,
}

impl Default for StorageCfg {
    fn default() -> Self {
        Self {
            db_path: default_db(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadioCfg {
    #[serde(default = "default_freq")]
    pub frequency_mhz: String,
    /// Peltor LiteCom Pro III analog channel 1–16. Drives frequency_mhz.
    #[serde(default = "default_channel")]
    pub channel: u8,
    #[serde(default = "default_squelch")]
    pub squelch: u8,
    /// CTCSS: 0 = Off, 1–38 = standard tone (same TX/RX on SA828).
    #[serde(default)]
    pub ctcss: u8,
    /// Pi hardware UART: /dev/serial0 (GPIO14 TX / GPIO15 RX).
    #[serde(default = "default_uart_port", alias = "usb_port")]
    pub uart_port: String,
    /// BCM pin wired to SA828 PTT (pin 20). 0 = disable GPIO PTT.
    #[serde(default = "default_ptt_gpio")]
    pub ptt_gpio: u8,
    #[serde(default = "default_audio_device")]
    pub audio_device: String,
    /// Laptop (no GPIO) defaults muted so a bench run never keys a radio.
    #[serde(default = "default_muted")]
    pub muted: bool,
}

impl Default for RadioCfg {
    fn default() -> Self {
        Self {
            frequency_mhz: default_freq(),
            channel: default_channel(),
            squelch: default_squelch(),
            ctcss: 0,
            uart_port: default_uart_port(),
            ptt_gpio: default_ptt_gpio(),
            audio_device: default_audio_device(),
            muted: default_muted(),
        }
    }
}

fn default_ptt_gpio() -> u8 {
    // BCM 23 = header pin 16. Open-drain: LOW = TX, High-Z idle (no 3.3 V).
    23
}

fn default_freq() -> String {
    "446.0062".into()
}

fn default_channel() -> u8 {
    1
}

fn default_squelch() -> u8 {
    1
}

fn default_uart_port() -> String {
    "/dev/serial0".into()
}

fn default_audio_device() -> String {
    "plughw:CARD=Headphones,DEV=0".into()
}

fn default_muted() -> bool {
    !Path::new("/dev/gpiomem").exists() && !Path::new("/dev/gpiochip0").exists()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SupabaseCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    /// Publishable / anon key. Prefer env SUPABASE_KEY so it is not committed.
    #[serde(default)]
    pub key: String,
    #[serde(default = "default_site")]
    pub site_id: String,
}

impl Default for SupabaseCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            key: String::new(),
            site_id: default_site(),
        }
    }
}

fn default_site() -> String {
    "spectr-pi".into()
}

fn d15f() -> f32 {
    15.0
}
fn default_db() -> String {
    "data/silo.db".into()
}

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
        let mut c: Config = serde_yaml::from_str(include_str!("../config.yaml")).unwrap_or_else(
            |_| Config {
                camera: CameraCfg::default(),
                level: LevelCfg::default(),
                storage: StorageCfg::default(),
                radio: RadioCfg::default(),
                supabase: SupabaseCfg::default(),
                source_path: None,
            },
        );
        c.source_path = Some(PathBuf::from("/home/spectr/silo-alert/config.yaml"));
        c
    }

    /// Write the settings the app owns back into config.yaml, leaving any
    /// other keys in the file untouched.
    pub fn save(&self) -> Result<(), String> {
        let path = self
            .source_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("config.yaml"));
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|_| include_str!("../config.yaml").to_string());
        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(&text).map_err(|e| format!("parse config: {e}"))?;

        if let Some(level) = doc.get_mut("level") {
            level["empty_confirmation_seconds"] = self.level.empty_confirmation_seconds.into();
            level["check_interval_seconds"] = self.level.check_interval_seconds.into();
            level["empty_match_threshold"] = self.level.empty_match_threshold.into();
        } else {
            doc["level"] = serde_yaml::to_value(&self.level).unwrap_or(serde_yaml::Value::Null);
        }
        doc["radio"] = serde_yaml::to_value(&self.radio).unwrap_or(serde_yaml::Value::Null);
        if let Some(cam) = doc.get_mut("camera") {
            cam["source"] = self.camera.source.clone().into();
            cam["rtsp_url"] = self.camera.rtsp_url.clone().into();
            if let Some(map) = cam.as_mapping_mut() {
                map.remove(serde_yaml::Value::from("device"));
            }
            cam["width"] = self.camera.width.into();
            cam["height"] = self.camera.height.into();
            cam["fps"] = self.camera.fps.into();
        } else {
            doc["camera"] = serde_yaml::to_value(&self.camera).unwrap_or(serde_yaml::Value::Null);
        }
        if let Some(sb) = doc.get_mut("supabase") {
            sb["enabled"] = serde_yaml::Value::Bool(self.supabase.enabled);
            sb["site_id"] = self.supabase.site_id.clone().into();
        }

        let out = serde_yaml::to_string(&doc).map_err(|e| format!("serialize: {e}"))?;
        fs::write(&path, out).map_err(|e| format!("write config: {e}"))?;
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
