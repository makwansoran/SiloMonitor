#!/usr/bin/env python3
"""Offline test on an MP4: run full pipeline, show overlay, print summary."""

from __future__ import annotations

import argparse
import logging
import sys
import time
from pathlib import Path

import cv2

from camera import Camera
from db import Database
from monitor import Monitor
from util import draw_overlay, load_config, setup_logging
from vision import DropDetector, LevelDetector, roi_from_cfg

log = logging.getLogger("test_video")


def main() -> int:
    ap = argparse.ArgumentParser(description="Test silo vision on a video file")
    ap.add_argument("--input", "-i", required=True, help="Path to MP4/AVI")
    ap.add_argument("--config", default=None)
    ap.add_argument("--no-show", action="store_true")
    ap.add_argument(
        "--db",
        default=None,
        help="Override SQLite path (default: data/test_run.db)",
    )
    args = ap.parse_args()

    inp = Path(args.input)
    if not inp.is_file():
        print(f"Input not found: {inp}", file=sys.stderr)
        return 1

    cfg = load_config(args.config)
    # isolated DB for test runs
    db_path = args.db or str(Path(__file__).resolve().parent / "data" / "test_run.db")
    cfg["storage"]["db_path"] = db_path
    # wipe previous test db for clean summary
    p = Path(db_path)
    if p.exists():
        p.unlink()

    setup_logging(cfg)
    cam_cfg = cfg["camera"]
    level_roi = roi_from_cfg(cfg["level_roi"])
    drop_roi = roi_from_cfg(cfg["drop_roi"])
    level_det = LevelDetector(
        cfg["level"], level_roi, full_at_top=bool(cfg.get("level_full_at_top", True))
    )
    drop_det = DropDetector(cfg["drop"], drop_roi)
    mon = Monitor(level_cfg=cfg["level"], alarm_cfg=cfg["alarm"])
    db = Database(db_path)
    level_interval = float(cfg["storage"].get("level_log_interval_seconds", 5))

    cam = Camera(
        source=str(inp),
        width=int(cam_cfg["width"]),
        height=int(cam_cfg["height"]),
        fps=float(cam_cfg["fps"]),
    )

    # For file playback, use video timestamps if available
    video_fps = cam.cap.get(cv2.CAP_PROP_FPS) or cam_cfg["fps"]
    frame_idx = 0
    t0_wall = time.time()
    min_level = None
    max_level = None
    empty_detected = False
    empty_time = None
    fps_ema = float(video_fps)
    t_prev = time.monotonic()
    alarm_msg = ""

    try:
        while True:
            frame = cam.read()
            if frame is None:
                break

            # Simulated timeline from video FPS so drop windows make sense
            now = t0_wall + (frame_idx / max(video_fps, 1e-3))
            lr = level_det.update(frame, timestamp=now)
            drop_ev = drop_det.update(frame, timestamp=now)

            mon.update_level(lr)
            if drop_ev is not None:
                mon.register_drop(drop_ev)
                db.log_drop(drop_ev.timestamp, drop_ev.confidence)

            alarm_msg = mon.update_alarm(lr, now=now)
            snap = mon.snapshot(lr, now=now)

            if lr.level_percent is not None:
                min_level = lr.level_percent if min_level is None else min(min_level, lr.level_percent)
                max_level = lr.level_percent if max_level is None else max(max_level, lr.level_percent)

            if mon.level_state == "EMPTY" and not empty_detected:
                empty_detected = True
                empty_time = now

            states, alarms = mon.pop_events()
            for sc in states:
                db.log_state_change(sc.timestamp, sc.kind, sc.old, sc.new, sc.message)
            for ae in alarms:
                db.log_alarm(ae.timestamp, ae.alarm_type, ae.message)

            db.log_level(
                now,
                lr.level_percent,
                lr.level_y,
                lr.confidence,
                lr.detected,
                interval_seconds=level_interval,
            )

            t_now = time.monotonic()
            dt = max(1e-6, t_now - t_prev)
            t_prev = t_now
            fps_ema = 0.9 * fps_ema + 0.1 * (1.0 / dt)

            if not args.no_show:
                vis = draw_overlay(
                    frame, level_roi, drop_roi, lr, snap, drop_det, fps_ema, alarm_msg
                )
                cv2.imshow("silo-vision-test", vis)
                key = cv2.waitKey(1) & 0xFF
                if key in (27, ord("q")):
                    break

            frame_idx += 1
    finally:
        cam.release()
        summary = db.stats_summary()
        db.close()
        if not args.no_show:
            cv2.destroyAllWindows()

    # Prefer live tracked stats; fall back to DB
    total_drops = mon.drop_count
    duration_m = (frame_idx / max(video_fps, 1e-3)) / 60.0 if frame_idx else 0.0
    avg_dpm = (total_drops / duration_m) if duration_m > 0 else float(total_drops)
    if min_level is None:
        min_level = summary.get("min_level")
    if max_level is None:
        max_level = summary.get("max_level")
    if not empty_detected:
        empty_detected = bool(summary.get("empty_detected"))
        empty_time = summary.get("empty_time")

    print()
    print("======== TEST SUMMARY ========")
    print(f"Total drops: {total_drops}")
    print(f"Average drops/min: {avg_dpm:.2f}")
    print(f"Minimum level: {min_level:.1f} %" if min_level is not None else "Minimum level: n/a")
    print(f"Maximum level: {max_level:.1f} %" if max_level is not None else "Maximum level: n/a")
    print(f"Empty detected: {'YES' if empty_detected else 'NO'}")
    if empty_detected and empty_time is not None:
        # time relative to video start
        rel = empty_time - t0_wall
        h, rem = divmod(int(rel), 3600)
        m, s = divmod(rem, 60)
        print(f"Time empty detected: {h:02d}:{m:02d}:{s:02d}")
    else:
        print("Time empty detected: n/a")
    print(f"Frames processed: {frame_idx}")
    print(f"DB: {db_path}")
    print("==============================")
    return 0


if __name__ == "__main__":
    sys.exit(main())
