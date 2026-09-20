//! Classical CV: level surface + drop motion (no ML / OpenCV).

use crate::config::{DropCfg, LevelCfg, RoiCfg};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug)]
pub struct LevelResult {
    pub level_y: Option<f32>,
    pub level_percent: Option<f32>,
    pub confidence: f32,
    pub detected: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct DropEvent {
    pub confidence: f32,
}

pub struct LevelDetector {
    roi: RoiCfg,
    full_at_top: bool,
    cfg: LevelCfg,
    ys: VecDeque<f32>,
    ema: Option<f32>,
}

impl LevelDetector {
    pub fn new(roi: RoiCfg, full_at_top: bool, cfg: LevelCfg) -> Self {
        let cap = cfg.smoothing_window.max(3);
        Self {
            roi,
            full_at_top,
            cfg,
            ys: VecDeque::with_capacity(cap),
            ema: None,
        }
    }

    pub fn set_roi(&mut self, roi: RoiCfg) {
        self.roi = roi;
        self.ys.clear();
        self.ema = None;
    }

    fn y_to_percent(&self, level_y: f32) -> f32 {
        let top = self.roi.y1 as f32;
        let bot = self.roi.y2 as f32;
        let span = (bot - top).max(1.0);
        let t = ((level_y - top) / span).clamp(0.0, 1.0);
        if self.full_at_top {
            100.0 * (1.0 - t)
        } else {
            100.0 * t
        }
    }

    fn detect_raw(&self, w: u32, h: u32, rgb: &[u8]) -> (Option<f32>, f32) {
        let (x1, y1, x2, y2) = self.roi.clamp(w, h);
        let rw = (x2 - x1) as usize;
        let rh = (y2 - y1) as usize;
        if rw < 3 || rh < 3 {
            return (None, 0.0);
        }

        // Grayscale + light horizontal box blur into row energy via Sobel-Y
        let mut gray = vec![0.0f32; rw * rh];
        for row in 0..rh {
            for col in 0..rw {
                let sx = (x1 as usize + col) as u32;
                let sy = (y1 as usize + row) as u32;
                let i = ((sy * w + sx) * 3) as usize;
                let r = rgb[i] as f32;
                let g = rgb[i + 1] as f32;
                let b = rgb[i + 2] as f32;
                gray[row * rw + col] = 0.299 * r + 0.587 * g + 0.114 * b;
            }
        }

        let mut energy = vec![0.0f32; rh];
        for row in 1..rh - 1 {
            let mut sum = 0.0f32;
            for col in 0..rw {
                let up = gray[(row - 1) * rw + col];
                let dn = gray[(row + 1) * rw + col];
                sum += (dn - up).abs();
            }
            energy[row] = sum / rw as f32;
        }

        // Smooth energy along rows
        let mut sm = energy.clone();
        let k = 3usize;
        for i in 0..rh {
            let a = i.saturating_sub(k);
            let b = (i + k + 1).min(rh);
            let slice = &energy[a..b];
            sm[i] = slice.iter().sum::<f32>() / slice.len() as f32;
        }

        let (peak_i, peak) = sm
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, v)| (i, *v))
            .unwrap_or((0, 0.0));
        let mean_e = sm.iter().sum::<f32>() / sm.len() as f32 + 1e-6;
        if peak < self.cfg.sobel_peak_min {
            return (None, 0.0);
        }
        let strength = ((peak - mean_e) / (peak + mean_e)).clamp(0.0, 1.0);
        let frac = peak_i as f32 / (rh - 1).max(1) as f32;
        let edge_pen = if frac < 0.05 || frac > 0.95 { 0.6 } else { 1.0 };
        let conf = (strength * edge_pen * (peak / (self.cfg.sobel_peak_min * 3.0)).min(1.0))
            .clamp(0.0, 1.0);
        let level_y = y1 as f32 + peak_i as f32;
        (Some(level_y), conf)
    }

    pub fn update(&mut self, w: u32, h: u32, rgb: &[u8]) -> LevelResult {
        let (raw_y, raw_conf) = self.detect_raw(w, h, rgb);
        let cap = self.cfg.smoothing_window.max(3);
        if let Some(y) = raw_y {
            if raw_conf >= self.cfg.min_confidence * 0.4 {
                if self.ys.len() == cap {
                    self.ys.pop_front();
                }
                self.ys.push_back(y);
            }
        }

        let need = (self.cfg.smoothing_window / 3).max(3);
        if self.ys.len() < need {
            return LevelResult {
                level_y: None,
                level_percent: None,
                confidence: raw_conf,
                detected: false,
            };
        }

        let mut sorted: Vec<f32> = self.ys.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = sorted[sorted.len() / 2];
        let ema = match self.ema {
            None => {
                self.ema = Some(med);
                med
            }
            Some(prev) => {
                let a = self.cfg.ema_alpha;
                let v = a * med + (1.0 - a) * prev;
                self.ema = Some(v);
                v
            }
        };

        let mean = self.ys.iter().sum::<f32>() / self.ys.len() as f32;
        let var = self.ys.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / self.ys.len() as f32;
        let std = var.sqrt();
        let roi_h = (self.roi.y2 - self.roi.y1).max(1) as f32;
        let stab = (1.0 - std / (8.0f32).max(roi_h * 0.15)).clamp(0.0, 1.0);
        let conf = (0.5 * raw_conf + 0.5 * stab).clamp(0.0, 1.0);
        let pct = self.y_to_percent(ema);
        let detected = conf >= self.cfg.min_confidence;
        LevelResult {
            level_y: Some(ema),
            level_percent: Some(pct),
            confidence: conf,
            detected,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DropState {
    Waiting,
    Detected,
    Cooldown,
}

pub struct DropDetector {
    roi: RoiCfg,
    cfg: DropCfg,
    state: DropState,
    prev_gray: Option<Vec<u8>>,
    prev_w: u32,
    prev_h: u32,
    cooldown_until_ms: u64,
    motion_streak: u32,
    last_cent_y: Option<f32>,
    pub last_box: Option<(u32, u32, u32, u32)>, // x,y,w,h absolute
}

impl DropDetector {
    pub fn new(roi: RoiCfg, cfg: DropCfg) -> Self {
        Self {
            roi,
            cfg,
            state: DropState::Waiting,
            prev_gray: None,
            prev_w: 0,
            prev_h: 0,
            cooldown_until_ms: 0,
            motion_streak: 0,
            last_cent_y: None,
            last_box: None,
        }
    }

    pub fn set_roi(&mut self, roi: RoiCfg) {
        self.roi = roi;
        self.state = DropState::Waiting;
        self.prev_gray = None;
        self.prev_w = 0;
        self.prev_h = 0;
        self.motion_streak = 0;
        self.last_cent_y = None;
        self.last_box = None;
    }

    pub fn state_name(&self) -> &'static str {
        match self.state {
            DropState::Waiting => "WAITING",
            DropState::Detected => "DETECTED",
            DropState::Cooldown => "COOLDOWN",
        }
    }

    fn crop_gray(&self, w: u32, h: u32, rgb: &[u8]) -> (u32, u32, u32, u32, Vec<u8>) {
        let (x1, y1, x2, y2) = self.roi.clamp(w, h);
        let rw = x2 - x1;
        let rh = y2 - y1;
        let mut gray = vec![0u8; (rw * rh) as usize];
        for row in 0..rh {
            for col in 0..rw {
                let i = (((y1 + row) * w + (x1 + col)) * 3) as usize;
                let y = (0.299 * rgb[i] as f32
                    + 0.587 * rgb[i + 1] as f32
                    + 0.114 * rgb[i + 2] as f32) as u8;
                gray[(row * rw + col) as usize] = y;
            }
        }
        (x1, y1, rw, rh, gray)
    }

    fn best_blob(&self, mask: &[u8], rw: u32, rh: u32) -> Option<(u32, u32, u32, u32, f32, f32)> {
        // Connected components via simple flood fill on binary mask
        let mut seen = vec![false; mask.len()];
        let mut best: Option<(u32, u32, u32, u32, f32, f32, f32)> = None; // x,y,w,h,area,cy,score

        for y0 in 0..rh {
            for x0 in 0..rw {
                let idx = (y0 * rw + x0) as usize;
                if mask[idx] == 0 || seen[idx] {
                    continue;
                }
                let mut stack = vec![(x0, y0)];
                seen[idx] = true;
                let mut min_x = x0;
                let mut max_x = x0;
                let mut min_y = y0;
                let mut max_y = y0;
                let mut area = 0u32;
                let mut sum_y = 0u64;
                while let Some((x, y)) = stack.pop() {
                    area += 1;
                    sum_y += y as u64;
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                    for (nx, ny) in [
                        (x.wrapping_sub(1), y),
                        (x + 1, y),
                        (x, y.wrapping_sub(1)),
                        (x, y + 1),
                    ] {
                        if nx >= rw || ny >= rh {
                            continue;
                        }
                        let ni = (ny * rw + nx) as usize;
                        if mask[ni] != 0 && !seen[ni] {
                            seen[ni] = true;
                            stack.push((nx, ny));
                        }
                    }
                }
                let area_f = area as f32;
                if area_f < self.cfg.min_area || area_f > self.cfg.max_area {
                    continue;
                }
                let bw = (max_x - min_x + 1) as f32;
                let bh = (max_y - min_y + 1) as f32;
                let aspect = bw / bh.max(1.0);
                if aspect > 4.0 || aspect < 0.15 {
                    continue;
                }
                let extent = area_f / (bw * bh).max(1.0);
                let score = area_f * (0.4 + 0.6 * extent);
                let cy = sum_y as f32 / area_f;
                let conf = (0.55 * extent + 0.45 * (area_f / (self.cfg.min_area * 8.0)).min(1.0))
                    .clamp(0.0, 1.0);
                let cand = (min_x, min_y, max_x - min_x + 1, max_y - min_y + 1, conf, cy, score);
                if best.as_ref().map(|b| b.6).unwrap_or(0.0) < score {
                    best = Some(cand);
                }
            }
        }
        best.map(|(x, y, w, h, conf, cy, _)| (x, y, w, h, conf, cy))
    }

    pub fn update(&mut self, w: u32, h: u32, rgb: &[u8], now_ms: u64) -> Option<DropEvent> {
        self.last_box = None;
        let (x1, y1, rw, rh, gray) = self.crop_gray(w, h, rgb);
        if rw < 2 || rh < 2 {
            return None;
        }

        let mut mask = vec![0u8; gray.len()];
        if let Some(prev) = &self.prev_gray {
            if prev.len() == gray.len() && self.prev_w == rw && self.prev_h == rh {
                for i in 0..gray.len() {
                    let d = gray[i].abs_diff(prev[i]);
                    if d >= self.cfg.diff_threshold {
                        mask[i] = 255;
                    }
                }
            }
        }
        self.prev_gray = Some(gray);
        self.prev_w = rw;
        self.prev_h = rh;

        // Tiny morph open: remove isolated pixels
        let mut cleaned = mask.clone();
        for y in 1..rh as usize - 1 {
            for x in 1..rw as usize - 1 {
                let i = y * rw as usize + x;
                if mask[i] == 0 {
                    continue;
                }
                let n = [
                    mask[i - 1],
                    mask[i + 1],
                    mask[i - rw as usize],
                    mask[i + rw as usize],
                ]
                .iter()
                .filter(|&&v| v != 0)
                .count();
                if n < 2 {
                    cleaned[i] = 0;
                }
            }
        }

        let blob = self.best_blob(&cleaned, rw, rh);
        let (moving, conf, cy) = if let Some((bx, by, bw, bh, conf, cy)) = blob {
            self.last_box = Some((x1 + bx, y1 + by, bw, bh));
            (conf >= self.cfg.min_confidence * 0.5, conf, Some(cy))
        } else {
            (false, 0.0, None)
        };

        let mut downward = true;
        if let (Some(cy), Some(prev)) = (cy, self.last_cent_y) {
            downward = cy >= prev - 1.0;
        }
        if let Some(cy) = cy {
            self.last_cent_y = Some(cy);
        }

        match self.state {
            DropState::Cooldown => {
                if now_ms >= self.cooldown_until_ms && !moving {
                    self.state = DropState::Waiting;
                    self.motion_streak = 0;
                }
                None
            }
            DropState::Waiting => {
                if moving && downward && conf >= self.cfg.min_confidence * 0.5 {
                    self.motion_streak += 1;
                    if self.motion_streak >= 2 && conf >= self.cfg.min_confidence {
                        self.state = DropState::Cooldown;
                        self.cooldown_until_ms = now_ms + self.cfg.cooldown_ms;
                        self.motion_streak = 0;
                        return Some(DropEvent { confidence: conf });
                    }
                    self.state = DropState::Detected;
                } else {
                    self.motion_streak = 0;
                }
                None
            }
            DropState::Detected => {
                if moving && downward && conf >= self.cfg.min_confidence {
                    self.state = DropState::Cooldown;
                    self.cooldown_until_ms = now_ms + self.cfg.cooldown_ms;
                    self.motion_streak = 0;
                    Some(DropEvent { confidence: conf })
                } else if !moving {
                    self.state = DropState::Waiting;
                    self.motion_streak = 0;
                    None
                } else {
                    None
                }
            }
        }
    }
}
