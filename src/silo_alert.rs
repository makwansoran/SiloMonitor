use rppal::gpio::{Gpio, OutputPin};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const AUDIO_DEVICE: &str = "plughw:CARD=Headphones,DEV=0";
const BEEP_REL: &str = "audio/vox_beep.wav";
const VOICE_REL: &str = "audio/silo_is_empty_vox.wav";
/// Re-transmit while the silo stays empty so a busy channel still gets the words.
const REPEAT_EVERY: Duration = Duration::from_secs(90);
const BURST_PLAYS: u32 = 2;
const BURST_GAP: Duration = Duration::from_millis(400);
/// SA828 needs time to come up before it will modulate.
const TX_DELAY: Duration = Duration::from_millis(500);
const TX_TAIL: Duration = Duration::from_millis(250);

static TX_BUSY: AtomicBool = AtomicBool::new(false);
/// 0 disables GPIO keying and falls back to VOX.
static PTT_PIN: AtomicU8 = AtomicU8::new(0);

pub fn set_ptt_pin(pin: u8) {
    PTT_PIN.store(pin, Ordering::SeqCst);
}

/// Holds PTT low for as long as it is alive. SA828 pin 20: low = TX.
struct Ptt(Option<OutputPin>);

impl Ptt {
    fn key() -> Self {
        let pin = PTT_PIN.load(Ordering::SeqCst);
        if pin == 0 {
            return Self(None);
        }
        match Gpio::new().and_then(|g| g.get(pin)) {
            Ok(p) => {
                let mut p = p.into_output_high();
                // Default reset-on-drop would pull PTT low and key TX forever.
                p.set_reset_on_drop(false);
                p.set_low();
                thread::sleep(TX_DELAY);
                Self(Some(p))
            }
            Err(e) => {
                eprintln!("PTT GPIO {pin} unavailable ({e}); relying on VOX");
                Self(None)
            }
        }
    }
}

impl Drop for Ptt {
    fn drop(&mut self) {
        if let Some(p) = self.0.as_mut() {
            thread::sleep(TX_TAIL);
            p.set_high();
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

    /// Auto-TX on the analog channel. No UI button. Repeats while empty.
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
        if !due {
            return false;
        }
        if !start_channel_burst() {
            return false;
        }
        println!("Empty silo: transmitting on channel.");
        self.last_sent = Some(Instant::now());
        self.last_sent_unix = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        true
    }

    pub fn silo_empty(&self) -> bool {
        self.was_empty
    }

    pub fn secs_since_alert(&self) -> Option<u64> {
        self.last_sent.map(|t| t.elapsed().as_secs())
    }

    pub fn last_alert_unix(&self) -> Option<u64> {
        self.last_sent_unix
    }

    /// Manual check only. Live empty uses `update` and does not need this.
    pub fn test_transmit(&mut self) -> Result<(), String> {
        if start_channel_burst() {
            Ok(())
        } else {
            Err("Already transmitting".into())
        }
    }
}

fn start_channel_burst() -> bool {
    if TX_BUSY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    thread::spawn(|| {
        for i in 0..BURST_PLAYS {
            if let Err(e) = play_voice() {
                eprintln!("Alert failed: {e}");
                break;
            }
            if i + 1 < BURST_PLAYS {
                thread::sleep(BURST_GAP);
            }
        }
        TX_BUSY.store(false, Ordering::SeqCst);
    });
    true
}

fn set_pcm(level: &str) {
    for card in ["Headphones", "2", "0", "1"] {
        if Command::new("amixer")
            .args(["-c", card, "sset", "PCM", "--", level, "unmute"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }
    }
}

fn audio_file(rel: &str) -> PathBuf {
    let mut paths = vec![PathBuf::from(rel)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            paths.push(root.join(rel));
        }
    }
    paths.push(PathBuf::from("/home/spectr/silo-alert").join(rel));
    paths
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(rel))
}

fn aplay(wav: &PathBuf) -> Result<(), String> {
    let play = Command::new("aplay")
        .args(["-D", AUDIO_DEVICE, "-q"])
        .arg(wav)
        .output();
    match play {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Err(format!("aplay failed ({}) {err}", o.status))
        }
        Err(e) => Err(format!("aplay failed ({e})")),
    }
}

/// Pot on MIC+ is the attenuator, so the jack stays hot.
pub fn play_voice() -> Result<(), String> {
    let beep = audio_file(BEEP_REL);
    let voice = audio_file(VOICE_REL);
    if !voice.is_file() {
        return Err(format!("Missing {VOICE_REL}"));
    }
    set_pcm("0dB");
    let _tx = Ptt::key();
    if beep.is_file() {
        aplay(&beep)?;
    }
    aplay(&voice)
}
