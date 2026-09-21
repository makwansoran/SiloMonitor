#!/usr/bin/env bash
# Spectr Vision — start the desktop app, building it first if needed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

BIN="$ROOT/target/release/silo-alert"
if [[ ! -x "$BIN" ]] || find src Cargo.toml -newer "$BIN" | grep -q .; then
  echo "Building Spectr Vision…"
  cargo build --release
fi

# The UI needs a display; the desktop session usually provides these already.
export DISPLAY="${DISPLAY:-:0}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

exec "$BIN" "$@"
