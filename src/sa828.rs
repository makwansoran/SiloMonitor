use rppal::uart::{Parity, Queue, Uart};
use std::path::Path;
use std::thread;
use std::time::Duration;

use crate::peltor;

/// GPIO14/15 (header pins 8/10). On Pi 5 that device is ttyAMA0, not serial0.
const PORTS: [&str; 4] = ["/dev/ttyAMA0", "/dev/ttyS0", "/dev/ttyUSB0", "/dev/ttyACM0"];

/// SA828 stores 100 Hz coarser than the 4-decimal command (446.0312 comes back
/// as 446.0311). A real channel step is 12.5 kHz, so 250 Hz still rejects
/// the wrong Peltor channel.
const FREQ_MATCH_HZ: f64 = 250.0;

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

pub fn program_plan(squelch: u8, ctcss: u8, port: &str) -> Result<String, String> {
    let freqs: Vec<String> = (1..=16).map(peltor::sa828_freq_for_channel).collect();
    let sq = clamp_squelch(squelch);
    let tone = peltor::clamp_ctcss(ctcss);
    let mut pairs = Vec::with_capacity(16);
    for freq in &freqs {
        let freq = normalize_freq(freq)?;
        pairs.push(format!("{freq},{freq}"));
    }
    let cmd = format!(
        "AAFA3{},{tone:03},{tone:03},{sq}\r\n",
        pairs.join(",")
    );
    let path = pick_port(port)?;
    let mut uart = open_uart(&path)?;
    chat(&mut uart, b"AAFAA\r\n", 600)?;
    let set = chat(&mut uart, cmd.as_bytes(), 2500)?;
    if set.to_ascii_uppercase().contains("ERROR") {
        return Err(format!("module rejected set: {set}"));
    }
    thread::sleep(Duration::from_millis(200));
    let (ver, raw) = read_config(&mut uart)?;
    let (tx, rx) = parse_tx_rx(&raw).ok_or_else(|| {
        format!("program reply unreadable (cannot verify TX/RX): {}", raw.trim())
    })?;
    let want = normalize_freq(&freqs[0])?;
    if !freq_matches(&tx, &want) || !freq_matches(&rx, &want) {
        return Err(format!(
            "module did not take channel plan (ch1 wanted {want}, got TX {tx} / RX {rx})"
        ));
    }
    Ok(format!(
        "UART programmed {path}\n  16 Peltor channels  squelch {sq}\n  {ver}\n  plan slot 1 TX {tx} / RX {rx} (live ch via GPIO pins)"
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
    if !want.is_empty() && want != "auto" && Path::new(want).exists() && !is_pi5_debug_uart(want)
    {
        return Ok(want.to_string());
    }
    for p in PORTS {
        if Path::new(p).exists() && !is_pi5_debug_uart(p) {
            return Ok(p.to_string());
        }
    }
    Err(
        "No GPIO UART. On a Pi 5 add dtoverlay=uart0-pi5 and reboot. SA828 TXD→pin 10, RXD→pin 8."
            .into(),
    )
}

/// Pi 5 `/dev/serial0` is the debug header (ttyAMA10), not GPIO 14/15.
fn is_pi5_debug_uart(path: &str) -> bool {
    if Path::new(path)
        .file_name()
        .is_some_and(|n| n == "ttyAMA10")
    {
        return true;
    }
    std::fs::read_link(path)
        .ok()
        .is_some_and(|t| t.to_string_lossy().contains("ttyAMA10"))
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
    // rppal flush() discards the queue (tcflush). drain() waits until the
    // bytes have left the Pi (tcdrain). AAFA3 is ~300 bytes at 9600 baud;
    // discarding here chops the command and the module keeps the old channel.
    uart.drain().map_err(|e| e.to_string())?;
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
            format!("plan slot 1 TX {tx} / RX {rx}  squelch {sq}")
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
    // Round so a true 100 Hz gap is not 100.00000003 and rejected.
    ((g - w).abs() * 1_000_000.0).round() <= FREQ_MATCH_HZ
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
    fn freq_match_accepts_module_100hz_step() {
        assert!(freq_matches("446.00625", "446.0062"));
        assert!(freq_matches("446.0311", "446.0312"));
        assert!(!freq_matches("446.01875", "446.0062"));
    }
}
