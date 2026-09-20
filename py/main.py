#!/usr/bin/env python3
"""Silo vision main loop: camera/video → level + drops → monitor → SQLite + debug UI."""

from __future__ import annotations

import argparse
import logging
import sys
import time

import cv2

from alert import SiloAlert
from camera import Camera
from db import Database
from empty_full import EmptyFullModel, count_labels, save_label, train
from monitor import Monitor
from util import draw_overlay, load_config, parse_source, setup_logging
from vision import DropDetector, LevelDetector, roi_from_cfg

log = logging.getLogger("main")


def build_pipeline(cfg: dict):
    level_roi = roi_from_cfg(cfg["level_roi"])
    drop_roi = roi_from_cfg(cfg["drop_roi"])
    level = LevelDetector(
        cfg["level"],
        level_roi,
        full_at_top=bool(cfg.get("level_full_at_top", True)),
    )
    drop = DropDetector(cfg["drop"], drop_roi)
    mon = Monitor(level_cfg=cfg["level"], alarm_cfg=cfg["alarm"])
    db = Database(cfg["storage"]["db_path"])
    return level_roi, drop_roi, level, drop, mon, db


def run(source, cfg: dict, show: bool = True, max_frames: int = 0) -> Monitor:
    cam_cfg = cfg["camera"]
    level_roi, drop_roi, level_det, drop_det, mon, db = build_pipeline(cfg)
    level_interval = float(cfg["storage"].get("level_log_interval_seconds", 5))

    cam = Camera(
        source=source,
        width=int(cam_cfg["width"]),
        height=int(cam_cfg["height"]),
        fps=float(cam_cfg["fps"]),
        save_debug_video=bool(cam_cfg.get("save_debug_video", False)),
        debug_video_path=str(cam_cfg.get("debug_video_path", "data/debug.avi")),
    )
    model = EmptyFullModel.load()
    alert = SiloAlert()
    ne, nf = count_labels()
    log.info("Labels empty=%d full=%d model=%s", ne, nf, "yes" if model else "no")

    fps_ema = float(cam_cfg["fps"])
    t_prev = time.monotonic()
    frames = 0
    alarm_msg = ""
    last_frame = None

    try:
        while True:
            frame = cam.read()
            if frame is None:
                log.info("End of stream")
                break

            now = time.time()
            lr = level_det.update(frame, timestamp=now)
            drop_ev = drop_det.update(frame, timestamp=now)

            mon.update_level(lr)
            if drop_ev is not None:
                mon.register_drop(drop_ev)
                db.log_drop(drop_ev.timestamp, drop_ev.confidence)

            alarm_msg = mon.update_alarm(lr, now=now)
            snap = mon.snapshot(lr, now=now)
            last_frame = frame
            if model is not None:
                is_empty, conf = model.predict(frame)
                if is_empty and conf >= 0.55:
                    snap.level_state = "EMPTY"
                    alarm_msg = f"EMPTY ({conf * 100:.0f}%)"
                    if alert.update(True):
                        log.info("Radio alert")
                else:
                    alert.update(False)

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
            inst = 1.0 / dt
            fps_ema = 0.9 * fps_ema + 0.1 * inst

            if show:
                vis = draw_overlay(
                    frame, level_roi, drop_roi, lr, snap, drop_det, fps_ema, alarm_msg
                )
                cv2.imshow("silo-vision", vis)
                key = cv2.waitKey(1) & 0xFF
                if key in (27, ord("q")):
                    break
                if last_frame is not None and key in (ord("e"), ord("E")):
                    save_label(True, last_frame)
                if last_frame is not None and key in (ord("f"), ord("F")):
                    save_label(False, last_frame)
                if key in (ord("t"), ord("T")):
                    try:
                        model = train()
                    except Exception as e:
                        log.error("Train failed: %s", e)

            frames += 1
            if max_frames and frames >= max_frames:
                break
    finally:
        cam.release()
        db.close()
        alert.close()
        if show:
            cv2.destroyAllWindows()

    return mon


def main() -> int:
    ap = argparse.ArgumentParser(description="Silo vision monitor")
    ap.add_argument(
        "--source",
        default=None,
        help="camera | device index | rtsp://… | video file (default: config camera.rtsp_url or device)",
    )
    ap.add_argument("--config", default=None)
    ap.add_argument("--no-show", action="store_true", help="Headless (no OpenCV window)")
    args = ap.parse_args()

    cfg = load_config(args.config)
    setup_logging(cfg)
    src_arg = args.source
    if not src_arg:
        rtsp = str(cfg.get("camera", {}).get("rtsp_url", "") or "").strip()
        src_arg = rtsp if rtsp else str(cfg.get("camera", {}).get("device", 0))
    src = parse_source(str(src_arg))
    log.info("Starting silo-vision source=%r", src)
    run(src, cfg, show=not args.no_show)
    return 0


if __name__ == "__main__":
    sys.exit(main())
