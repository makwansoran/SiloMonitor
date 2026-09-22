# Deploy to the Pi (Ethernet camera)

The site camera is RTSP only. The Pi needs `ffmpeg` and a working desktop session.

```bash
cd /home/makwan/Development/Veolia/SiloMonitor
chmod +x scripts/deploy_pi.sh
./scripts/deploy_pi.sh spectr@spectr.local
```

Or manually:

1. Supabase SQL Editor → run `sql/supabase_silo.sql` (creates `silo_events` + `silo_allowed_sites`).
2. Copy `.env` (set `SUPABASE_KEY`) and set `camera.rtsp_url` in `config.yaml`.
3. On the Pi:

```bash
sudo apt install -y ffmpeg
scp .env spectr@spectr.local:~/silo-alert/.env
# on Pi:
cd ~/silo-alert && source ~/.cargo/env && cargo build --release
sudo cp scripts/spectr-vision.service /etc/systemd/system/spectr-vision.service
sudo systemctl daemon-reload
sudo systemctl enable --now spectr-vision.service
```

Confirm log: `supabase: enabled site=spectr-pi` and `camera: rtsp …`.
No `/dev/video0` is required.

```bash
sudo apt install -y ffmpeg sox
```

Set the Peltor LiteCom Pro III headset and SA828 to the **same analog channel** (Config → Radio). If the headset uses a privacy/CTCSS tone, set the same CTCSS in the app before Program module.

**UART TX/RX** (GPIO14/15): only needed to **Program / Read** the module. Voice alerts need **PTT + audio jack → MIC**, not UART.
