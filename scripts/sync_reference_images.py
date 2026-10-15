#!/usr/bin/env python3
"""Upload all local reference photos + metadata to Supabase.

Requires silo_reference_images (run sql/silo_reference_images.sql once).
Reads SUPABASE_URL / SUPABASE_KEY from ../.env
"""
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EMPTY = ROOT / "data" / "empty"
FULL = ROOT / "data" / "full"
BW = ROOT / "data" / "empty_bw"
SITE = "spectr-pi"
BUCKET = "silo-frames"


def load_env() -> dict[str, str]:
    env = {}
    path = ROOT / ".env"
    if path.exists():
        for line in path.read_text().splitlines():
            if "=" in line and not line.strip().startswith("#"):
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip().strip('"').strip("'")
    env.update({k: v for k, v in os.environ.items() if k.startswith("SUPABASE_")})
    return env


def req(method: str, url: str, key: str, data: bytes | None = None, headers: dict | None = None):
    h = {
        "apikey": key,
        "Authorization": f"Bearer {key}",
    }
    if headers:
        h.update(headers)
    r = urllib.request.Request(url, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(r, timeout=60) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def jpeg_size(data: bytes) -> tuple[int, int] | None:
    if len(data) < 4 or data[0:2] != b"\xff\xd8":
        return None
    i = 2
    while i + 9 < len(data):
        if data[i] != 0xFF:
            i += 1
            continue
        marker = data[i + 1]
        i += 2
        if marker in (0xD8, 0xD9):
            continue
        if i + 2 > len(data):
            break
        seglen = int.from_bytes(data[i : i + 2], "big")
        if seglen < 2 or i + seglen > len(data):
            break
        if marker in (0xC0, 0xC2) and seglen >= 7:
            h = int.from_bytes(data[i + 3 : i + 5], "big")
            w = int.from_bytes(data[i + 5 : i + 7], "big")
            return w, h
        i += seglen
    return None


def put_object(base: str, key: str, path: str, body: bytes) -> None:
    url = f"{base}/storage/v1/object/{BUCKET}/{path}"
    status, raw = req(
        "PUT",
        url,
        key,
        body,
        {"Content-Type": "image/jpeg", "x-upsert": "true"},
    )
    if not (200 <= status < 300):
        raise RuntimeError(f"storage {path}: HTTP {status} {raw[:120]!r}")


def delete_object(base: str, key: str, path: str) -> None:
    url = f"{base}/storage/v1/object/{BUCKET}/{path}"
    req("DELETE", url, key)


def upsert_row(base: str, key: str, row: dict) -> None:
    url = f"{base}/rest/v1/silo_reference_images?on_conflict=site_id,file_name"
    body = json.dumps(row).encode()
    status, raw = req(
        "POST",
        url,
        key,
        body,
        {
            "Content-Type": "application/json",
            "Prefer": "resolution=merge-duplicates,return=minimal",
        },
    )
    if status == 404 or b"PGRST205" in raw or b"schema cache" in raw:
        raise SystemExit(
            "Table silo_reference_images is missing.\n"
            "Open https://supabase.com/dashboard/project/mranicltlsnjurjqwjpv/sql/new\n"
            "Paste and run: sql/silo_reference_images.sql\n"
            "Then re-run this script."
        )
    if not (200 <= status < 300):
        raise RuntimeError(f"upsert {row['file_name']}: HTTP {status} {raw[:160]!r}")


def samples() -> list[tuple[Path, str]]:
    out: list[tuple[Path, str]] = []
    for p in sorted(EMPTY.glob("*.jpg")):
        out.append((p, "empty"))
    for p in sorted(FULL.glob("*.jpg")):
        out.append((p, "not_empty"))
    out.sort(key=lambda t: t[0].name, reverse=True)
    return out


def main() -> int:
    env = load_env()
    base = env.get("SUPABASE_URL", "").rstrip("/")
    key = env.get("SUPABASE_KEY", "")
    if not base or not key:
        print("SUPABASE_URL / SUPABASE_KEY missing", file=sys.stderr)
        return 1

    # Fail fast if table missing.
    status, raw = req(
        "GET",
        f"{base}/rest/v1/silo_reference_images?select=id&limit=1",
        key,
    )
    if status == 404 or b"PGRST205" in raw:
        print(
            "Table silo_reference_images is missing.\n"
            "Run sql/silo_reference_images.sql in the Supabase SQL Editor, then re-run.\n"
            "https://supabase.com/dashboard/project/mranicltlsnjurjqwjpv/sql/new",
            file=sys.stderr,
        )
        return 2

    items = samples()
    print(f"syncing {len(items)} reference photos for site={SITE}")
    now = datetime.now(timezone.utc).isoformat()
    ok = 0
    for path, label in items:
        name = path.name
        colour_folder = "empty" if label == "empty" else "full"
        colour_path = f"{SITE}/{colour_folder}/{name}"
        bw_path = f"{SITE}/empty_bw/{name}"
        colour = path.read_bytes()
        size = jpeg_size(colour) or (0, 0)
        put_object(base, key, colour_path, colour)
        other = f"{SITE}/{'full' if label == 'empty' else 'empty'}/{name}"
        delete_object(base, key, other)
        bw_storage = None
        bw_file = BW / name
        if bw_file.is_file():
            put_object(base, key, bw_path, bw_file.read_bytes())
            bw_storage = bw_path
        captured = None
        stem = name.rsplit(".", 1)[0]
        if stem.isdigit():
            captured = int(stem)
        row = {
            "site_id": SITE,
            "file_name": name,
            "label": label,
            "captured_ms": captured,
            "bytes": path.stat().st_size,
            "width": size[0] or None,
            "height": size[1] or None,
            "colour_path": colour_path,
            "bw_path": bw_storage,
            "updated_at": now,
        }
        upsert_row(base, key, row)
        ok += 1
        print(f"  {ok:02d}/{len(items)}  {label:9}  {name}")
    print(f"done — {ok} rows in silo_reference_images")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
