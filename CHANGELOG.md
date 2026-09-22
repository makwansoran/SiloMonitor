# Changelog

Every change in this pass. Same product: empty-reference match → confirm → SA828 → log.

## Radio

- **peltor-channels** — `src/peltor.rs`, Config Radio, `src/sa828.rs`, `config.yaml`, `tools/program_sa828.py`. Channel picker for Peltor LiteCom Pro III analog PMR446 Ch 1–16. CTCSS Off + 38 tones (default Off). Program writes matching TX/RX tone to SA828. Digital DMR channels are not used (analog module only).
- **tx-volume** — Config Radio slider for PCM dB into the SA828 mic (default −28). Was a hardcoded constant.
- **ptt-open-drain** — `src/silo_alert.rs`. PTT idle is GPIO INPUT (High-Z), TX is OUTPUT LOW. Never drive 3.3 V into SA828. Default pin GPIO23 (header 16).
- **ptt-once** — Radio TX only when the silo *becomes* empty (PTT + WAV once). No 90s re-announce while still empty. Config → Test radio still works when disarmed.

## P0

- **rtsp-only** — `src/camera.rs`, `src/config.rs`, `config.yaml`, `Cargo.toml`, Config Camera UI, `scripts/spectr-vision.service`. Site camera is Ethernet only. USB/nokhwa gone. systemd waits for network + desktop, not `/dev/video0`. Operator sees only an RTSP URL.
- **live-faults** — `src/main.rs`. Live/Stats hero is CAMERA FAULT / RADIO FAULT / DISARMED / PAUSED / EMPTY / OK. A dead camera never looks Empty or Full. Alerts freeze until the stream returns and empty is confirmed again.
- **radio-restart** — `src/silo_alert.rs`, `src/main.rs`. Restart while already empty does not re-blast the intercom. Repeat uses the saved alert time. Audio device is configurable; TX fail lights RADIO FAULT.
- **empty-bar** — `src/main.rs`, Config Detection. Empty must last ≥15s and ≥2 checks in a row. Marking a region rebuilds the reference. Live has Clear.
- **cloud-sparse** — `src/supabase.rs`, `sql/supabase_silo.sql`, `.env.example`. Cloud gets alerts, heartbeats, and faults — not a row every check. RLS only accepts known `site_id`. Example env has no real key.

## P1

- **vision-honest** — `src/vision.rs`, Model/Stats copy. Match is lighting-normalized. B&W uses the reference threshold, not a new Otsu every frame. Full samples show false-empty risk. No fake “held-out accuracy”.
- **arm-radio-ui** — `src/stats.rs`, Live sidebar. Boot is disarmed until Arm. Pause is remembered. Radio strip shows last TX and the 90s countdown. Reconnect uses the URL on screen.

## P2

- **kiosk-wizard** — Live overlay, windowed (not fullscreen), 10 Hz UI, alert evidence in `data/alerts/`, MUTE badge, honest 24h chart labels, deploy docs for RTSP + ffmpeg.
