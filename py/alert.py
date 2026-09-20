"""Play the empty-silo clip on the Pi headphone jack. GPIO PTT keys TX."""

from __future__ import annotations

import logging
import subprocess
import time
from pathlib import Path

log = logging.getLogger(__name__)

AUDIO_DEVICE = "plughw:CARD=Headphones,DEV=0"
ROOT = Path(__file__).resolve().parent.parent
VOICE_FILE = ROOT / "audio" / "silo_is_empty_vox.wav"
COOLDOWN_S = 30.0


class SiloAlert:
    def __init__(self) -> None:
        self._last = 0.0
        self._was_empty = False

    def close(self) -> None:
        pass

    def update(self, empty: bool) -> bool:
        became = empty and not self._was_empty
        self._was_empty = empty
        if not became:
            return False
        now = time.monotonic()
        if now - self._last < COOLDOWN_S:
            log.info("Alert skipped (cooldown)")
            return False
        self._play()
        self._last = now
        return True

    def _play(self) -> None:
        play_voice()
        log.info("Alert played")


def play_voice() -> None:
    try:
        subprocess.run(
            ["amixer", "-c", "Headphones", "sset", "PCM", "--", "0dB", "unmute"],
            check=False,
            timeout=5,
        )
        subprocess.run(
            ["aplay", "-D", AUDIO_DEVICE, "-q", str(VOICE_FILE)],
            check=False,
            timeout=30,
        )
    except Exception as e:
        log.error("aplay failed: %s", e)
