"""Classical OpenCV level + drop detection (no ML)."""

from __future__ import annotations

import logging
import time
from collections import deque
from dataclasses import dataclass
from typing import Any, Deque, Dict, Optional, Tuple

import cv2
import numpy as np

log = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# ROI helpers
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class ROI:
    x1: int
    y1: int
    x2: int
    y2: int

    def clamp(self, w: int, h: int) -> "ROI":
        x1 = max(0, min(self.x1, w - 1))
        y1 = max(0, min(self.y1, h - 1))
        x2 = max(x1 + 1, min(self.x2, w))
        y2 = max(y1 + 1, min(self.y2, h))
        return ROI(x1, y1, x2, y2)

    def crop(self, frame: np.ndarray) -> np.ndarray:
        r = self.clamp(frame.shape[1], frame.shape[0])
        return frame[r.y1 : r.y2, r.x1 : r.x2]

    @property
    def height(self) -> int:
        return max(1, self.y2 - self.y1)

    @property
    def width(self) -> int:
        return max(1, self.x2 - self.x1)

    def as_dict(self) -> Dict[str, int]:
        return {"x1": self.x1, "y1": self.y1, "x2": self.x2, "y2": self.y2}


def roi_from_cfg(d: Dict[str, Any]) -> ROI:
    return ROI(int(d["x1"]), int(d["y1"]), int(d["x2"]), int(d["y2"]))


def blur_gray(bgr: np.ndarray, ksize: int = 5) -> np.ndarray:
    k = ksize if ksize % 2 == 1 else ksize + 1
    gray = cv2.cvtColor(bgr, cv2.COLOR_BGR2GRAY)
    return cv2.GaussianBlur(gray, (k, k), 0)


# ---------------------------------------------------------------------------
# Level
# ---------------------------------------------------------------------------

@dataclass
class LevelResult:
    level_y: Optional[float]
    level_percent: Optional[float]
    confidence: float
    detected: bool
    timestamp: float


class LevelDetector:
    """Find granule surface Y in LEVEL ROI via Sobel-Y row energy (+ optional adaptive)."""

    def __init__(self, cfg: Dict[str, Any], level_roi: ROI, full_at_top: bool = True) -> None:
        self.cfg = cfg
        self.roi = level_roi
        self.full_at_top = full_at_top
        self.window = int(cfg.get("smoothing_window", 15))
        self.ema_alpha = float(cfg.get("ema_alpha", 0.25))
        self.min_conf = float(cfg.get("min_confidence", 0.6))
        self.blur_ksize = int(cfg.get("blur_ksize", 5))
        self.peak_min = float(cfg.get("sobel_peak_min", 8.0))
        self.method = str(cfg.get("method", "sobel")).lower()

        self._ys: Deque[float] = deque(maxlen=max(3, self.window))
        self._ema: Optional[float] = None

    def reset(self) -> None:
        self._ys.clear()
        self._ema = None

    def _y_to_percent(self, level_y: float) -> float:
        """level_y is absolute frame Y."""
        top, bot = float(self.roi.y1), float(self.roi.y2)
        span = max(1.0, bot - top)
        t = (level_y - top) / span  # 0 at top, 1 at bottom
        t = float(np.clip(t, 0.0, 1.0))
        if self.full_at_top:
            return 100.0 * (1.0 - t)
        return 100.0 * t

    def _detect_raw(self, frame: np.ndarray) -> Tuple[Optional[float], float]:
        """Return (absolute level_y, raw_confidence 0..1)."""
        roi = self.roi.clamp(frame.shape[1], frame.shape[0])
        crop = frame[roi.y1 : roi.y2, roi.x1 : roi.x2]
        if crop.size == 0:
            return None, 0.0

        gray = blur_gray(crop, self.blur_ksize)

        if self.method == "adaptive":
            return self._detect_adaptive(gray, roi)
        return self._detect_sobel(gray, roi)

    def _detect_sobel(self, gray: np.ndarray, roi: ROI) -> Tuple[Optional[float], float]:
        sobel = cv2.Sobel(gray, cv2.CV_32F, 0, 1, ksize=3)
        energy = np.abs(sobel).mean(axis=1)
        if energy.size < 3:
            return None, 0.0

        # Light smooth across rows to ignore pits / dust spikes
        k = min(7, energy.size | 1)
        if k >= 3:
            energy = cv2.GaussianBlur(energy.reshape(-1, 1), (1, k), 0).ravel()

        peak_i = int(np.argmax(energy))
        peak = float(energy[peak_i])
        mean_e = float(energy.mean()) + 1e-6
        if peak < self.peak_min:
            return None, 0.0

        # Relative peak strength
        strength = float(np.clip((peak - mean_e) / (peak + mean_e), 0.0, 1.0))
        # Prefer mid-band peaks slightly (edges of ROI often noisy)
        edge_pen = 1.0
        frac = peak_i / max(1, len(energy) - 1)
        if frac < 0.05 or frac > 0.95:
            edge_pen = 0.6

        conf = float(np.clip(strength * edge_pen * min(1.0, peak / (self.peak_min * 3)), 0.0, 1.0))
        level_y = float(roi.y1 + peak_i)
        return level_y, conf

    def _detect_adaptive(self, gray: np.ndarray, roi: ROI) -> Tuple[Optional[float], float]:
        thr = cv2.adaptiveThreshold(
            gray, 255, cv2.ADAPTIVE_THRESH_GAUSSIAN_C, cv2.THRESH_BINARY, 21, 5
        )
        kernel = cv2.getStructuringElement(cv2.MORPH_RECT, (5, 5))
        thr = cv2.morphologyEx(thr, cv2.MORPH_CLOSE, kernel, iterations=2)
        # Assume material is darker or denser near bottom — find top-most filled row band
        row_fill = (thr < 128).mean(axis=1)  # fraction "dark"
        # Surface = first row from top where fill rises above threshold and stays
        surface = None
        for i, f in enumerate(row_fill):
            if f > 0.35:
                surface = i
                break
        if surface is None:
            # try inverted polarity
            row_fill = (thr >= 128).mean(axis=1)
            for i, f in enumerate(row_fill):
                if f > 0.35:
                    surface = i
                    break
        if surface is None:
            return None, 0.0
        conf = float(np.clip(row_fill[surface], 0.3, 1.0))
        return float(roi.y1 + surface), conf

    def update(self, frame: np.ndarray, timestamp: Optional[float] = None) -> LevelResult:
        ts = time.time() if timestamp is None else timestamp
        raw_y, raw_conf = self._detect_raw(frame)

        if raw_y is not None and raw_conf >= self.min_conf * 0.4:
            self._ys.append(raw_y)

        if len(self._ys) < max(3, self.window // 3):
            return LevelResult(None, None, raw_conf, False, ts)

        med = float(np.median(np.fromiter(self._ys, dtype=np.float64)))
        if self._ema is None:
            self._ema = med
        else:
            a = self.ema_alpha
            self._ema = a * med + (1.0 - a) * self._ema

        level_y = float(self._ema)
        level_pct = self._y_to_percent(level_y)

        # Stability: low std → higher confidence
        arr = np.fromiter(self._ys, dtype=np.float64)
        std = float(arr.std())
        stab = float(np.clip(1.0 - std / max(8.0, self.roi.height * 0.15), 0.0, 1.0))
        conf = float(np.clip(0.5 * raw_conf + 0.5 * stab, 0.0, 1.0))
        detected = conf >= self.min_conf

        if not detected:
            return LevelResult(level_y, level_pct, conf, False, ts)

        return LevelResult(level_y, level_pct, conf, True, ts)


# ---------------------------------------------------------------------------
# Drop
# ---------------------------------------------------------------------------

@dataclass
class DropEvent:
    timestamp: float
    confidence: float


class DropDetector:
    """Detect granule falls through DROP ROI with WAITING/DETECTED/COOLDOWN."""

    WAITING = "WAITING"
    DETECTED = "DETECTED"
    COOLDOWN = "COOLDOWN"

    def __init__(self, cfg: Dict[str, Any], drop_roi: ROI) -> None:
        self.cfg = cfg
        self.roi = drop_roi
        self.method = str(cfg.get("method", "frame_diff")).lower()
        self.cooldown_ms = int(cfg.get("cooldown_ms", 500))
        self.min_area = float(cfg.get("min_area", 15))
        self.max_area = float(cfg.get("max_area", 5000))
        self.min_conf = float(cfg.get("min_confidence", 0.5))
        self.diff_thr = int(cfg.get("diff_threshold", 25))
        self.morph_k = int(cfg.get("morph_ksize", 3))

        self.state = self.WAITING
        self._prev_gray: Optional[np.ndarray] = None
        self._cooldown_until = 0.0
        self._motion_streak = 0
        self._last_cent_y: Optional[float] = None
        self.last_motion_box: Optional[Tuple[int, int, int, int]] = None  # x,y,w,h absolute

        self._bg = None
        if self.method == "mog2":
            self._bg = cv2.createBackgroundSubtractorMOG2(
                history=int(cfg.get("mog2_history", 120)),
                varThreshold=float(cfg.get("mog2_var_threshold", 16)),
                detectShadows=False,
            )

    def reset(self) -> None:
        self.state = self.WAITING
        self._prev_gray = None
        self._cooldown_until = 0.0
        self._motion_streak = 0
        self._last_cent_y = None
        self.last_motion_box = None

    def _mask(self, crop_bgr: np.ndarray) -> np.ndarray:
        gray = blur_gray(crop_bgr, 3)
        if self.method == "mog2" and self._bg is not None:
            fg = self._bg.apply(gray, learningRate=-1)
            _, mask = cv2.threshold(fg, 200, 255, cv2.THRESH_BINARY)
        else:
            if self._prev_gray is None or self._prev_gray.shape != gray.shape:
                self._prev_gray = gray
                return np.zeros_like(gray)
            diff = cv2.absdiff(self._prev_gray, gray)
            self._prev_gray = gray
            _, mask = cv2.threshold(diff, self.diff_thr, 255, cv2.THRESH_BINARY)

        k = self.morph_k if self.morph_k % 2 == 1 else self.morph_k + 1
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (k, k))
        mask = cv2.morphologyEx(mask, cv2.MORPH_OPEN, kernel, iterations=1)
        mask = cv2.morphologyEx(mask, cv2.MORPH_CLOSE, kernel, iterations=1)
        return mask

    def _best_blob(self, mask: np.ndarray) -> Tuple[Optional[Tuple[int, int, int, int]], float, Optional[float]]:
        """Return (x,y,w,h local), confidence, centroid_y local."""
        cnts, _ = cv2.findContours(mask, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
        best = None
        best_score = 0.0
        for c in cnts:
            area = float(cv2.contourArea(c))
            if area < self.min_area or area > self.max_area:
                continue
            x, y, w, h = cv2.boundingRect(c)
            if w < 1 or h < 1:
                continue
            # Reject wide horizontal bands (moving fill surface) and huge dust sheets
            aspect = w / float(h)
            if aspect > 4.0 or aspect < 0.15:
                continue
            extent = area / float(w * h)
            # Prefer compact, medium-sized blobs
            size_score = 1.0 - abs(area - (self.min_area * 4)) / (self.max_area * 0.5 + 1e-6)
            size_score = float(np.clip(size_score, 0.0, 1.0))
            score = area * (0.4 + 0.6 * extent) * (1.0 if 0.3 <= aspect <= 3.0 else 0.5)
            if score > best_score:
                best_score = score
                best = (c, area, x, y, w, h, extent)

        if best is None:
            return None, 0.0, None
        c, area, x, y, w, h, extent = best
        # Confidence: area in band + compactness
        log_span = max(1.0, np.log10(self.max_area) - np.log10(max(self.min_area, 1)))
        area_term = 1.0 - abs(np.log10(max(area, 1)) - np.log10(max(self.min_area * 5, 1))) / (log_span + 1e-6)
        conf = float(np.clip(0.55 * float(np.clip(area_term, 0, 1)) + 0.45 * extent, 0.0, 1.0))
        M = cv2.moments(c)
        cy = (M["m01"] / M["m00"]) if M["m00"] > 1e-6 else float(y + h / 2)
        return (x, y, w, h), conf, cy

    def update(self, frame: np.ndarray, timestamp: Optional[float] = None) -> Optional[DropEvent]:
        ts = time.time() if timestamp is None else timestamp
        roi = self.roi.clamp(frame.shape[1], frame.shape[0])
        crop = frame[roi.y1 : roi.y2, roi.x1 : roi.x2]
        self.last_motion_box = None

        if crop.size == 0:
            return None

        mask = self._mask(crop)
        box, conf, cy = self._best_blob(mask)
        moving = box is not None and conf >= self.min_conf * 0.5

        if moving and box is not None:
            x, y, w, h = box
            self.last_motion_box = (roi.x1 + x, roi.y1 + y, w, h)

        # Downward motion preference
        downward = True
        if cy is not None and self._last_cent_y is not None:
            downward = cy >= self._last_cent_y - 1.0
        if cy is not None:
            self._last_cent_y = cy

        now_ms = ts * 1000.0
        event: Optional[DropEvent] = None

        if self.state == self.COOLDOWN:
            if now_ms >= self._cooldown_until and not moving:
                self.state = self.WAITING
                self._motion_streak = 0
            elif now_ms >= self._cooldown_until and moving:
                # still busy — stay until quiet
                pass
            return None

        if self.state == self.WAITING:
            if moving and downward and conf >= self.min_conf * 0.5:
                self._motion_streak += 1
                # Require brief persistence, then count once and enter cooldown
                if self._motion_streak >= 2 and conf >= self.min_conf:
                    event = DropEvent(timestamp=ts, confidence=float(conf))
                    self.state = self.COOLDOWN
                    self._cooldown_until = now_ms + self.cooldown_ms
                    self._motion_streak = 0
                    log.info("Drop detected conf=%.2f", event.confidence)
                    return event
                self.state = self.DETECTED
            else:
                self._motion_streak = 0
            return None

        if self.state == self.DETECTED:
            if moving and downward and conf >= self.min_conf:
                event = DropEvent(timestamp=ts, confidence=float(conf))
                self.state = self.COOLDOWN
                self._cooldown_until = now_ms + self.cooldown_ms
                self._motion_streak = 0
                log.info("Drop detected conf=%.2f", conf)
                return event
            if not moving:
                self.state = self.WAITING
                self._motion_streak = 0
            return None

        return None
