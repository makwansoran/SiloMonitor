#!/usr/bin/env bash
# Program a reset SA828 (receiver test radio) over the NiceRF USB-TTL stick.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

echo
echo "SET cap ON  = program only (module is deaf, will not receive)"
echo "SET cap OFF = run / listen"
echo
echo "Wire the stick: VCC→22, GND→GND, TXD→RXD, RXD→TXD. CS open."
echo
echo ">>> Put the SET cap ON now."
read -r -p "SET cap is ON? Press Enter to factory-reset, then program 446.0062 MHz squelch 1..."

sudo python3 "$ROOT/py/program_sa828.py" --factory
sudo python3 "$ROOT/py/program_sa828.py" --freq 446.0062 --squelch 1
sudo python3 "$ROOT/py/program_sa828.py" --read

echo
echo ">>> Take the SET cap OFF now."
echo "Receiver is on 446.0062 squelch 1. Silent until the sender transmits."
echo
echo "Optional speaker check: SET ON, then:"
echo "  sudo python3 py/program_sa828.py --freq 446.0062 --squelch 0"
echo "  SET OFF → hiss. Then SET ON, run this script again (squelch 1), SET OFF."
echo
