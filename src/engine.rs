use crate::config::{AlarmCfg, LevelCfg};
use crate::cv::{DropEvent, LevelResult};
use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LevelState {
    Normal,
    Low,
    Critical,
    Empty,
    Unknown,
}

impl LevelState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Low => "LOW",
            Self::Critical => "CRITICAL",
            Self::Empty => "EMPTY",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlarmLevel {
    Ok,
    Warning,
    Critical,
}

impl AlarmLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub level_state: LevelState,
    pub alarm: AlarmLevel,
    pub level_percent: Option<f32>,
    pub level_y: Option<f32>,
    pub level_confidence: f32,
    pub level_detected: bool,
    pub drop_count: u64,
    pub drops_last_1m: u32,
    pub drops_per_min: f32,
    pub last_drop_unix: Option<u64>,
    pub secs_since_drop: Option<u64>,
    pub alarm_message: String,
}

pub struct MonitorEngine {
    level_cfg: LevelCfg,
    alarm_cfg: AlarmCfg,
    pub level_state: LevelState,
    pub alarm: AlarmLevel,
    pub drop_count: u64,
    pub last_drop_unix: Option<u64>,
    drop_ts: VecDeque<u64>,
    empty_since: Option<u64>,
    pub last_message: String,
}

impl MonitorEngine {
    pub fn new(level_cfg: LevelCfg, alarm_cfg: AlarmCfg) -> Self {
        Self {
            level_cfg,
            alarm_cfg,
            level_state: LevelState::Unknown,
            alarm: AlarmLevel::Ok,
            drop_count: 0,
            last_drop_unix: None,
            drop_ts: VecDeque::with_capacity(4096),
            empty_since: None,
            last_message: String::new(),
        }
    }

    pub fn register_drop(&mut self, _ev: DropEvent, unix: u64) {
        self.drop_count += 1;
        self.last_drop_unix = Some(unix);
        if self.drop_ts.len() > 20000 {
            self.drop_ts.pop_front();
        }
        self.drop_ts.push_back(unix);
    }

    fn count_since(&self, now: u64, secs: u64) -> u32 {
        let cut = now.saturating_sub(secs);
        self.drop_ts.iter().filter(|&&t| t >= cut).count() as u32
    }

    pub fn update_level(&mut self, lr: &LevelResult, unix: u64) -> Option<(LevelState, LevelState)> {
        let cfg = &self.level_cfg;
        if !lr.detected || lr.level_percent.is_none() || lr.confidence < cfg.min_confidence {
            self.empty_since = None;
            return self.set_state(LevelState::Unknown);
        }
        let pct = lr.level_percent.unwrap();

        if pct <= cfg.empty_threshold {
            if self.empty_since.is_none() {
                self.empty_since = Some(unix);
            }
        } else {
            self.empty_since = None;
        }

        let empty_ok = self
            .empty_since
            .map(|t| (unix - t) as f32 >= cfg.empty_confirmation_seconds)
            .unwrap_or(false);

        let cur = self.level_state;
        let next = match cur {
            LevelState::Empty => {
                if pct > cfg.empty_exit {
                    band(pct, cfg)
                } else {
                    LevelState::Empty
                }
            }
            LevelState::Critical => {
                if pct <= cfg.empty_threshold && empty_ok {
                    LevelState::Empty
                } else if pct > cfg.critical_exit {
                    if pct >= cfg.low_exit {
                        LevelState::Normal
                    } else {
                        LevelState::Low
                    }
                } else {
                    LevelState::Critical
                }
            }
            LevelState::Low => {
                if pct <= cfg.empty_threshold && empty_ok {
                    LevelState::Empty
                } else if pct <= cfg.critical_threshold {
                    LevelState::Critical
                } else if pct > cfg.low_exit {
                    LevelState::Normal
                } else {
                    LevelState::Low
                }
            }
            LevelState::Normal | LevelState::Unknown => {
                if pct <= cfg.empty_threshold && empty_ok {
                    LevelState::Empty
                } else if pct <= cfg.critical_threshold {
                    LevelState::Critical
                } else if pct < cfg.low_threshold {
                    LevelState::Low
                } else {
                    LevelState::Normal
                }
            }
        };
        self.set_state(next)
    }

    fn set_state(&mut self, new: LevelState) -> Option<(LevelState, LevelState)> {
        if new == self.level_state {
            return None;
        }
        let old = self.level_state;
        self.level_state = new;
        Some((old, new))
    }

    pub fn update_alarm(&mut self, lr: &LevelResult, unix: u64) -> Option<(AlarmLevel, AlarmLevel, String)> {
        let acfg = &self.alarm_cfg;
        let pct = lr.level_percent;
        let since = self.last_drop_unix.map(|t| unix.saturating_sub(t));

        let (new, msg) = if self.level_state == LevelState::Empty {
            (AlarmLevel::Critical, "Silo EMPTY (confirmed)".to_string())
        } else if self.level_state == LevelState::Unknown {
            (AlarmLevel::Ok, "Level UNKNOWN".to_string())
        } else {
            let ignore = pct
                .map(|p| p >= acfg.no_drop_ignore_above_percent)
                .unwrap_or(true);
            let lowish = matches!(self.level_state, LevelState::Low | LevelState::Critical);
            if let Some(s) = since {
                if !ignore && lowish && s as f32 >= acfg.no_drop_critical_minutes * 60.0 {
                    (
                        AlarmLevel::Critical,
                        format!("No drop {:.1}m + level {}", s as f32 / 60.0, self.level_state.as_str()),
                    )
                } else if !ignore && lowish && s as f32 >= acfg.no_drop_warning_minutes * 60.0 {
                    (
                        AlarmLevel::Warning,
                        format!("No drop {:.1}m + low level", s as f32 / 60.0),
                    )
                } else if self.level_state == LevelState::Critical {
                    (
                        AlarmLevel::Critical,
                        format!("Level CRITICAL ({:.1}%)", pct.unwrap_or(0.0)),
                    )
                } else if self.level_state == LevelState::Low {
                    (
                        AlarmLevel::Warning,
                        format!("Level LOW ({:.1}%)", pct.unwrap_or(0.0)),
                    )
                } else {
                    (AlarmLevel::Ok, "OK".to_string())
                }
            } else if self.level_state == LevelState::Critical {
                (
                    AlarmLevel::Critical,
                    format!("Level CRITICAL ({:.1}%)", pct.unwrap_or(0.0)),
                )
            } else if self.level_state == LevelState::Low {
                (
                    AlarmLevel::Warning,
                    format!("Level LOW ({:.1}%)", pct.unwrap_or(0.0)),
                )
            } else {
                (AlarmLevel::Ok, "OK".to_string())
            }
        };

        self.last_message = msg.clone();
        if new != self.alarm {
            let old = self.alarm;
            self.alarm = new;
            Some((old, new, msg))
        } else {
            None
        }
    }

    pub fn snapshot(&self, lr: &LevelResult, unix: u64) -> Snapshot {
        let d1 = self.count_since(unix, 60);
        Snapshot {
            level_state: self.level_state,
            alarm: self.alarm,
            level_percent: lr.level_percent,
            level_y: lr.level_y,
            level_confidence: lr.confidence,
            level_detected: lr.detected,
            drop_count: self.drop_count,
            drops_last_1m: d1,
            drops_per_min: d1 as f32,
            last_drop_unix: self.last_drop_unix,
            secs_since_drop: self.last_drop_unix.map(|t| unix.saturating_sub(t)),
            alarm_message: self.last_message.clone(),
        }
    }
}

fn band(pct: f32, cfg: &LevelCfg) -> LevelState {
    if pct <= cfg.critical_threshold {
        LevelState::Critical
    } else if pct < cfg.low_threshold {
        LevelState::Low
    } else {
        LevelState::Normal
    }
}
