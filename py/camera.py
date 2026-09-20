"""Camera capture: OpenCV device, video file, optional debug writer."""

from __future__ import annotations

import logging
import time
from pathlib import Path
from typing import Optional, Union

import cv2
import numpy as np

log = logging.getLogger(__name__)


class Camera:
    def __init__(
        self,
        source: Union[str, int] = 0,
        width: int = 640,
        height: int = 480,
        fps: float = 10.0,
        save_debug_video: bool = False,
        debug_video_path: str = "data/debug.avi",
    ) -> None:
        self.width = int(width)
        self.height = int(height)
        self.target_fps = float(fps)
        self._min_dt = 1.0 / self.target_fps if self.target_fps > 0 else 0.0
        self._last_grab = 0.0
        self._writer: Optional[cv2.VideoWriter] = None
        self._source = source

        if isinstance(source, str) and source.lower() in ("camera", "cam", "0"):
            source = 0

        live = isinstance(source, str) and source.lower().startswith(
            ("rtsp://", "http://", "https://", "tcp://")
        )

        if isinstance(source, str):
            self.cap = cv2.VideoCapture(source, cv2.CAP_FFMPEG)
            self.is_file = not live
            if live:
                self.cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)
        else:
            self.cap = cv2.VideoCapture(int(source))
            self.is_file = False
            self.cap.set(cv2.CAP_PROP_FRAME_WIDTH, self.width)
            self.cap.set(cv2.CAP_PROP_FRAME_HEIGHT, self.height)
            self.cap.set(cv2.CAP_PROP_FPS, self.target_fps)

        if not self.cap.isOpened():
            raise RuntimeError(f"Could not open video source: {self._source!r}")

        log.info(
            "Camera open source=%r size=%dx%d fps=%.1f file=%s",
            self._source,
            self.width,
            self.height,
            self.target_fps,
            self.is_file,
        )

        if save_debug_video:
            path = Path(debug_video_path)
            path.parent.mkdir(parents=True, exist_ok=True)
            fourcc = cv2.VideoWriter_fourcc(*"XVID")
            self._writer = cv2.VideoWriter(
                str(path), fourcc, self.target_fps, (self.width, self.height)
            )
            if not self._writer.isOpened():
                log.warning("Debug video writer failed for %s", path)
                self._writer = None

    def read(self) -> Optional[np.ndarray]:
        """Return BGR frame resized to configured size, or None on EOF/failure."""
        if self._min_dt > 0 and not self.is_file:
            now = time.monotonic()
            wait = self._min_dt - (now - self._last_grab)
            if wait > 0:
                time.sleep(wait)

        ok, frame = self.cap.read()
        self._last_grab = time.monotonic()
        if not ok or frame is None:
            return None

        if frame.shape[1] != self.width or frame.shape[0] != self.height:
            frame = cv2.resize(frame, (self.width, self.height), interpolation=cv2.INTER_AREA)

        if self._writer is not None:
            self._writer.write(frame)
        return frame

    def release(self) -> None:
        if self.cap is not None:
            self.cap.release()
        if self._writer is not None:
            self._writer.release()
            self._writer = None
        log.info("Camera released")

    def __enter__(self) -> "Camera":
        return self

    def __exit__(self, *args) -> None:
        self.release()
