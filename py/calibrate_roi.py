#!/usr/bin/env python3
"""Draw LEVEL and DROP ROIs with the mouse; save into config.yaml."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

import cv2
import yaml

from camera import Camera
from vision import ROI


def load_cfg(path: Path) -> dict:
    with path.open() as f:
        return yaml.safe_load(f)


def save_cfg(path: Path, cfg: dict) -> None:
    with path.open("w") as f:
        yaml.safe_dump(cfg, f, sort_keys=False, default_flow_style=False)


def grab_frame(args, cfg: dict):
    if args.image:
        img = cv2.imread(args.image)
        if img is None:
            raise SystemExit(f"Could not read image: {args.image}")
        w = int(cfg["camera"]["width"])
        h = int(cfg["camera"]["height"])
        return cv2.resize(img, (w, h))
    src = 0 if args.source in ("camera", "cam", "0") else args.source
    cam = Camera(
        source=src if src != "camera" else 0,
        width=int(cfg["camera"]["width"]),
        height=int(cfg["camera"]["height"]),
        fps=float(cfg["camera"]["fps"]),
    )
    frame = None
    for _ in range(30):
        frame = cam.read()
        if frame is not None:
            break
    cam.release()
    if frame is None:
        raise SystemExit("No frame from camera")
    return frame


def select_roi(window: str, frame, title: str) -> ROI:
    print(f"Drag ROI for {title}, ENTER to confirm, C to cancel")
    clone = frame.copy()
    cv2.putText(
        clone,
        f"Select {title} ROI",
        (10, 24),
        cv2.FONT_HERSHEY_SIMPLEX,
        0.7,
        (0, 255, 255),
        2,
    )
    r = cv2.selectROI(window, clone, showCrosshair=True, fromCenter=False)
    cv2.destroyWindow(window)
    x, y, w, h = [int(v) for v in r]
    if w < 2 or h < 2:
        raise SystemExit(f"Invalid ROI for {title}")
    return ROI(x, y, x + w, y + h)


def main() -> int:
    ap = argparse.ArgumentParser(description="Calibrate LEVEL/DROP ROIs")
    ap.add_argument("--config", default="config.yaml")
    ap.add_argument("--image", default=None, help="Still image path")
    ap.add_argument("--source", default="camera", help="camera or video path")
    args = ap.parse_args()

    cfg_path = Path(args.config)
    if not cfg_path.is_file():
        cfg_path = Path(__file__).resolve().parent / "config.yaml"
    cfg = load_cfg(cfg_path)
    frame = grab_frame(args, cfg)

    win = "silo-roi-calibrate"
    cv2.namedWindow(win, cv2.WINDOW_NORMAL)
    level = select_roi(win, frame, "LEVEL")
    drop = select_roi(win, frame, "DROP")

    cfg["level_roi"] = level.as_dict()
    cfg["drop_roi"] = drop.as_dict()
    save_cfg(cfg_path, cfg)
    print(f"Saved ROIs to {cfg_path}")
    print("level_roi:", cfg["level_roi"])
    print("drop_roi:", cfg["drop_roi"])

    vis = frame.copy()
    cv2.rectangle(vis, (level.x1, level.y1), (level.x2, level.y2), (0, 255, 0), 2)
    cv2.rectangle(vis, (drop.x1, drop.y1), (drop.x2, drop.y2), (0, 165, 255), 2)
    cv2.putText(vis, "LEVEL", (level.x1, max(15, level.y1 - 6)), cv2.FONT_HERSHEY_SIMPLEX, 0.5, (0, 255, 0), 1)
    cv2.putText(vis, "DROP", (drop.x1, max(15, drop.y1 - 6)), cv2.FONT_HERSHEY_SIMPLEX, 0.5, (0, 165, 255), 1)
    cv2.imshow(win, vis)
    print("Press any key to exit")
    cv2.waitKey(0)
    cv2.destroyAllWindows()
    return 0


if __name__ == "__main__":
    sys.exit(main())
