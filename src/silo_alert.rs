//! Empty-silo announcement over the SA828.
//!
//! Wiring this module assumes:
//!   PTT (SA828 pin 20) -> Pi GPIO17, header pin 11. Low = transmit.
//!   Audio: Pi 3.5 mm jack -> series pot -> MIC+/MIC-.
//!   Radio VCC from its own 5 V supply, grounds bonded to the Pi.

use rppal::gpio::{Gpio, OutputPin};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const AUDIO_DEVICE: &str = "plughw:CARD=Headphones,DEV=0";
const VOICE_REL: &str = "audio/silo_is_empty_vox.wav";
const INSTALL_DIR: &str = "/home/spectr/silo-alert";
/// The jack drives a ~10 mV mic input; louder than this transmits as hiss.
const PCM_LEVEL: &str = "-28dB";
/// The module needs time to key before it will modulate.
const TX_LEAD_IN: Duration = Duration::from_millis(800);
const TX_TAIL: Duration = Duration::from_millis(1200);
/// Never hold the channel longer than this, whatever aplay does.
const TX_MAX: Duration = Duration::from_secs(20);
/// Re-announce while the silo stays empty so a busy channel still gets it.
const REPEAT_EVERY: Duration = Duration::from_secs(90);

static TX_BUSY: AtomicBool = AtomicBool::new(false);
static PTT_PIN: AtomicU8 = AtomicU8::new(0);
static STATUS: Mutex<Option<String>> = Mutex::new(None);

pub fn set_ptt_pin(pin: u8) {
    PTT_PIN.store(pin, Ordering::SeqCst);
}

/// Result of the last transmission, consumed once by the UI.
pub fn take_status() -> Option<String> {
    STATUS.lock().ok().and_then(|mut s| s.take())
}

fn set_status(msg: String) {
    if let Ok(mut s) = STATUS.lock() {
        *s = Some(msg);
    }
}

fn open_ptt() -> Result<Option<OutputPin>, String> {
    let pin = PTT_PIN.load(Ordering::SeqCst);
    if pin == 0 {
        return Ok(None);
    }
    let mut out = Gpio::new()
        .and_then(|gpio| gpio.get(pin))
        .map(|p| p.into_output_high())
        .map_err(|e| format!("PTT GPIO{pin}: {e}"))?;
    // Reset-on-drop restores whatever level the pin had before we claimed it.
    // After an interrupted transmission that is low, which would re-key the
    // radio the moment this handle drops, so release it explicitly instead.
    out.set_reset_on_drop(false);
    Ok(Some(out))
}

/// Undo a transmission that was interrupted before PTT could be released.
pub fn release_ptt() {
    if let Ok(Some(mut out)) = open_ptt() {
        out.set_high();
    }
}

/// Keys the transmitter for as long as it is alive.
struct Ptt(Option<OutputPin>);

impl Ptt {
    fn key() -> Result<Self, String> {
        let mut out = open_ptt()?;
        if let Some(p) = out.as_mut() {
            p.set_low();
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
        if let Some(out) = self.0.as_mut() {
            thread::sleep(TX_TAIL);
            out.set_high();
        }
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

    pub fn restore_last_alert_unix(&mut self, unix: Option<u64>) {
        self.last_sent_unix = unix;
    }

    pub fn last_alert_unix(&self) -> Option<u64> {
        self.last_sent_unix
    }

    /// Announce when the silo turns empty, then repeat while it stays empty.
    pub fn update(&mut self, empty: bool) -> bool {
        let became_empty = empty && !self.was_empty;
        self.was_empty = empty;
        if !empty {
            return false;
        }
        let due = became_empty
            || self
                .last_sent
                .map(|t| t.elapsed() >= REPEAT_EVERY)
                .unwrap_or(true);
        if !due || !transmit() {
            return false;
        }
        self.last_sent = Some(Instant::now());
        self.last_sent_unix = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        true
    }

    /// The Test radio button. Returns immediately; the real outcome arrives
    /// through `take_status` so the window never freezes while keyed.
    pub fn test_transmit(&mut self) -> Result<(), String> {
        if transmit() {
            Ok(())
        } else {
            Err("Already transmitting".into())
        }
    }
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
        // target/release/silo-alert -> project root
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
    // Absolute path: the desktop session starts the app with a bare PATH.
    let _ = Command::new("/usr/bin/amixer")
        .args(["-c", "Headphones", "sset", "PCM", "--", PCM_LEVEL, "unmute"])
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

    let mut child = Command::new("/usr/bin/aplay")
        .args(["-D", AUDIO_DEVICE, "-q"])
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
