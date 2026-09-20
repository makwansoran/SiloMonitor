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
/// Re-announce while the silo stays empty so a busy channel still gets it.
const REPEAT_EVERY: Duration = Duration::from_secs(90);

static TX_BUSY: AtomicBool = AtomicBool::new(false);
static PTT_PIN: AtomicU8 = AtomicU8::new(0);

pub fn set_ptt_pin(pin: u8) {
    PTT_PIN.store(pin, Ordering::SeqCst);
}

/// Keys the transmitter for as long as it is alive.
struct Ptt(Option<OutputPin>);

impl Ptt {
    fn key() -> Result<Self, String> {
        let pin = PTT_PIN.load(Ordering::SeqCst);
        if pin == 0 {
            return Ok(Self(None));
        }
        let mut out = Gpio::new()
            .and_then(|gpio| gpio.get(pin))
            .map(|p| p.into_output_high())
            .map_err(|e| format!("PTT GPIO{pin}: {e}"))?;
        // Releasing the pin must not leave it low, which would key forever.
        out.set_reset_on_drop(false);
        out.set_low();
        thread::sleep(TX_LEAD_IN);
        Ok(Self(Some(out)))
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
        if !due || !transmit_in_background() {
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

    /// The Test radio button. Blocks so the UI can report the real result.
    pub fn test_transmit(&mut self) -> Result<(), String> {
        if TX_BUSY
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("Already transmitting".into());
        }
        let result = play_voice();
        TX_BUSY.store(false, Ordering::SeqCst);
        result
    }
}

fn transmit_in_background() -> bool {
    if TX_BUSY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    thread::spawn(|| {
        if let Err(e) = play_voice() {
            eprintln!("radio: {e}");
        }
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
        .status();
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

    let played = Command::new("/usr/bin/aplay")
        .args(["-D", AUDIO_DEVICE, "-q"])
        .arg(&wav)
        .output()
        .map_err(|e| format!("aplay: {e}"))?;
    if !played.status.success() {
        return Err(format!(
            "aplay {}: {}",
            played.status,
            String::from_utf8_lossy(&played.stderr).trim()
        ));
    }
    Ok(())
}
