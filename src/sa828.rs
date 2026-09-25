use rppal::uart::{Parity, Queue, Uart};
use std::path::Path;
use std::thread;
use std::time::Duration;

use crate::peltor;

/// Pi 4 hardware UART on GPIO14/15 (pins 8/10). Prefer serial0 over USB stick.
const PORTS: [&str; 5] = [
    "/dev/serial0",
    "/dev/ttyAMA0",
    "/dev/ttyS0",
    "/dev/ttyUSB0",
    "/dev/ttyACM0",
];

/// Accept TX/RX within this many Hz of the requested frequency.
const FREQ_MATCH_HZ: f64 = 100.0;

/// UHF simplex, 4 decimal places (SA828 UART format).
pub fn normalize_freq(raw: &str) -> Result<String, String> {
    let t = raw.trim().replace(',', ".");
    let f: f64 = t.parse().map_err(|_| "Bad frequency".to_string())?;
    if !(400.0..=480.0).contains(&f) {
        return Err("UHF only: 400–480 MHz".into());
    }
    Ok(format!("{f:.4}"))
}

pub fn clamp_squelch(s: u8) -> u8 {
    s.min(8)
}

/// AAFA protocol over the Pi hardware UART (SA828 TXD↔Pi RXD, RXD↔Pi TXD).
///
/// `ctcss` is 0 = Off, 1–38 = standard tone (same on TX and RX).
/// Success requires channel-1 TX and RX to match the requested frequency.
pub fn program(freq: &str, squelch: u8, ctcss: u8, port: &str) -> Result<String, String> {
    let freq = normalize_freq(freq)?;
    let sq = clamp_squelch(squelch);
    let tone = peltor::clamp_ctcss(ctcss);
    let cmd = format!("AAFA3{}\r\n", set_payload(&freq, sq, tone));
    let path = pick_port(port)?;

    let mut uart = open_uart(&path)?;
    chat(&mut uart, b"AAFAA\r\n", 600)?;
    let set = chat(&mut uart, cmd.as_bytes(), 2500)?;
    if set.to_ascii_uppercase().contains("ERROR") {
        return Err(format!("module rejected set: {set}"));
    }
    // Settle before readback — flash write on the module can lag the OK.
    thread::sleep(Duration::from_millis(200));
    let (ver, raw) = read_config(&mut uart)?;
    let (tx, rx) = parse_tx_rx(&raw).ok_or_else(|| {
        format!("program reply unreadable (cannot verify TX/RX): {}", raw.trim())
    })?;
    if !freq_matches(&tx, &freq) || !freq_matches(&rx, &freq) {
        return Err(format!(
            "module did not take new freq (wanted {freq}, got TX {tx} / RX {rx}). \
             Check UART wiring; use Read module."
        ));
    }

    let tone_lbl = peltor::ctcss_label(tone);
    Ok(format!(
        "UART programmed {path}\n  Ch freq {freq} MHz  squelch {sq}  CTCSS {tone_lbl}\n  {ver}\n  channel 1 TX {tx} / RX {rx}"
    ))
}

pub fn read(port: &str) -> Result<String, String> {
    let path = pick_port(port)?;
    let mut uart = open_uart(&path)?;
    let (ver, raw) = read_config(&mut uart)?;
    Ok(format!("UART {path}\n{ver}\n{}", summarize(&raw)))
}

fn pick_port(preferred: &str) -> Result<String, String> {
    let want = preferred.trim();
    if !want.is_empty() && want != "auto" && Path::new(want).exists() {
        return Ok(want.to_string());
    }
    for p in PORTS {
        if Path::new(p).exists() {
            return Ok(p.to_string());
        }
    }
    Err(
        "No UART. Wire SA828 TXD→Pi pin 10, RXD→Pi pin 8. Enable serial0 (disable-bt)."
            .into(),
    )
}

fn set_payload(freq: &str, squelch: u8, ctcss: u8) -> String {
    let mut pairs = Vec::with_capacity(16);
    for _ in 0..16 {
        pairs.push(format!("{freq},{freq}"));
    }
    format!("{},{ctcss:03},{ctcss:03},{squelch}", pairs.join(","))
}

fn open_uart(path: &str) -> Result<Uart, String> {
    let mut uart = Uart::with_path(path, 9600, Parity::None, 8, 1).map_err(|e| e.to_string())?;
    uart.set_read_mode(0, Duration::from_millis(200))
        .map_err(|e| e.to_string())?;
    // Blocking writes — AAFA3 is ~300 bytes; non-blocking can truncate.
    uart.set_write_mode(true).map_err(|e| e.to_string())?;
    let _ = uart.flush(Queue::Both);
    Ok(uart)
}

fn write_all(uart: &mut Uart, cmd: &[u8]) -> Result<(), String> {
    let mut sent = 0;
    while sent < cmd.len() {
        let n = uart.write(&cmd[sent..]).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("UART write stalled".into());
        }
        sent += n;
    }
    uart.flush(Queue::Output).map_err(|e| e.to_string())?;
    Ok(())
}

fn chat(uart: &mut Uart, cmd: &[u8], wait_ms: u64) -> Result<String, String> {
    let _ = uart.flush(Queue::Input);
    write_all(uart, cmd)?;
    thread::sleep(Duration::from_millis(wait_ms));
    let mut buf = [0u8; 512];
    let mut got = Vec::new();
    loop {
        let n = uart.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
        if got.len() >= 500 {
            break;
        }
    }
    let reply = String::from_utf8_lossy(&got).trim().to_string();
    if reply.is_empty() {
        return Err(
            "No reply. Check TXD↔RXD crossed, common GND, enable_uart=1 + disable-bt."
                .into(),
        );
    }
    Ok(reply)
}

fn read_config(uart: &mut Uart) -> Result<(String, String), String> {
    let ver = chat(uart, b"AAFAA\r\n", 600)?;
    let raw = chat(uart, b"AAFA1\r\n", 800)?;
    Ok((ver, raw))
}

fn summarize(raw: &str) -> String {
    match parse_tx_rx(raw) {
        Some((tx, rx)) => {
            let sq = parse_squelch(raw).unwrap_or_else(|| "?".into());
            format!("channel 1 TX {tx} / RX {rx}  squelch {sq}")
        }
        None => raw.trim().to_string(),
    }
}

fn cleaned_aafa1(raw: &str) -> String {
    let line = raw.replace("OK", " ").replace(['\r', '\n'], " ");
    let mut t = line.trim().to_string();
    if t.len() >= 2 && t[..2].eq_ignore_ascii_case("aa") {
        t = t[2..].trim_start().to_string();
    }
    t
}

fn parse_squelch(raw: &str) -> Option<String> {
    let t = cleaned_aafa1(raw);
    let parts: Vec<&str> = t.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    parts.last().map(|s| (*s).to_string())
}

/// Channel-1 TX and RX frequency strings from an AAFA1 reply.
fn parse_tx_rx(raw: &str) -> Option<(String, String)> {
    let t = cleaned_aafa1(raw);
    let parts: Vec<&str> = t.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string()))
}

fn freq_matches(got: &str, want: &str) -> bool {
    let Ok(g) = got.trim().replace(',', ".").parse::<f64>() else {
        return false;
    };
    let Ok(w) = want.trim().replace(',', ".").parse::<f64>() else {
        return false;
    };
    ((g - w).abs() * 1_000_000.0) <= FREQ_MATCH_HZ
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tx_rx_from_aafa1() {
        let raw = "AA446.0062,446.0062,446.0062,446.0062,000,000,1\r\n";
        let (tx, rx) = parse_tx_rx(raw).unwrap();
        assert_eq!(tx, "446.0062");
        assert_eq!(rx, "446.0062");
    }

    #[test]
    fn freq_match_within_100hz() {
        assert!(freq_matches("446.00625", "446.0062"));
        assert!(!freq_matches("446.01875", "446.0062"));
    }
}
