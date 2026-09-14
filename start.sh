#!/usr/bin/env bash
# Spectr Vision — egui UI (level + drops + radio + training)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# If linked from ~/start-silo → py/start.sh, go up to silo-alert
if [[ ! -f "$ROOT/Cargo.toml" ]]; then
  ROOT="$(cd "$(dirname "$0")" && pwd)"
fi
if [[ -f /home/spectr/silo-alert/Cargo.toml ]]; then
  ROOT=/home/spectr/silo-alert
fi
cd "$ROOT"

BIN="$ROOT/target/release/silo-alert"
if [[ ! -x "$BIN" ]]; then
  echo "Building Spectr Vision (first time may take a while)…"
  cargo build --release
fi

exec "$BIN" "$@"
