//! Raspberry Pi SoC readings for the Live sidebar.
//!
//! Reads sysfs and `/proc` every 2 s. `vcgencmd` is only for throttle bits
//! and as a temperature fallback — never on the 10 Hz UI tick.

use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

pub const SAMPLE_EVERY: Duration = Duration::from_secs(2);

const LIVE_UNDERVOLT: u32 = 1 << 0;
const LIVE_FREQ_CAP: u32 = 1 << 1;
const LIVE_THROTTLED: u32 = 1 << 2;
const LIVE_SOFT_TEMP: u32 = 1 << 3;
const LIVE_MASK: u32 = 0xF;

#[derive(Clone, Debug)]
pub struct HostSnapshot {
    pub temp_c: Option<f32>,
    pub cpu: Option<f32>,
    pub ram_total_kb: Option<u64>,
    pub ram_avail_kb: Option<u64>,
    pub throttled: Option<u32>,
}

impl HostSnapshot {
    pub fn ram_used_gb(&self) -> Option<(f32, f32)> {
        let total = self.ram_total_kb?;
        let avail = self.ram_avail_kb?;
        if total == 0 {
            return None;
        }
        let used = total.saturating_sub(avail);
        Some((used as f32 / 1_048_576.0, total as f32 / 1_048_576.0))
    }
}

pub struct HostMonitor {
    last: Option<HostSnapshot>,
    last_at: Option<Instant>,
    cpu_prev: Option<CpuTimes>,
}

#[derive(Clone, Copy)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

impl HostMonitor {
    pub fn new() -> Self {
        let mut m = Self {
            last: None,
            last_at: None,
            cpu_prev: None,
        };
        m.refresh();
        m
    }

    pub fn tick(&mut self) {
        let due = self
            .last_at
            .map(|t| t.elapsed() >= SAMPLE_EVERY)
            .unwrap_or(true);
        if due {
            self.refresh();
        }
    }

    pub fn snapshot(&self) -> Option<&HostSnapshot> {
        self.last.as_ref()
    }

    fn refresh(&mut self) {
        let (cpu, next_times) = cpu_frac(self.cpu_prev);
        self.cpu_prev = next_times.or(self.cpu_prev);
        let (_ram_used, ram_total_kb, ram_avail_kb) = ram();
        self.last = Some(HostSnapshot {
            temp_c: read_temp_c(),
            cpu,
            ram_total_kb,
            ram_avail_kb,
            throttled: read_throttled(),
        });
        self.last_at = Some(Instant::now());
    }
}

pub fn read_throttled() -> Option<u32> {
    let output = Command::new("vcgencmd").arg("get_throttled").output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_throttled(&String::from_utf8_lossy(&output.stdout))
}

fn parse_throttled(text: &str) -> Option<u32> {
    let hex = text.split('=').nth(1)?.trim().trim_start_matches("0x");
    u32::from_str_radix(hex, 16).ok()
}

fn live_bits(bits: u32) -> u32 {
    bits & LIVE_MASK
}

/// Live (right now) power fault, or `OK`.
pub fn live_power_label(bits: u32) -> String {
    let live = live_bits(bits);
    if live == 0 {
        return "OK".into();
    }
    let mut parts = Vec::new();
    if live & LIVE_UNDERVOLT != 0 {
        parts.push("Undervoltage");
    }
    if live & LIVE_FREQ_CAP != 0 {
        parts.push("Freq capped");
    }
    if live & LIVE_SOFT_TEMP != 0 {
        parts.push("Thermal");
    } else if live & LIVE_THROTTLED != 0 {
        parts.push("Throttled");
    }
    if parts.is_empty() {
        "Throttled".into()
    } else {
        parts.join(" · ")
    }
}

fn read_temp_c() -> Option<f32> {
    if let Ok(s) = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        if let Ok(milli) = s.trim().parse::<f32>() {
            return Some(milli / 1000.0);
        }
    }
    let output = Command::new("vcgencmd").arg("measure_temp").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let num = text.split('=').nth(1)?.trim().trim_end_matches("'C");
    num.parse().ok()
}

fn cpu_frac(prev: Option<CpuTimes>) -> (Option<f32>, Option<CpuTimes>) {
    let Some(now) = read_cpu_times() else {
        return (None, prev);
    };
    let frac = prev.and_then(|p| {
        let dt = now.total.saturating_sub(p.total);
        if dt == 0 {
            return None;
        }
        let di = now.idle.saturating_sub(p.idle);
        Some((1.0 - (di as f32 / dt as f32)).clamp(0.0, 1.0))
    });
    (frac, Some(now))
}

fn read_cpu_times() -> Option<CpuTimes> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next() != Some("cpu") {
        return None;
    }
    let mut vals = [0u64; 8];
    for v in vals.iter_mut() {
        *v = parts.next()?.parse().ok()?;
    }
    // user nice system idle iowait irq softirq steal
    let idle = vals[3].saturating_add(vals[4]);
    let total = vals.iter().sum();
    Some(CpuTimes { idle, total })
}

fn ram() -> (Option<f32>, Option<u64>, Option<u64>) {
    let Ok(text) = fs::read_to_string("/proc/meminfo") else {
        return (None, None, None);
    };
    let mut total = None;
    let mut avail = None;
    for line in text.lines() {
        if let Some(v) = meminfo_kb(line, "MemTotal:") {
            total = Some(v);
        } else if let Some(v) = meminfo_kb(line, "MemAvailable:") {
            avail = Some(v);
        }
        if total.is_some() && avail.is_some() {
            break;
        }
    }
    let used = match (total, avail) {
        (Some(t), Some(a)) if t > 0 => Some(1.0 - (a as f32 / t as f32)),
        _ => None,
    };
    (used, total, avail)
}

fn meminfo_kb(line: &str, key: &str) -> Option<u64> {
    let rest = line.strip_prefix(key)?.trim();
    rest.split_whitespace().next()?.parse().ok()
}
