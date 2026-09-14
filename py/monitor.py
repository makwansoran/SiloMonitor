"""Level state machine, hysteresis, alarms, drop statistics."""

from __future__ import annotations

import logging
import time
from collections import deque
from dataclasses import dataclass, field
from typing import Deque, Dict, List, Optional, Tuple

from vision import DropEvent, LevelResult

log = logging.getLogger(__name__)


class LevelState:
    NORMAL = "NORMAL"
    LOW = "LOW"
    CRITICAL = "CRITICAL"
    EMPTY = "EMPTY"
    UNKNOWN = "UNKNOWN"


class AlarmLevel:
    OK = "OK"
    WARNING = "WARNING"
    CRITICAL = "CRITICAL"


@dataclass
class MonitorSnapshot:
    level_state: str
    alarm: str
    level_percent: Optional[float]
    level_y: Optional[float]
    level_confidence: float
    level_detected: bool
    drop_count: int
    drops_last_1m: int
    drops_last_5m: int
    drops_last_1h: int
    drops_per_min: float
    last_drop_ts: Optional[float]
    seconds_since_drop: Optional[float]
    alarm_message: str = ""


@dataclass
class StateChange:
    timestamp: float
    kind: str  # level_state | alarm
    old: str
    new: str
    message: str = ""


@dataclass
class AlarmEvent:
    timestamp: float
    alarm_type: str
    message: str


@dataclass
class Monitor:
    level_cfg: Dict
    alarm_cfg: Dict
    drop_timestamps: Deque[float] = field(default_factory=lambda: deque(maxlen=20000))
    drop_count: int = 0
    last_drop_ts: Optional[float] = None
    level_state: str = LevelState.UNKNOWN
    alarm: str = AlarmLevel.OK
    _empty_since: Optional[float] = None
    _pending_states: List[StateChange] = field(default_factory=list)
    _pending_alarms: List[AlarmEvent] = field(default_factory=list)

    def pop_events(self) -> Tuple[List[StateChange], List[AlarmEvent]]:
        s, a = self._pending_states, self._pending_alarms
        self._pending_states = []
        self._pending_alarms = []
        return s, a

    def register_drop(self, ev: DropEvent) -> None:
        self.drop_count += 1
        self.last_drop_ts = ev.timestamp
        self.drop_timestamps.append(ev.timestamp)

    def _count_since(self, now: float, seconds: float) -> int:
        cut = now - seconds
        return sum(1 for t in self.drop_timestamps if t >= cut)

    def _set_level_state(self, new: str, ts: float, msg: str = "") -> None:
        if new == self.level_state:
            return
        old = self.level_state
        self.level_state = new
        self._pending_states.append(
            StateChange(timestamp=ts, kind="level_state", old=old, new=new, message=msg)
        )
        log.info("Level state %s → %s %s", old, new, msg)

    def _set_alarm(self, new: str, ts: float, message: str) -> None:
        if new == self.alarm and not message:
            return
        if new != self.alarm:
            old = self.alarm
            self.alarm = new
            self._pending_states.append(
                StateChange(timestamp=ts, kind="alarm", old=old, new=new, message=message)
            )
            self._pending_alarms.append(
                AlarmEvent(timestamp=ts, alarm_type=new, message=message)
            )
            log.warning("Alarm %s → %s: %s", old, new, message)
        elif message and new != AlarmLevel.OK:
            # same level, refresh message only if escalating detail — skip spam
            pass

    def update_level(self, lr: LevelResult) -> None:
        cfg = self.level_cfg
        ts = lr.timestamp
        empty_thr = float(cfg.get("empty_threshold", 5))
        crit_thr = float(cfg.get("critical_threshold", 20))
        low_thr = float(cfg.get("low_threshold", 40))
        low_exit = float(cfg.get("low_exit", low_thr + 2))
        crit_exit = float(cfg.get("critical_exit", crit_thr + 2))
        empty_exit = float(cfg.get("empty_exit", empty_thr + 3))
        empty_confirm = float(cfg.get("empty_confirmation_seconds", 10))
        min_conf = float(cfg.get("min_confidence", 0.6))

        if not lr.detected or lr.level_percent is None or lr.confidence < min_conf:
            self._empty_since = None
            self._set_level_state(LevelState.UNKNOWN, ts, "low confidence")
            return

        pct = float(lr.level_percent)

        # Track continuous empty condition
        if pct <= empty_thr:
            if self._empty_since is None:
                self._empty_since = ts
        else:
            self._empty_since = None

        cur = self.level_state

        # Hysteresis transitions
        if cur == LevelState.EMPTY:
            if pct > empty_exit:
                # leave empty into appropriate band
                cur = self._band_from_pct(pct, low_thr, crit_thr, empty_thr)
                self._set_level_state(cur, ts)
            return

        if cur == LevelState.CRITICAL:
            if pct <= empty_thr and self._empty_since and (ts - self._empty_since) >= empty_confirm:
                self._set_level_state(LevelState.EMPTY, ts, "confirmed empty")
            elif pct > crit_exit:
                new = LevelState.LOW if pct < low_exit else LevelState.NORMAL
                if pct >= low_exit:
                    new = LevelState.NORMAL
                elif pct >= crit_exit:
                    new = LevelState.LOW
                self._set_level_state(new, ts)
            return

        if cur == LevelState.LOW:
            if pct <= empty_thr and self._empty_since and (ts - self._empty_since) >= empty_confirm:
                self._set_level_state(LevelState.EMPTY, ts, "confirmed empty")
            elif pct <= crit_thr:
                self._set_level_state(LevelState.CRITICAL, ts)
            elif pct > low_exit:
                self._set_level_state(LevelState.NORMAL, ts)
            return

        if cur in (LevelState.NORMAL, LevelState.UNKNOWN):
            if pct <= empty_thr and self._empty_since and (ts - self._empty_since) >= empty_confirm:
                self._set_level_state(LevelState.EMPTY, ts, "confirmed empty")
            elif pct <= crit_thr:
                self._set_level_state(LevelState.CRITICAL, ts)
            elif pct < low_thr:
                self._set_level_state(LevelState.LOW, ts)
            else:
                self._set_level_state(LevelState.NORMAL, ts)

    @staticmethod
    def _band_from_pct(pct: float, low_thr: float, crit_thr: float, empty_thr: float) -> str:
        if pct <= empty_thr:
            return LevelState.CRITICAL  # not yet confirmed empty
        if pct <= crit_thr:
            return LevelState.CRITICAL
        if pct < low_thr:
            return LevelState.LOW
        return LevelState.NORMAL

    def update_alarm(self, lr: LevelResult, now: Optional[float] = None) -> str:
        """Compute OK/WARNING/CRITICAL from level state + drop timing. Returns message."""
        ts = time.time() if now is None else now
        acfg = self.alarm_cfg
        warn_m = float(acfg.get("no_drop_warning_minutes", 5))
        crit_m = float(acfg.get("no_drop_critical_minutes", 10))
        ignore_above = float(acfg.get("no_drop_ignore_above_percent", 60))

        pct = lr.level_percent
        since = None if self.last_drop_ts is None else (ts - self.last_drop_ts)
        msg = "OK"

        # Highest priority: EMPTY / critical level
        if self.level_state == LevelState.EMPTY:
            msg = "Silo EMPTY (confirmed)"
            self._set_alarm(AlarmLevel.CRITICAL, ts, msg)
            return msg

        if self.level_state == LevelState.UNKNOWN:
            msg = "Level UNKNOWN"
            # don't escalate alarm solely from unknown
            if self.alarm == AlarmLevel.CRITICAL:
                pass
            else:
                self._set_alarm(AlarmLevel.OK, ts, msg)
            return msg

        no_drop_bad = False
        if since is not None and pct is not None and pct < ignore_above:
            if since >= crit_m * 60 and self.level_state in (
                LevelState.LOW,
                LevelState.CRITICAL,
            ):
                msg = f"No drop {since/60:.1f}m + level {self.level_state}"
                self._set_alarm(AlarmLevel.CRITICAL, ts, msg)
                return msg
            if since >= warn_m * 60 and self.level_state in (
                LevelState.LOW,
                LevelState.CRITICAL,
            ):
                no_drop_bad = True

        if self.level_state == LevelState.CRITICAL:
            msg = f"Level CRITICAL ({pct:.1f}%)" if pct is not None else "Level CRITICAL"
            if no_drop_bad:
                msg += " + no recent drops"
            self._set_alarm(AlarmLevel.CRITICAL, ts, msg)
            return msg

        if self.level_state == LevelState.LOW or no_drop_bad:
            if no_drop_bad:
                msg = f"No drop {since/60:.1f}m + low level"
            else:
                msg = f"Level LOW ({pct:.1f}%)" if pct is not None else "Level LOW"
            self._set_alarm(AlarmLevel.WARNING, ts, msg)
            return msg

        # NORMAL with high fill → OK even if no drops
        msg = "OK"
        self._set_alarm(AlarmLevel.OK, ts, msg)
        return msg

    def snapshot(self, lr: LevelResult, now: Optional[float] = None) -> MonitorSnapshot:
        ts = time.time() if now is None else now
        since = None if self.last_drop_ts is None else (ts - self.last_drop_ts)
        d1 = self._count_since(ts, 60)
        d5 = self._count_since(ts, 300)
        d60 = self._count_since(ts, 3600)
        # instantaneous drops/min from last 60s
        dpm = float(d1)
        return MonitorSnapshot(
            level_state=self.level_state,
            alarm=self.alarm,
            level_percent=lr.level_percent,
            level_y=lr.level_y,
            level_confidence=lr.confidence,
            level_detected=lr.detected,
            drop_count=self.drop_count,
            drops_last_1m=d1,
            drops_last_5m=d5,
            drops_last_1h=d60,
            drops_per_min=dpm,
            last_drop_ts=self.last_drop_ts,
            seconds_since_drop=since,
            alarm_message="",
        )
