#!/usr/bin/env python3
"""Upload colour + B&W reference JPEGs to silo-frames (no table required)."""
from __future__ import annotations

import os
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SITE = "spectr-pi"
BUCKET = "silo-frames"


def load_env():
    env = {}
    for line in (ROOT / ".env").read_text().splitlines():
        if "=" in line and not line.strip().startswith("#"):
            k, v = line.split("=", 1)
            env[k.strip()] = v.strip().strip('"').strip("'")
    env.update({k: v for k, v in os.environ.items() if k.startswith("SUPABASE_")})
    return env


def put(base, key, path, body):
    url = f"{base}/storage/v1/object/{BUCKET}/{path}"
    req = urllib.request.Request(
        url,
        data=body,
        method="PUT",
        headers={
            "apikey": key,
            "Authorization": f"Bearer {key}",
            "Content-Type": "image/jpeg",
            "x-upsert": "true",
        },
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        return resp.status


def main():
    env = load_env()
    base = env["SUPABASE_URL"].rstrip("/")
    key = env["SUPABASE_KEY"]
    n = 0
    for label, folder in [("empty", ROOT / "data/empty"), ("full", ROOT / "data/full")]:
        for path in sorted(folder.glob("*.jpg")):
            dest = f"{SITE}/{label}/{path.name}"
            put(base, key, dest, path.read_bytes())
            n += 1
            print("colour", dest)
            bw = ROOT / "data/empty_bw" / path.name
            if bw.is_file():
                bdest = f"{SITE}/empty_bw/{path.name}"
                put(base, key, bdest, bw.read_bytes())
                n += 1
                print("bw    ", bdest)
    print(f"uploaded {n} objects")


if __name__ == "__main__":
    main()
