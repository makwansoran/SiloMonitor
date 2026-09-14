"""Shared helpers: config load, logging setup, debug overlay."""

from __future__ import annotations

import logging
import time
from pathlib import Path
from typing import Any, Dict, Optional

import cv2
import numpy as np
import yaml

from monitor import MonitorSnapshot
from vision import DropDetector, LevelResult, ROI, roi_from_cfg


def app_dir() -> Path:
    return Path(__file__).resolve().parent


def load_config(path: Optional[str] = None) -> Dict[str, Any]:
    p = Path(path) if path else app_dir() / "config.yaml"
    if not p.is_file():
        p = app_dir() / "config.yaml"
    with p.open() as f:
        cfg = yaml.safe_load(f)
    # resolve relative db/log paths against py/
    storage = cfg.setdefault("storage", {})
    db = Path(storage.get("db_path", "data/silo.db"))
    if not db.is_absolute():
        storage["db_path"] = str(app_dir() / db)
    log_cfg = cfg.setdefault("logging", {})
    lf = Path(log_cfg.get("file", "data/silo.log"))
    if not lf.is_absolute():
        log_cfg["file"] = str(app_dir() / lf)
    cam = cfg.setdefault("camera", {})
    dv = Path(cam.get("debug_video_path", "data/debug.avi"))
    if not dv.is_absolute():
        cam["debug_video_path"] = str(app_dir() / dv)
    return cfg


def setup_logging(cfg: Dict[str, Any]) -> None:
    level_name = str(cfg.get("logging", {}).get("level", "INFO")).upper()
    level = getattr(logging, level_name, logging.INFO)
    log_path = Path(cfg.get("logging", {}).get("file", "data/silo.log"))
    log_path.parent.mkdir(parents=True, exist_ok=True)
    root = logging.getLogger()
    root.handlers.clear()
    root.setLevel(level)
    fmt = logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
    fh = logging.FileHandler(log_path)
    fh.setFormatter(fmt)
    sh = logging.StreamHandler()
    sh.setFormatter(fmt)
    root.addHandler(fh)
    root.addHandler(sh)


def fmt_hms(seconds: Optional[float]) -> str:
    if seconds is None:
        return "--:--:--"
    s = int(max(0, seconds))
    h, rem = divmod(s, 3600)
    m, sec = divmod(rem, 60)
    if h:
        return f"{h:02d}:{m:02d}:{sec:02d}"
    return f"{m:02d}:{sec:02d}"


def fmt_clock(ts: Optional[float]) -> str:
    if ts is None:
        return "--:--:--"
    return time.strftime("%H:%M:%S", time.localtime(ts))


def draw_overlay(
    frame: np.ndarray,
    level_roi: ROI,
    drop_roi: ROI,
    lr: LevelResult,
    snap: MonitorSnapshot,
    drop_det: DropDetector,
    fps: float,
    alarm_message: str = "",
) -> np.ndarray:
    vis = frame.copy()
    cv2.rectangle(vis, (level_roi.x1, level_roi.y1), (level_roi.x2, level_roi.y2), (0, 255, 0), 2)
    cv2.rectangle(vis, (drop_roi.x1, drop_roi.y1), (drop_roi.x2, drop_roi.y2), (0, 165, 255), 2)
    cv2.putText(vis, "LEVEL", (level_roi.x1, max(14, level_roi.y1 - 6)),
                cv2.FONT_HERSHEY_SIMPLEX, 0.45, (0, 255, 0), 1)
    cv2.putText(vis, "DROP", (drop_roi.x1, max(14, drop_roi.y1 - 6)),
                cv2.FONT_HERSHEY_SIMPLEX, 0.45, (0, 165, 255), 1)

    if lr.level_y is not None:
        y = int(round(lr.level_y))
        cv2.line(vis, (level_roi.x1, y), (level_roi.x2, y), (0, 255, 255), 2)
        cv2.putText(
            vis,
            f"{lr.level_percent:.1f}%" if lr.level_percent is not None else "?",
            (level_roi.x2 + 4, y + 4),
            cv2.FONT_HERSHEY_SIMPLEX,
            0.5,
            (0, 255, 255),
            1,
        )

    if drop_det.last_motion_box is not None:
        x, y, w, h = drop_det.last_motion_box
        cv2.rectangle(vis, (x, y), (x + w, y + h), (0, 0, 255), 2)
        cv2.putText(vis, "DROP!", (x, max(14, y - 4)), cv2.FONT_HERSHEY_SIMPLEX, 0.6, (0, 0, 255), 2)

    # HUD panel
    lines = [
        f"STATUS: {snap.level_state}",
        f"ALARM: {snap.alarm}",
        f"LEVEL: {snap.level_percent:.1f} %" if snap.level_percent is not None else "LEVEL: --",
        f"CONFIDENCE: {snap.level_confidence * 100:.0f} %",
        f"DROPS: {snap.drop_count}",
        f"DROPS/MIN: {snap.drops_per_min:.1f}",
        f"LAST DROP: {fmt_clock(snap.last_drop_ts)}",
        f"TIME SINCE DROP: {fmt_hms(snap.seconds_since_drop)}",
        f"DROP STATE: {drop_det.state}",
        f"FPS: {fps:.1f}",
    ]
    if alarm_message:
        lines.append(alarm_message[:48])

    x0, y0 = 8, 18
    for i, line in enumerate(lines):
        cv2.putText(
            vis,
            line,
            (x0, y0 + i * 18),
            cv2.FONT_HERSHEY_SIMPLEX,
            0.5,
            (220, 220, 220),
            1,
            cv2.LINE_AA,
        )
    return vis


def parse_source(source: str):
    if source in ("camera", "cam", "0"):
        return 0
    if source.isdigit():
        return int(source)
    return source
