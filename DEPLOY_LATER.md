# Deploy later (after the current change batch)

When ready, either:

```bash
cd /home/makwan/Development/Veolia/SiloMonitor
chmod +x scripts/deploy_pi.sh
./scripts/deploy_pi.sh spectr@spectr.local
```

Or manually:

1. Supabase SQL Editor → run `sql/supabase_silo.sql` (drops old tables, creates `silo_events`).
2. On the Pi after syncing code:

```bash
scp .env spectr@spectr.local:~/silo-alert/.env
# on Pi:
cd ~/silo-alert && source ~/.cargo/env && cargo build --release
pkill silo-alert || true
export DISPLAY=:0 XDG_RUNTIME_DIR=/run/user/1000
./start.sh
```

Confirm log: `supabase: enabled site=spectr-pi`
