#!/usr/bin/env python3
"""Diagnostic tones for the SA828 mic path.

left.wav / right.wav tell us which jack conductor is soldered to MIC+.
tune.wav is a long steady tone for turning the series pot on-air.
"""

from __future__ import annotations

import math
import wave
from array import array
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "audio"
RATE = 16000
AMP = 26000
FREQ = 1000


def tone(seconds: float) -> list[int]:
    n = int(RATE * seconds)
    return [int(AMP * math.sin(2 * math.pi * FREQ * i / RATE)) for i in range(n)]


def write(name: str, channels: int, samples: array) -> None:
    path = OUT / name
    with wave.open(str(path), "wb") as o:
        o.setnchannels(channels)
        o.setsampwidth(2)
        o.setframerate(RATE)
        o.writeframes(samples.tobytes())
    print(f"wrote {path} ch={channels} {len(samples) / channels / RATE:.1f}s")


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    mono = tone(5.0)

    left = array("h")
    right = array("h")
    for s in mono:
        left.extend((s, 0))
        right.extend((0, s))
    write("tone_left.wav", 2, left)
    write("tone_right.wav", 2, right)

    both = array("h")
    for s in tone(60.0):
        both.extend((s, s))
    write("tone_tune.wav", 2, both)


if __name__ == "__main__":
    main()
