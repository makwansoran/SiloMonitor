# Deploy to the Pi (Hikvision NVR → RTSP)

The site camera is RTSP only (Ethernet). The Pi needs `ffmpeg` and a working desktop session.

## Site checklist (before software)

### Network

1. Put the Pi on the **same LAN** as the Hikvision NVR (Ethernet preferred).
2. From the Pi, confirm reachability:

```bash
ping -c 3 <NVR_IP>
# RTSP default port:
nc -vz <NVR_IP> 554
```

3. Prefer a DHCP reservation / static IP for the NVR.

### Installer credentials

Ask the company that installed the NVR for:

- NVR LAN IP (and RTSP port if not **554**)
- Login — prefer a **new view-only** user for the Pi (not admin)
- Which channel number (1–16) looks at the silo

Do **not** commit passwords to git. Enter them only in Config → Camera on the Pi (local `config.yaml`).

### Prove the stream (before the app)

Hikvision DS-76xx URL pattern:

```text
rtsp://USER:PASS@NVR_IP:554/Streaming/Channels/<channel><01|02>
```

- `01` = main, `02` = sub (use **sub** for SiloMonitor)
- Channel 3 sub → `…/Channels/302`

```bash
sudo apt install -y ffmpeg
ffplay -rtsp_transport tcp "rtsp://USER:PASS@NVR_IP:554/Streaming/Channels/CHANNEL02"

# or one frame:
ffmpeg -rtsp_transport tcp -i "rtsp://USER:PASS@NVR_IP:554/Streaming/Channels/CHANNEL02" \
  -frames:v 1 -y /tmp/silo_test.jpg
```

## Deploy app

```bash
cd /home/makwan/Development/Veolia/SiloMonitor
chmod +x scripts/deploy_pi.sh
./scripts/deploy_pi.sh spectr@spectr.local
```

Or manually:

1. Supabase SQL Editor → run `sql/supabase_silo.sql` (events + **silo-frames** Storage bucket).
2. Copy `.env` (set `SUPABASE_KEY`).
3. On the Pi:

```bash
sudo apt install -y ffmpeg sox
scp .env spectr@spectr.local:~/silo-alert/.env
# on Pi:
cd ~/silo-alert && source ~/.cargo/env && cargo build --release
sudo cp scripts/spectr-vision.service /etc/systemd/system/spectr-vision.service
sudo systemctl daemon-reload
sudo systemctl enable --now spectr-vision.service
```

4. Config → Camera: enter NVR host, username, password, channel, Sub stream → **Reconnect camera**.

Confirm log: `supabase: enabled site=spectr-pi` and `camera: rtsp …`.
No `/dev/video0` is required.

### Cloud stills (later app)

While the camera is healthy, the Pi uploads `{site_id}/latest.jpg` to the `silo-frames` Storage bucket about every check interval. Empty alerts also upload `{site_id}/alerts/{unix}.jpg`. The later cloud software should fetch those objects over HTTPS (not RTSP through Supabase).

## Radio

Set the Peltor LiteCom Pro III headset and SA828 to the **same analog channel** (Config → Radio). If the headset uses a privacy/CTCSS tone, set the same CTCSS in the app before Program module.

**UART TX/RX** (GPIO14/15): only needed to **Program / Read** the module. Voice alerts need **PTT + audio jack → MIC**, not UART.
