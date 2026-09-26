# Changelog

Every change in this pass. Same product: empty-reference match → confirm → SA828 → log.

## NVR / camera

- **hikvision-config** — `src/config.rs`, `config.yaml`, Config Camera UI, `src/camera.rs`, `DEPLOY_LATER.md`. Easy NVR fields (host, user, password, channel 1–16, sub/main) compose Hikvision `…/Streaming/Channels/<id>`. Optional custom RTSP URL. Password masked in UI; sub-stream default.
- **cloud-frames** — `src/supabase.rs`, `src/vision.rs`, `src/main.rs`, `sql/supabase_silo.sql`. Uploads `{site}/latest.jpg` to Storage bucket `silo-frames` on the check interval; alert evidence to `{site}/alerts/{unix}.jpg` for the later cloud app.

## Radio

- **sa828-verify** — `src/sa828.rs`, Config Radio. One UART session, blocking write-all + flush; program succeeds only if channel-1 TX/RX readback matches requested freq (±100 Hz). Channel/CTCSS UI rolls back on failure (no fake “saved”).
- **spken-csma** — `src/silo_alert.rs`, Config Radio. Optional SPKEN GPIO: high=busy, low=free. Wait (debounce + 30s cap) before PTT so alerts do not talk over an occupied channel.
- **peltor-channels** — `src/peltor.rs`, Config Radio, `src/sa828.rs`, `config.yaml`, `tools/program_sa828.py`. Channel picker for Peltor LiteCom Pro III analog PMR446 Ch 1–16. CTCSS Off + 38 tones (default Off). Program writes matching TX/RX tone to SA828. Digital DMR channels are not used (analog module only).
- **tx-volume** — Removed. App no longer calls amixer/sox gain. Operator sets Pi system volume only; Test radio / alerts play the WAV as-is.
- **ptt-once** — Radio TX only on the empty-*confirmed* edge (PTT + WAV once). Sticky latch until vision sees full again. Test radio: PTT LOW → WAV once → High-Z.
- **ptt-open-drain** — `src/silo_alert.rs`. True open-drain on **GPIO23 only**: TX = OUTPUT LOW, idle = INPUT High-Z. Never touch GPIO17. Never OUTPUT HIGH / pull-up.

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
