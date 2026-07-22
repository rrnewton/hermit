#!/usr/bin/env bash
#
# Demo 2: record and replay.
#
# Record an execution into an isolated data directory, inspect the recording,
# and replay it to completion -- with and without GDB. Keep the recording
# directory, executable, inputs, and Hermit revision unchanged between recording
# and replay.

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

export DEMO_DATA_DIR="$DEMO_TMP/recordings"
mkdir -p "$DEMO_DATA_DIR"

demo_banner "Record /bin/echo, list the recording, and replay it"
"$HERMIT" --log=error record start \
  --data-dir="$DEMO_DATA_DIR" -- /bin/echo recorded
"$HERMIT" record list --data-dir="$DEMO_DATA_DIR"
"$HERMIT" record list --json --data-dir="$DEMO_DATA_DIR"
"$HERMIT" --log=error replay --autopilot --data-dir="$DEMO_DATA_DIR"

demo_banner "Record and immediately verify a replay (temp recording auto-deleted)"
"$HERMIT" --log=error record start --verify \
  --data-dir="$DEMO_TMP/verified-recording" -- /bin/echo verified-recording

demo_banner "Replay under GDB (noninteractive: continue to completion)"
# Without --autopilot, replay starts a replay gdbserver and GDB client. This
# noninteractive session connects, continues the guest, and exits after
# /bin/echo completes. For interactive debugging, omit the --gdbex options and
# the external timeout.
timeout 90 "$HERMIT" --log=error replay \
  --data-dir="$DEMO_DATA_DIR" \
  --gdbex='set confirm off' \
  --gdbex='set pagination off' \
  --gdbex=continue

echo
echo "Demo 2 complete."
