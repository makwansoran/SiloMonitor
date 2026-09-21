#!/usr/bin/env python3
"""Program a NiceRF SA828 over the USB-TTL stick (SET ON). Not Pi GPIO UART."""

from __future__ import annotations

import argparse
import glob
import os
import sys
import time

try:
    import serial
except ImportError:
    sys.exit("Need pyserial:  pip install pyserial")

DEFAULT_FREQ = "446.0062"


def normalize(raw: str) -> str:
    f = float(raw.strip().replace(",", "."))
    if not 400.0 <= f <= 480.0:
        raise SystemExit("UHF only: 400–480 MHz")
    return f"{f:.4f}"


def payload(freq: str, squelch: int, ctcss: int = 0) -> str:
    if ctcss not in range(39):
        raise SystemExit("CTCSS must be 0–38 (0=Off)")
    pairs = ",".join(f"{freq},{freq}" for _ in range(16))
    return f"{pairs},{ctcss:03d},{ctcss:03d},{squelch}"


def detect_port() -> str:
    for p in (
        "/dev/ttyUSB0",
        "/dev/ttyUSB1",
        "/dev/ttyACM0",
        *sorted(glob.glob("/dev/ttyUSB*")),
        *sorted(glob.glob("/dev/ttyACM*")),
    ):
        if os.path.exists(p):
            return p
    raise SystemExit("No USB serial port. Plug in the USB stick.")


def open_port(port: str):
    try:
        return serial.Serial(port, 9600, timeout=2)
    except PermissionError:
        sys.exit(f"Permission denied on {port}. Run:  sudo python3 py/program_sa828.py --read")


def chat(ser, data: bytes, wait: float) -> str:
    ser.reset_input_buffer()
    ser.write(data)
    ser.flush()
    time.sleep(wait)
    return ser.read(512).decode("ascii", errors="replace")


def require_reply(label: str, reply: str) -> str:
    if not reply.strip():
        sys.exit(
            f"No reply to {label}. Check USB TXD→SA828 RXD, RXD→TXD, SET jumper ON, then retry."
        )
    if "ERROR" in reply.upper():
        sys.exit(f"SA828 rejected {label}: {reply!r}")
    return reply


def read_config(ser) -> str:
    require_reply("version", chat(ser, b"AAFAA\r\n", 0.6))
    return require_reply("read", chat(ser, b"AAFA1\r\n", 0.8))


def parse_ch1(raw: str) -> str:
    line = raw.replace("OK", "").replace("\r", " ").replace("\n", " ").strip()
    if line.upper().startswith("AA"):
        line = line[2:]
    parts = [p.strip() for p in line.split(",") if p.strip()]
    if len(parts) < 2:
        return raw.strip()
    return f"TX {parts[0]} / RX {parts[1]}"


def factory(port: str) -> None:
    ser = open_port(port)
    try:
        require_reply("version", chat(ser, b"AAFAA\r\n", 0.6))
        require_reply("factory", chat(ser, b"AAFA2\r\n", 1.2))
        raw = read_config(ser)
    finally:
        ser.close()
    print(f"Factory defaults restored  ({port})")
    print(f"  channel 1: {parse_ch1(raw)}")
    print(raw.strip())


def program(port: str, freq: str, squelch: int, ctcss: int = 0) -> None:
    freq = normalize(freq)
    if squelch not in range(9):
        raise SystemExit("Squelch must be 0–8")
    if ctcss not in range(39):
        raise SystemExit("CTCSS must be 0–38 (0=Off)")
    ser = open_port(port)
    try:
        read_config(ser)
        require_reply(
            "set",
            chat(ser, f"AAFA3{payload(freq, squelch, ctcss)}\r\n".encode("ascii"), 2.5),
        )
        raw = read_config(ser)
    finally:
        ser.close()
    print(f"Module programmed  ({port})")
    print(f"  requested: {freq} MHz  squelch {squelch}  CTCSS {ctcss}")
    print(f"  channel 1: {parse_ch1(raw)}")
    print("Take the SET cap OFF to run / listen.")
    if squelch == 0:
        print("Squelch 0: speaker should hiss. Then program again with --squelch 1.")


def read(port: str) -> None:
    ser = open_port(port)
    try:
        raw = read_config(ser)
    finally:
        ser.close()
    print(f"Channel 1: {parse_ch1(raw)}")
    print(raw.strip())


def loopback(port: str) -> None:
    ser = open_port(port)
    try:
        ser.reset_input_buffer()
        ser.write(b"PING\r\n")
        ser.flush()
        time.sleep(0.2)
        got = ser.read(32)
    finally:
        ser.close()
    if b"PING" in got:
        print("USB stick is fine. Fault is the wires to the SA828.")
    else:
        print("No echo. Unplug TXD/RXD from the radio, join USB TXD to USB RXD, run --loopback again.")
        print(f"got: {got!r}")


def probe(port: str) -> None:
    cmds = [
        b"AAFAA\r\n",
        b"AAFA1\r\n",
        bytes([0x41, 0x41, 0x46, 0x41, 0x41, 0x0D, 0x0A]),
        b"AT+DMOCONNECT\r\n",
        b"AT+DMOCONNECT\r",
    ]
    any_hit = False
    for baud in (9600, 19200, 115200):
        ser = serial.Serial(port, baud, timeout=0.6)
        try:
            for dtr in (False, True):
                ser.dtr = dtr
                ser.rts = dtr
                time.sleep(0.05)
                for cmd in cmds:
                    ser.reset_input_buffer()
                    ser.write(cmd)
                    ser.flush()
                    time.sleep(0.35)
                    got = ser.read(256)
                    if got:
                        any_hit = True
                        print(f"baud={baud} dtr={dtr} cmd={cmd!r}")
                        print(f"  reply={got!r}")
        finally:
            ser.close()
    if not any_hit:
        sys.exit("Stick opened, radio silent. Cross TXD/RXD and put SET on the stick.")


def main() -> None:
    p = argparse.ArgumentParser(description="Set SA828 TX/RX to one UHF frequency on all channels")
    p.add_argument("--port", help="USB serial device (default: first ttyUSB/ttyACM)")
    p.add_argument("--freq", default=DEFAULT_FREQ, help="MHz, e.g. 446.0062")
    p.add_argument("--squelch", type=int, default=1, help="0=always hiss (test speaker), 1–8=normal")
    p.add_argument(
        "--ctcss",
        type=int,
        default=0,
        help="CTCSS 0=Off, 1–38 = tone (same TX/RX)",
    )
    p.add_argument("--read", action="store_true", help="Read current frequency only")
    p.add_argument(
        "--factory",
        action="store_true",
        help="Restore SA828 factory defaults (AAFA2), then stop",
    )
    p.add_argument("--loopback", action="store_true", help="Echo-test the USB stick")
    p.add_argument("--probe", action="store_true", help="Try several baud rates and command formats")
    args = p.parse_args()
    port = args.port or detect_port()
    if args.loopback:
        loopback(port)
    elif args.probe:
        probe(port)
    elif args.factory:
        factory(port)
    elif args.read:
        read(port)
    else:
        program(port, args.freq, args.squelch, args.ctcss)


if __name__ == "__main__":
    main()
