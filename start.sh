#!/usr/bin/env bash
# Industrial Smart System — start the desktop app, building it first if needed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

# Cursor/agent sandboxes may set CARGO_TARGET_DIR elsewhere; always build into
# this repo's target/ so we exec the binary we just compiled.
unset CARGO_TARGET_DIR

BIN="$ROOT/target/release/silo-alert"
if [[ ! -x "$BIN" ]] || find src Cargo.toml -newer "$BIN" | grep -q .; then
  echo "Building Industrial Smart System…"
  cargo build --release
fi

# The UI needs a display; the desktop session usually provides these already.
export DISPLAY="${DISPLAY:-:0}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

exec "$BIN" "$@"
