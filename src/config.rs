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

/// Ethernet camera on the plant network (Hikvision NVR or raw RTSP).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CameraCfg {
    #[serde(default = "default_cam_source")]
    pub source: String,
    /// NVR / camera LAN IP or hostname.
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// NVR channel 1–16.
    #[serde(default = "default_nvr_channel")]
    pub channel: u8,
    /// `sub` (…02, default) or `main` (…01).
    #[serde(default = "default_stream")]
    pub stream: String,
    /// When true, use [`rtsp_url`] instead of composing a Hikvision URL.
    #[serde(default)]
    pub use_custom_url: bool,
    /// Advanced / legacy full RTSP URL override.
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
fn default_rtsp_port() -> u16 {
    554
}
fn default_nvr_channel() -> u8 {
    1
}
fn default_stream() -> String {
    "sub".into()
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
            host: String::new(),
            rtsp_port: default_rtsp_port(),
            username: String::new(),
            password: String::new(),
            channel: default_nvr_channel(),
            stream: default_stream(),
            use_custom_url: false,
            rtsp_url: String::new(),
            width: default_cam_w(),
            height: default_cam_h(),
            fps: default_cam_fps(),
        }
    }
}

impl CameraCfg {
    /// Hikvision channel id: channel 3 sub → 302, main → 301.
    pub fn hikvision_channel_id(&self) -> u32 {
        let ch = self.channel.clamp(1, 16) as u32;
        let suffix = if self.stream.eq_ignore_ascii_case("main") {
            1
        } else {
            2
        };
        ch * 100 + suffix
    }

    /// URL used by ffmpeg. Custom/legacy `rtsp_url` wins when enabled or when
    /// host is empty (old configs that only set the full URL).
    pub fn resolved_rtsp_url(&self) -> Result<String, String> {
        let custom = self.rtsp_url.trim();
        if self.use_custom_url {
            if custom.is_empty() {
                return Err("Custom RTSP URL is empty".into());
            }
            return Ok(custom.to_string());
        }
        if self.host.trim().is_empty() {
            if !custom.is_empty() {
                return Ok(custom.to_string());
            }
            return Err("NVR host is empty — set host or a custom RTSP URL".into());
        }
        let user = percent_encode_userinfo(self.username.trim());
        let pass = percent_encode_userinfo(&self.password);
        let host = self.host.trim();
        let port = if self.rtsp_port == 0 {
            554
        } else {
            self.rtsp_port
        };
        let id = self.hikvision_channel_id();
        if user.is_empty() {
            Ok(format!(
                "rtsp://{host}:{port}/Streaming/Channels/{id}"
            ))
        } else {
            Ok(format!(
                "rtsp://{user}:{pass}@{host}:{port}/Streaming/Channels/{id}"
            ))
        }
    }

    /// Composed URL for UI preview (password shown as `***`).
    pub fn preview_rtsp_url(&self) -> String {
        if self.use_custom_url {
            let u = self.rtsp_url.trim();
            return if u.is_empty() {
                "(custom URL empty)".into()
            } else {
                mask_rtsp_password(u)
            };
        }
        if self.host.trim().is_empty() {
            if !self.rtsp_url.trim().is_empty() {
                return mask_rtsp_password(self.rtsp_url.trim());
            }
            return "(set NVR host)".into();
        }
        let mut preview = self.clone();
        if !preview.password.is_empty() {
            preview.password = "***".into();
        }
        preview
            .resolved_rtsp_url()
            .unwrap_or_else(|_| "(invalid)".into())
    }
}

fn percent_encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn mask_rtsp_password(url: &str) -> String {
    // rtsp://user:pass@host → rtsp://user:***@host
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    let Some(at) = rest.find('@') else {
        return url.to_string();
    };
    let creds = &rest[..at];
    let Some(colon) = creds.find(':') else {
        return url.to_string();
    };
    format!(
        "{}{}:***{}",
        &url[..scheme_end + 3],
        &creds[..colon],
        &rest[at..]
    )
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LevelCfg {
    /// The silo must look empty this long before an alert goes out.
    /// The same window applies when clearing sticky-empty (filled confirm).
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
    /// BCM pin wired to SA828 SPKEN (pin 9). High = channel busy. 0 = off.
    #[serde(default)]
    pub spken_gpio: u8,
    #[serde(default = "default_audio_device")]
    pub audio_device: String,
    /// Laptop (no GPIO) defaults muted so a bench run never keys a radio.
    #[serde(default = "default_muted")]
    pub muted: bool,
    /// When true, Test radio plays a volume-test WAV instead of the production clip.
    #[serde(default)]
    pub volume_test_mode: bool,
    /// Filename under `audio/volume_tests/` used when volume_test_mode is on.
    #[serde(default)]
    pub volume_test_file: String,
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
            spken_gpio: 0,
            audio_device: default_audio_device(),
            muted: default_muted(),
            volume_test_mode: false,
            volume_test_file: String::new(),
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
            cam["host"] = self.camera.host.clone().into();
            cam["rtsp_port"] = (self.camera.rtsp_port as i64).into();
            cam["username"] = self.camera.username.clone().into();
            cam["password"] = self.camera.password.clone().into();
            cam["channel"] = (self.camera.channel as i64).into();
            cam["stream"] = self.camera.stream.clone().into();
            cam["use_custom_url"] = serde_yaml::Value::Bool(self.camera.use_custom_url);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hikvision_sub_stream_url() {
        let cam = CameraCfg {
            host: "192.168.1.64".into(),
            username: "viewer".into(),
            password: "secret".into(),
            channel: 3,
            stream: "sub".into(),
            ..CameraCfg::default()
        };
        assert_eq!(cam.hikvision_channel_id(), 302);
        assert_eq!(
            cam.resolved_rtsp_url().unwrap(),
            "rtsp://viewer:secret@192.168.1.64:554/Streaming/Channels/302"
        );
    }

    #[test]
    fn custom_url_override() {
        let cam = CameraCfg {
            use_custom_url: true,
            rtsp_url: "rtsp://x/custom".into(),
            host: "ignored".into(),
            ..CameraCfg::default()
        };
        assert_eq!(cam.resolved_rtsp_url().unwrap(), "rtsp://x/custom");
    }

    #[test]
    fn legacy_full_url_when_host_empty() {
        let cam = CameraCfg {
            rtsp_url: "rtsp://old@host/path".into(),
            ..CameraCfg::default()
        };
        assert_eq!(cam.resolved_rtsp_url().unwrap(), "rtsp://old@host/path");
    }
}
