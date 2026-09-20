"""OpenCV empty vs full classifier from labeled JPEGs (32x32 grayscale centroid)."""

from __future__ import annotations

import json
import logging
import time
from pathlib import Path
from typing import Optional, Tuple

import cv2
import numpy as np

log = logging.getLogger(__name__)

SIZE = 32


def repo_data_dir() -> Path:
    return Path(__file__).resolve().parent.parent / "data"


def empty_dir() -> Path:
    return repo_data_dir() / "empty"


def full_dir() -> Path:
    return repo_data_dir() / "full"


def model_path() -> Path:
    return repo_data_dir() / "model.json"


def features_bgr(bgr: np.ndarray) -> np.ndarray:
    gray = cv2.cvtColor(bgr, cv2.COLOR_BGR2GRAY)
    small = cv2.resize(gray, (SIZE, SIZE), interpolation=cv2.INTER_AREA)
    return small.astype(np.float32).reshape(-1) / 255.0


def save_label(empty: bool, bgr: np.ndarray) -> Path:
    folder = empty_dir() if empty else full_dir()
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / f"{int(time.time() * 1000)}.jpg"
    if not cv2.imwrite(str(path), bgr):
        raise RuntimeError(f"could not write {path}")
    log.info("Saved %s → %s", "EMPTY" if empty else "FULL", path)
    return path


def count_labels() -> Tuple[int, int]:
    def n(p: Path) -> int:
        if not p.is_dir():
            return 0
        return sum(1 for f in p.iterdir() if f.suffix.lower() in (".jpg", ".jpeg"))

    return n(empty_dir()), n(full_dir())


def _load_feats(folder: Path) -> list[np.ndarray]:
    feats: list[np.ndarray] = []
    if not folder.is_dir():
        return feats
    for f in sorted(folder.iterdir()):
        if f.suffix.lower() not in (".jpg", ".jpeg"):
            continue
        img = cv2.imread(str(f))
        if img is None:
            log.warning("Skip unreadable %s", f)
            continue
        feats.append(features_bgr(img))
    return feats


class EmptyFullModel:
    def __init__(
        self,
        empty: np.ndarray,
        full: np.ndarray,
        train_accuracy: float,
        n_empty: int,
        n_full: int,
    ) -> None:
        self.empty = empty
        self.full = full
        self.train_accuracy = train_accuracy
        self.n_empty = n_empty
        self.n_full = n_full

    @classmethod
    def load(cls, path: Optional[Path] = None) -> Optional["EmptyFullModel"]:
        p = path or model_path()
        if not p.is_file():
            return None
        data = json.loads(p.read_text())
        return cls(
            empty=np.array(data["empty"], dtype=np.float32),
            full=np.array(data["full"], dtype=np.float32),
            train_accuracy=float(data.get("train_accuracy", 0.0)),
            n_empty=int(data.get("n_empty", 0)),
            n_full=int(data.get("n_full", 0)),
        )

    def save(self, path: Optional[Path] = None) -> None:
        p = path or model_path()
        p.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "name": "Spectr Silo",
            "w": SIZE,
            "h": SIZE,
            "empty": self.empty.tolist(),
            "full": self.full.tolist(),
            "train_accuracy": self.train_accuracy,
            "trained_at_unix": int(time.time()),
            "n_empty": self.n_empty,
            "n_full": self.n_full,
        }
        p.write_text(json.dumps(payload, indent=2))
        log.info("Wrote %s", p)

    def predict(self, bgr: np.ndarray) -> Tuple[bool, float]:
        """Return (is_empty, confidence 0..1)."""
        feat = features_bgr(bgr)
        d_empty = float(np.sum((feat - self.empty) ** 2))
        d_full = float(np.sum((feat - self.full) ** 2))
        empty = d_empty <= d_full
        total = d_empty + d_full
        if total <= 1e-9:
            return empty, 0.5
        winner = d_full if empty else d_empty
        return empty, float(np.clip(winner / total, 0.0, 1.0))


def train() -> EmptyFullModel:
    empty_feats = _load_feats(empty_dir())
    full_feats = _load_feats(full_dir())
    if not empty_feats:
        raise RuntimeError(f"need ≥1 image in {empty_dir()}")
    if not full_feats:
        raise RuntimeError(f"need ≥1 image in {full_dir()}")
    empty_c = np.mean(np.stack(empty_feats), axis=0)
    full_c = np.mean(np.stack(full_feats), axis=0)
    ok = 0
    total = 0
    for f in empty_feats:
        total += 1
        if np.sum((f - empty_c) ** 2) <= np.sum((f - full_c) ** 2):
            ok += 1
    for f in full_feats:
        total += 1
        if np.sum((f - full_c) ** 2) < np.sum((f - empty_c) ** 2):
            ok += 1
    model = EmptyFullModel(
        empty=empty_c,
        full=full_c,
        train_accuracy=ok / total,
        n_empty=len(empty_feats),
        n_full=len(full_feats),
    )
    model.save()
    log.info(
        "Trained accuracy=%.0f%% empty=%d full=%d",
        model.train_accuracy * 100.0,
        model.n_empty,
        model.n_full,
    )
    return model
