#!/usr/bin/env bash
# One-shot later deploy: .env → Pi, sync sources, cargo build, restart UI.
# Usage: ./scripts/deploy_pi.sh [spectr@spectr.local]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${1:-spectr@spectr.local}"

echo "==> Reminder: run sql/supabase_silo.sql in Supabase SQL Editor if not done yet"
echo "    File: $ROOT/sql/supabase_silo.sql"
echo

if [[ ! -f "$ROOT/.env" ]]; then
  echo "Missing $ROOT/.env — copy from .env.example and set SUPABASE_KEY"
  exit 1
fi

echo "==> Copy .env to $HOST:~/silo-alert/.env"
scp -o StrictHostKeyChecking=accept-new "$ROOT/.env" "$HOST:~/silo-alert/.env"

echo "==> Sync app sources"
rsync -az --delete \
  --exclude target --exclude .git --exclude data --exclude py/.venv \
  "$ROOT/" "$HOST:~/silo-alert/"

echo "==> Build + install service on Pi"
ssh -o StrictHostKeyChecking=accept-new "$HOST" bash -s <<'REMOTE'
set -euo pipefail
cd ~/silo-alert
source "$HOME/.cargo/env"
cargo build --release
mkdir -p data

# Autostart + watchdog: survives reboots and crashes, 24/7.
sudo cp scripts/spectr-vision.service /etc/systemd/system/spectr-vision.service
sudo systemctl daemon-reload
sudo systemctl enable spectr-vision.service
sudo systemctl restart spectr-vision.service
sleep 3
systemctl is-active spectr-vision.service
systemctl --no-pager -l status spectr-vision.service | head -15
REMOTE

echo "==> Deploy finished. Check Supabase Table Editor → silo_events after the app runs."
echo "    Logs:    ssh $HOST 'tail -f ~/silo-alert/data/silo.log'"
echo "    Service: ssh $HOST 'journalctl -u spectr-vision -f'"
