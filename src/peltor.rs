//! Peltor WS LiteCom Pro III (MT73H7A4D10EU) — analog PMR446 channel map.
//!
//! The headset also has 16 digital DMR channels on the same frequencies.
//! The SA828 is analog FM only, so this module exposes the analog table.

/// Analog PMR446 channels 1–16 (MHz, 12.5 kHz spacing).
pub const ANALOG_CHANNELS: [(u8, f64); 16] = [
    (1, 446.00625),
    (2, 446.01875),
    (3, 446.03125),
    (4, 446.04375),
    (5, 446.05625),
    (6, 446.06875),
    (7, 446.08125),
    (8, 446.09375),
    (9, 446.10625),
    (10, 446.11875),
    (11, 446.13125),
    (12, 446.14375),
    (13, 446.15625),
    (14, 446.16875),
    (15, 446.18125),
    (16, 446.19375),
];

/// Standard CTCSS tones. Index 1–38 matches SA828 AAFA subaudio codes.
/// Index 0 = Off (no tone).
pub const CTCSS_TONES: [(u8, f32); 38] = [
    (1, 67.0),
    (2, 71.9),
    (3, 74.4),
    (4, 77.0),
    (5, 79.7),
    (6, 82.5),
    (7, 85.4),
    (8, 88.5),
    (9, 91.5),
    (10, 94.8),
    (11, 97.4),
    (12, 100.0),
    (13, 103.5),
    (14, 107.2),
    (15, 110.9),
    (16, 114.8),
    (17, 118.8),
    (18, 123.0),
    (19, 127.3),
    (20, 131.8),
    (21, 136.5),
    (22, 141.3),
    (23, 146.2),
    (24, 151.4),
    (25, 156.7),
    (26, 162.2),
    (27, 167.9),
    (28, 173.8),
    (29, 179.9),
    (30, 186.2),
    (31, 192.8),
    (32, 203.5),
    (33, 210.7),
    (34, 218.1),
    (35, 225.7),
    (36, 233.6),
    (37, 241.8),
    (38, 250.3),
];

pub fn clamp_channel(ch: u8) -> u8 {
    ch.clamp(1, 16)
}

pub fn clamp_ctcss(idx: u8) -> u8 {
    idx.min(38)
}

/// Exact table frequency for a channel (1–16).
pub fn freq_for_channel(ch: u8) -> f64 {
    let ch = clamp_channel(ch);
    ANALOG_CHANNELS[(ch - 1) as usize].1
}

/// SA828 UART string (4 decimal places) for a channel.
pub fn sa828_freq_for_channel(ch: u8) -> String {
    format!("{:.4}", freq_for_channel(ch))
}

/// Nearest Peltor channel for a stored MHz string.
pub fn channel_for_freq(mhz: &str) -> u8 {
    let t = mhz.trim().replace(',', ".");
    let Ok(f) = t.parse::<f64>() else {
        return 1;
    };
    let mut best = 1u8;
    let mut best_d = f64::MAX;
    for &(ch, freq) in &ANALOG_CHANNELS {
        let d = (freq - f).abs();
        if d < best_d {
            best_d = d;
            best = ch;
        }
    }
    best
}

pub fn channel_label(ch: u8) -> String {
    let ch = clamp_channel(ch);
    format!("Ch {ch} — {:.5} MHz", freq_for_channel(ch))
}

pub fn ctcss_label(idx: u8) -> String {
    let idx = clamp_ctcss(idx);
    if idx == 0 {
        return "Off".into();
    }
    let hz = CTCSS_TONES[(idx - 1) as usize].1;
    format!("{idx} — {hz:.1} Hz")
}
