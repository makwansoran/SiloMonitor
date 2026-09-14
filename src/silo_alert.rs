use rppal::gpio::{Gpio, OutputPin};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PTT_GPIO: u8 = 17;
const AUDIO_DEVICE: &str = "plughw:1,0";
const VOICE_FILE: &str = "audio/silo_is_empty.wav";
const COOLDOWN: Duration = Duration::from_secs(30);

pub struct SiloAlert {
    ptt: OutputPin,
    was_empty: bool,
    last_sent: Option<Instant>,
    last_sent_unix: Option<u64>,
}

impl SiloAlert {
    pub fn new() -> Result<Self, rppal::gpio::Error> {
        let mut ptt = Gpio::new()?.get(PTT_GPIO)?.into_output();
        ptt_receive(&mut ptt);
        Ok(Self {
            ptt,
            was_empty: false,
            last_sent: None,
            last_sent_unix: None,
        })
    }

    pub fn restore_last_alert_unix(&mut self, unix: Option<u64>) {
        self.last_sent_unix = unix;
    }

    /// Returns true if an intercom alert was transmitted.
    pub fn update(&mut self, empty: bool) -> bool {
        let became_empty = empty && !self.was_empty;
        self.was_empty = empty;
        if !became_empty {
            return false;
        }
        if let Some(t) = self.last_sent {
            if t.elapsed() < COOLDOWN {
                eprintln!("Alert skipped (cooldown).");
                return false;
            }
        }
        match self.transmit() {
            Ok(()) => println!("Alert sent."),
            Err(e) => {
                eprintln!("Alert failed: {e}");
                return false;
            }
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

    pub fn silo_empty(&self) -> bool {
        self.was_empty
    }

    pub fn secs_since_alert(&self) -> Option<u64> {
        self.last_sent.map(|t| t.elapsed().as_secs())
    }

    pub fn last_alert_unix(&self) -> Option<u64> {
        self.last_sent_unix
    }

    fn transmit(&mut self) -> Result<(), String> {
        ptt_transmit(&mut self.ptt);
        thread::sleep(Duration::from_millis(300));

        let play = Command::new("aplay")
            .args(["-D", AUDIO_DEVICE, VOICE_FILE])
            .status();

        thread::sleep(Duration::from_millis(200));
        ptt_receive(&mut self.ptt);

        match play {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("aplay failed ({s})")),
            Err(e) => Err(format!("aplay failed ({e})")),
        }
    }
}

impl Drop for SiloAlert {
    fn drop(&mut self) {
        ptt_receive(&mut self.ptt);
    }
}

// Swap these two bodies if your SA828 is keyed the opposite way.
fn ptt_transmit(pin: &mut OutputPin) {
    pin.set_low();
}

fn ptt_receive(pin: &mut OutputPin) {
    pin.set_high();
}
