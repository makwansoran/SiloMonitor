//! Empty-silo announcement over the SA828.
//!
//! Wiring this module assumes:
//!   PTT (SA828 pin 20) -> Pi GPIO23, header pin 16.
//!   Open-drain style: OUTPUT LOW sinks to key TX; INPUT (High-Z) for idle
//!   so the Pi never drives 3.3 V into the module's pull-up.
//!   Audio: Pi 3.5 mm jack -> series pot -> MIC+/MIC-.
//!   Radio VCC from its own 5 V supply, grounds bonded to the Pi.

use rppal::gpio::{Gpio, OutputPin};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU8, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_AUDIO: &str = "plughw:CARD=Headphones,DEV=0";
const VOICE_REL: &str = "audio/silo_is_empty_vox.wav";
const INSTALL_DIR: &str = "/home/spectr/silo-alert";
/// Safe default: the jack drives a ~10 mV mic input; louder transmits as hiss.
const DEFAULT_PCM_DB: i8 = -28;
/// The module needs time to key before it will modulate.
const TX_LEAD_IN: Duration = Duration::from_millis(800);
const TX_TAIL: Duration = Duration::from_millis(1200);
/// Never hold the channel longer than this, whatever aplay does.
const TX_MAX: Duration = Duration::from_secs(20);
/// Re-announce while the silo stays empty so a busy channel still gets it.
pub const REPEAT_EVERY: Duration = Duration::from_secs(90);

static TX_BUSY: AtomicBool = AtomicBool::new(false);
static PTT_PIN: AtomicU8 = AtomicU8::new(0);
static MUTED: AtomicBool = AtomicBool::new(false);
static PCM_DB: AtomicI8 = AtomicI8::new(DEFAULT_PCM_DB);
static AUDIO_DEVICE: Mutex<String> = Mutex::new(String::new());
static STATUS: Mutex<Option<String>> = Mutex::new(None);

pub fn set_ptt_pin(pin: u8) {
    PTT_PIN.store(pin, Ordering::SeqCst);
}

pub fn set_audio_device(dev: &str) {
    if let Ok(mut g) = AUDIO_DEVICE.lock() {
        *g = dev.trim().to_string();
    }
}

pub fn set_muted(muted: bool) {
    MUTED.store(muted, Ordering::SeqCst);
}

/// PCM playback level in dB for amixer (typical useful range about −40…0).
pub fn set_pcm_db(db: i8) {
    PCM_DB.store(db.clamp(-60, 0), Ordering::SeqCst);
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

/// Idle = High-Z input. Never drive the pin high into the SA828 PTT pull-up.
pub fn release_ptt() {
    let pin = PTT_PIN.load(Ordering::SeqCst);
    if pin == 0 {
        return;
    }
    let Ok(gpio) = Gpio::new() else {
        return;
    };
    let Ok(p) = gpio.get(pin) else {
        return;
    };
    let _ = p.into_input();
}

/// Claim the pin as an open-drain sink (OUTPUT LOW) for the TX window.
fn key_ptt_pin() -> Result<Option<OutputPin>, String> {
    let pin = PTT_PIN.load(Ordering::SeqCst);
    if pin == 0 {
        return Ok(None);
    }
    let mut out = Gpio::new()
        .and_then(|gpio| gpio.get(pin))
        .map(|p| p.into_output_low())
        .map_err(|e| format!("PTT GPIO{pin}: {e}"))?;
    // Drop must not restore a previous level — we go High-Z via release_ptt.
    out.set_reset_on_drop(false);
    Ok(Some(out))
}

/// Keys the transmitter for as long as it is alive.
struct Ptt(Option<OutputPin>);

impl Ptt {
    fn key() -> Result<Self, String> {
        let out = key_ptt_pin()?;
        if out.is_some() {
            thread::sleep(TX_LEAD_IN);
        }
        Ok(Self(out))
    }

    /// False only when a pin is configured but did not actually go low.
    fn is_keyed(&self) -> bool {
        self.0.as_ref().map(|p| p.is_set_low()).unwrap_or(true)
    }
}

impl Drop for Ptt {
    fn drop(&mut self) {
        thread::sleep(TX_TAIL);
        // Drop the output handle first, then float the pin.
        drop(self.0.take());
        release_ptt();
    }
}

pub struct SiloAlert {
    was_empty: bool,
    last_sent: Option<Instant>,
    last_sent_unix: Option<u64>,
}

impl SiloAlert {
    pub fn new() -> Self {
        // Recover the channel if a previous run died mid-transmission.
        release_ptt();
        Self {
            was_empty: false,
            last_sent: None,
            last_sent_unix: None,
        }
    }

    /// Carry empty + last TX across a restart so we do not re-blast the plant.
    pub fn restore(&mut self, was_empty: bool, last_alert_unix: Option<u64>) {
        self.was_empty = was_empty;
        self.last_sent_unix = last_alert_unix;
        self.last_sent = Some(Instant::now());
    }

    pub fn last_alert_unix(&self) -> Option<u64> {
        self.last_sent_unix
    }

    /// Seconds until the next re-announce, if the silo is still empty.
    pub fn next_repeat_in(&self) -> Option<u64> {
        let last = self.last_sent_unix?;
        let now = now_unix();
        let every = REPEAT_EVERY.as_secs();
        Some(every.saturating_sub(now.saturating_sub(last)))
    }

    /// Announce when the silo turns empty, then repeat while it stays empty.
    ///
    /// A restored already-empty state is not "became empty" — only the 90s
    /// wall-clock cooldown may fire again.
    pub fn update(&mut self, empty: bool) -> bool {
        let became_empty = empty && !self.was_empty;
        self.was_empty = empty;
        if !empty {
            return false;
        }
        if MUTED.load(Ordering::SeqCst) {
            return false;
        }
        let due = if became_empty {
            true
        } else if let Some(u) = self.last_sent_unix {
            now_unix().saturating_sub(u) >= REPEAT_EVERY.as_secs()
        } else {
            false
        };
        if !due || !transmit() {
            return false;
        }
        self.last_sent = Some(Instant::now());
        self.last_sent_unix = Some(now_unix());
        true
    }

    /// The Test radio button. Returns immediately; the real outcome arrives
    /// through `take_status` so the window never freezes while keyed.
    /// Test ignores mute so the operator can still prove the radio.
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

fn set_playback_level() {
    let db = PCM_DB.load(Ordering::SeqCst);
    let level = format!("{db}dB");
    let _ = Command::new("/usr/bin/amixer")
        .args(["-c", "Headphones", "sset", "PCM", "--", &level, "unmute"])
        .output();
}

/// Key PTT, play the clip, unkey. Returns the first thing that actually failed.
pub fn play_voice() -> Result<(), String> {
    let wav = voice_file();
    if !wav.is_file() {
        return Err(format!("Missing {}", wav.display()));
    }
    set_playback_level();

    let tx = Ptt::key()?;
    if !tx.is_keyed() {
        return Err("PTT did not go low".into());
    }

    let device = audio_device();
    let mut child = Command::new("/usr/bin/aplay")
        .args(["-D", &device, "-q"])
        .arg(&wav)
        .spawn()
        .map_err(|e| format!("aplay: {e}"))?;

    let deadline = Instant::now() + TX_MAX;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("aplay {status}")),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err("aplay timed out".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("aplay: {e}")),
        }
    }
}
