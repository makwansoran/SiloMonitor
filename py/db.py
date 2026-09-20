"""SQLite storage for level samples, drops, alarms, state changes."""

from __future__ import annotations

import logging
import sqlite3
from pathlib import Path
from typing import Any, Dict, Optional

log = logging.getLogger(__name__)


class Database:
    def __init__(self, path: str) -> None:
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.conn = sqlite3.connect(str(self.path), check_same_thread=False)
        self.conn.execute("PRAGMA journal_mode=WAL;")
        self._init_schema()
        self._last_level_log = 0.0
        log.info("SQLite open %s", self.path)

    def _init_schema(self) -> None:
        cur = self.conn.cursor()
        cur.executescript(
            """
            CREATE TABLE IF NOT EXISTS level_measurements (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp REAL NOT NULL,
                level_percent REAL,
                level_y REAL,
                confidence REAL,
                detected INTEGER
            );
            CREATE TABLE IF NOT EXISTS drop_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp REAL NOT NULL,
                confidence REAL
            );
            CREATE TABLE IF NOT EXISTS alarms (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp REAL NOT NULL,
                alarm_type TEXT NOT NULL,
                message TEXT
            );
            CREATE TABLE IF NOT EXISTS state_changes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp REAL NOT NULL,
                kind TEXT NOT NULL,
                old_value TEXT,
                new_value TEXT,
                message TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_level_ts ON level_measurements(timestamp);
            CREATE INDEX IF NOT EXISTS idx_drop_ts ON drop_events(timestamp);
            """
        )
        self.conn.commit()

    def log_level(
        self,
        timestamp: float,
        level_percent: Optional[float],
        level_y: Optional[float],
        confidence: float,
        detected: bool,
        interval_seconds: float = 5.0,
        force: bool = False,
    ) -> None:
        if not force and (timestamp - self._last_level_log) < interval_seconds:
            return
        self._last_level_log = timestamp
        self.conn.execute(
            "INSERT INTO level_measurements(timestamp, level_percent, level_y, confidence, detected) "
            "VALUES (?,?,?,?,?)",
            (timestamp, level_percent, level_y, confidence, int(detected)),
        )
        self.conn.commit()

    def log_drop(self, timestamp: float, confidence: float) -> None:
        self.conn.execute(
            "INSERT INTO drop_events(timestamp, confidence) VALUES (?,?)",
            (timestamp, confidence),
        )
        self.conn.commit()

    def log_alarm(self, timestamp: float, alarm_type: str, message: str) -> None:
        self.conn.execute(
            "INSERT INTO alarms(timestamp, alarm_type, message) VALUES (?,?,?)",
            (timestamp, alarm_type, message),
        )
        self.conn.commit()

    def log_state_change(
        self, timestamp: float, kind: str, old: str, new: str, message: str = ""
    ) -> None:
        self.conn.execute(
            "INSERT INTO state_changes(timestamp, kind, old_value, new_value, message) "
            "VALUES (?,?,?,?,?)",
            (timestamp, kind, old, new, message),
        )
        self.conn.commit()

    def stats_summary(self) -> Dict[str, Any]:
        cur = self.conn.cursor()
        drops = cur.execute("SELECT COUNT(*), MIN(timestamp), MAX(timestamp) FROM drop_events").fetchone()
        levels = cur.execute(
            "SELECT MIN(level_percent), MAX(level_percent) FROM level_measurements "
            "WHERE level_percent IS NOT NULL"
        ).fetchone()
        empty_row = cur.execute(
            "SELECT timestamp FROM state_changes WHERE kind='level_state' AND new_value='EMPTY' "
            "ORDER BY timestamp ASC LIMIT 1"
        ).fetchone()
        n_drops = int(drops[0] or 0)
        t0, t1 = drops[1], drops[2]
        duration_m = 0.0
        if t0 is not None and t1 is not None and t1 > t0:
            duration_m = (t1 - t0) / 60.0
        avg_dpm = (n_drops / duration_m) if duration_m > 0 else float(n_drops)
        return {
            "total_drops": n_drops,
            "avg_drops_per_min": avg_dpm,
            "min_level": levels[0],
            "max_level": levels[1],
            "empty_detected": empty_row is not None,
            "empty_time": empty_row[0] if empty_row else None,
            "first_drop_ts": t0,
            "last_drop_ts": t1,
        }

    def close(self) -> None:
        self.conn.close()
