#!/usr/bin/env python3
"""Build one continuous TX clip: beep + gap + silo is empty (stereo)."""

from __future__ import annotations

import math
import wave
from array import array
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "audio" / "silo_is_empty.wav"
OUT = ROOT / "audio" / "silo_is_empty_vox.wav"
BEEP = ROOT / "audio" / "vox_beep.wav"

VOICE_SCALE = 0.25
BEEP_AMP = 12000
BEEP_S = 0.5
GAP_S = 0.35
TAIL_S = 0.4


def write_stereo(path: Path, rate: int, mono: array) -> None:
    stereo = array("h")
    for s in mono:
        stereo.extend((s, s))
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as o:
        o.setnchannels(2)
        o.setsampwidth(2)
        o.setframerate(rate)
        o.writeframes(stereo.tobytes())
    print(f"wrote {path}  {len(mono) / rate:.2f}s  {rate} Hz  stereo")


def main() -> None:
    with wave.open(str(SRC), "rb") as w:
        ch, sw, rate, n = w.getnchannels(), w.getsampwidth(), w.getframerate(), w.getnframes()
        if ch != 1 or sw != 2:
            raise SystemExit(f"Need mono 16-bit wav, got {ch}ch {sw}B")
        voice = array("h")
        voice.frombytes(w.readframes(n))

    for i, s in enumerate(voice):
        voice[i] = int(max(-32767, min(32767, s * VOICE_SCALE)))

    n_beep = int(rate * BEEP_S)
    beep = array(
        "h",
        [int(BEEP_AMP * math.sin(2 * math.pi * 1000 * i / rate)) for i in range(n_beep)],
    )
    gap = array("h", [0] * int(rate * GAP_S))
    tail = array("h", [0] * int(rate * TAIL_S))

    combined = array("h")
    combined.extend(beep)
    combined.extend(gap)
    combined.extend(voice)
    combined.extend(tail)

    write_stereo(BEEP, rate, beep)
    write_stereo(OUT, rate, combined)


if __name__ == "__main__":
    main()
