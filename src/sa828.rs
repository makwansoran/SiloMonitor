use rppal::uart::{Parity, Queue, Uart};
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Pi 4 hardware UART on GPIO14/15 (pins 8/10). Prefer serial0 over USB stick.
const PORTS: [&str; 5] = [
    "/dev/serial0",
    "/dev/ttyAMA0",
    "/dev/ttyS0",
    "/dev/ttyUSB0",
    "/dev/ttyACM0",
];

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
pub fn program(freq: &str, squelch: u8, port: &str) -> Result<String, String> {
    let freq = normalize_freq(freq)?;
    let sq = clamp_squelch(squelch);
    let cmd = format!("AAFA3{}\r\n", set_payload(&freq, sq));
    let path = pick_port(port)?;
    chat(&path, b"AAFAA\r\n", 600)?;
    let set = chat(&path, cmd.as_bytes(), 2500)?;
    if set.to_ascii_uppercase().contains("ERROR") {
        return Err(format!("module rejected set: {set}"));
    }
    let info = read_on(&path)?;
    Ok(format!(
        "UART programmed {path}\n  {freq} MHz  squelch {sq}\n  {info}"
    ))
}

pub fn read(port: &str) -> Result<String, String> {
    let path = pick_port(port)?;
    let info = read_on(&path)?;
    Ok(format!("UART {path}\n{info}"))
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

fn set_payload(freq: &str, squelch: u8) -> String {
    let mut pairs = Vec::with_capacity(16);
    for _ in 0..16 {
        pairs.push(format!("{freq},{freq}"));
    }
    format!("{},000,000,{squelch}", pairs.join(","))
}

fn open_uart(path: &str) -> Result<Uart, String> {
    let mut uart = Uart::with_path(path, 9600, Parity::None, 8, 1).map_err(|e| e.to_string())?;
    uart.set_read_mode(0, Duration::from_millis(200))
        .map_err(|e| e.to_string())?;
    let _ = uart.flush(Queue::Both);
    Ok(uart)
}

fn chat(path: &str, cmd: &[u8], wait_ms: u64) -> Result<String, String> {
    let mut uart = open_uart(path)?;
    uart.write(cmd).map_err(|e| e.to_string())?;
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

fn read_on(path: &str) -> Result<String, String> {
    let ver = chat(path, b"AAFAA\r\n", 600)?;
    let raw = chat(path, b"AAFA1\r\n", 800)?;
    Ok(format!("{ver}\n{}", summarize(&raw)))
}

fn summarize(raw: &str) -> String {
    let line = raw.replace("OK", " ").replace(['\r', '\n'], " ");
    let mut t = line.trim();
    if t.len() >= 2 && t[..2].eq_ignore_ascii_case("aa") {
        t = t[2..].trim_start();
    }
    let parts: Vec<&str> = t.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return raw.trim().to_string();
    }
    let sq = parts.last().copied().unwrap_or("?");
    format!("channel 1 TX {} / RX {}  squelch {sq}", parts[0], parts[1])
}
