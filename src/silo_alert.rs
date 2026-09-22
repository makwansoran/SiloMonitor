//! Empty-silo announcement over the SA828.
//!
//! Wiring:
//!   PTT (SA828 pin 20) -> Pi GPIO23, header pin 16.
//!   Open-drain: OUTPUT LOW = TX (sink); idle = INPUT High-Z (Bias::Off).
//!   The Pi must never drive 3.3 V into the SA828 PTT line — the module's
//!   own pull-up holds idle inactive. Do not use OUTPUT HIGH or pull-up bias.
//!   Audio: Pi 3.5 mm jack -> series pot -> MIC+/MIC-.
//!   Radio VCC from its own 5 V supply, grounds bonded to the Pi.

use rppal::gpio::{Gpio, OutputPin};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_AUDIO: &str = "plughw:CARD=Headphones,DEV=0";
const VOICE_REL: &str = "audio/silo_is_empty_vox.wav";
const INSTALL_DIR: &str = "/home/spectr/silo-alert";
/// The module needs time to key before it will modulate.
const TX_LEAD_IN: Duration = Duration::from_millis(800);
/// Brief hang so the last syllable clears the channel — then PTT goes High-Z.
const TX_TAIL: Duration = Duration::from_millis(150);
/// Never hold the channel longer than this, whatever aplay does.
const TX_MAX: Duration = Duration::from_secs(20);

static TX_BUSY: AtomicBool = AtomicBool::new(false);
static PTT_PIN: AtomicU8 = AtomicU8::new(0);
static MUTED: AtomicBool = AtomicBool::new(false);
static AUDIO_DEVICE: Mutex<String> = Mutex::new(String::new());
static STATUS: Mutex<Option<String>> = Mutex::new(None);

pub fn set_ptt_pin(pin: u8) {
    PTT_PIN.store(pin, Ordering::SeqCst);
    ensure_ptt_idle();
}

pub fn set_audio_device(dev: &str) {
    if let Ok(mut g) = AUDIO_DEVICE.lock() {
        *g = dev.trim().to_string();
    }
}

pub fn set_muted(muted: bool) {
    MUTED.store(muted, Ordering::SeqCst);
}

fn audio_device() -> String {
    AUDIO_DEVICE
        .lock()
        .ok()
        .map(|g| g.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_AUDIO.to_string())
}

/// Result of the last transmission, consumed once by the UI.
pub fn take_status() -> Option<String> {
    STATUS.lock().ok().and_then(|mut s| s.take())
}

pub fn status_ok(msg: &str) -> bool {
    msg.starts_with("Played")
}

fn set_status(msg: String) {
    if let Ok(mut s) = STATUS.lock() {
        *s = Some(msg);
    }
}

/// INPUT + Bias::Off = true High-Z. Never OUTPUT HIGH / pull-up (that is 3.3 V).
fn float_ptt_pin(pin: u8) {
    if pin == 0 {
        return;
    }
    let Ok(gpio) = Gpio::new() else {
        return;
    };
    let Ok(p) = gpio.get(pin) else {
        return;
    };
    let mut inp = p.into_input();
    // Keep High-Z after drop — default reset would restore prior OUTPUT LOW.
    inp.set_reset_on_drop(false);
}

/// Idle = High-Z on the configured PTT pin only (never touch GPIO17).
pub fn ensure_ptt_idle() {
    float_ptt_pin(PTT_PIN.load(Ordering::SeqCst));
}

pub fn release_ptt() {
    ensure_ptt_idle();
}

/// OUTPUT LOW = sink / key TX for the announcement window.
fn key_ptt_pin() -> Result<Option<OutputPin>, String> {
    let pin = PTT_PIN.load(Ordering::SeqCst);
    if pin == 0 {
        return Ok(None);
    }
    let mut out = Gpio::new()
        .and_then(|gpio| gpio.get(pin))
        .map(|p| p.into_output_low())
        .map_err(|e| format!("PTT GPIO{pin}: {e}"))?;
    // Drop must not restore a level — we go High-Z via release_ptt.
    out.set_reset_on_drop(false);
    Ok(Some(out))
}

/// Keys the transmitter while alive; Drop returns the pin to High-Z (no 3.3 V).
struct Ptt(Option<OutputPin>);

impl Ptt {
    fn key() -> Result<Self, String> {
        let out = key_ptt_pin()?;
        if out.is_some() {
            thread::sleep(TX_LEAD_IN);
        }
        Ok(Self(out))
    }

    fn is_keyed(&self) -> bool {
        self.0.as_ref().map(|p| p.is_set_low()).unwrap_or(true)
    }
}

impl Drop for Ptt {
    fn drop(&mut self) {
        thread::sleep(TX_TAIL);
        // Do not set_high() — that drives 3.3 V into SA828 PTT.
        drop(self.0.take());
        release_ptt();
    }
}

pub struct SiloAlert {
    /// Sticky for this empty episode. Cleared only when vision sees full again.
    announced: bool,
    last_sent: Option<Instant>,
    last_sent_unix: Option<u64>,
}

impl SiloAlert {
    pub fn new() -> Self {
        ensure_ptt_idle();
        Self {
            announced: false,
            last_sent: None,
            last_sent_unix: None,
        }
    }

    /// Carry empty + last TX across a restart so we do not re-blast the plant.
    pub fn restore(&mut self, was_empty: bool, last_alert_unix: Option<u64>) {
        self.announced = was_empty;
        self.last_sent_unix = last_alert_unix;
        self.last_sent = Some(Instant::now());
    }

    pub fn last_alert_unix(&self) -> Option<u64> {
        self.last_sent_unix
    }

    /// Play the clip once for a freshly confirmed empty.
    pub fn on_empty_confirmed(&mut self) -> bool {
        if self.announced {
            return false;
        }
        if MUTED.load(Ordering::SeqCst) {
            self.announced = true;
            return false;
        }
        if !transmit() {
            return false;
        }
        self.announced = true;
        self.last_sent = Some(Instant::now());
        self.last_sent_unix = Some(now_unix());
        true
    }

    /// Vision confirmed the silo is full again — allow one TX on the next empty.
    pub fn on_filled(&mut self) {
        self.announced = false;
    }

    /// Test radio: PTT + WAV once, then idle. Works when disarmed.
    pub fn test_transmit(&mut self) -> Result<(), String> {
        if transmit() {
            Ok(())
        } else {
            Err("Already transmitting".into())
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Transmit on a worker thread. False if one is already running.
fn transmit() -> bool {
    if TX_BUSY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    thread::spawn(|| {
        let msg = match play_voice() {
            Ok(()) => "Played silo is empty".to_string(),
            Err(e) => {
                eprintln!("radio: {e}");
                e
            }
        };
        set_status(msg);
        TX_BUSY.store(false, Ordering::SeqCst);
    });
    true
}

fn voice_file() -> PathBuf {
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(VOICE_REL));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.ancestors().nth(3) {
            candidates.push(root.join(VOICE_REL));
        }
    }
    candidates.push(PathBuf::from(INSTALL_DIR).join(VOICE_REL));
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(INSTALL_DIR).join(VOICE_REL))
}

/// Key PTT → play WAV once at system volume → High-Z idle.
/// Does not touch ALSA/PCM levels — operator sets volume on the Pi.
pub fn play_voice() -> Result<(), String> {
    let wav = voice_file();
    if !wav.is_file() {
        return Err(format!("Missing {}", wav.display()));
    }

    let result = play_voice_keyed(&wav);
    ensure_ptt_idle();
    result
}

fn play_voice_keyed(wav: &PathBuf) -> Result<(), String> {
    let tx = Ptt::key()?;
    if !tx.is_keyed() {
        return Err("PTT did not go low".into());
    }

    let device = audio_device();
    let mut child = Command::new("/usr/bin/aplay")
        .args(["-D", &device, "-q"])
        .arg(wav)
        .spawn()
        .map_err(|e| format!("aplay: {e}"))?;

    let deadline = Instant::now() + TX_MAX;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => break Err(format!("aplay {status}")),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break Err("aplay timed out".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => break Err(format!("aplay: {e}")),
        }
    }
}
