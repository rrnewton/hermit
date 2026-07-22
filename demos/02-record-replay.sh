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
# noninteractive session connects, continues the guest, and quits after
# /bin/echo completes. The trailing --gdbex=quit is required: once the guest
# exits, GDB has no more -ex commands to run and would otherwise drop to its
# interactive prompt and block on stdin, so `hermit replay` (which waits on the
# GDB client) would hang until the external timeout killed it. For interactive
# debugging, omit the --gdbex options and the external timeout.
timeout 90 "$HERMIT" --log=error replay \
  --data-dir="$DEMO_DATA_DIR" \
  --gdbex='set confirm off' \
  --gdbex='set pagination off' \
  --gdbex=continue \
  --gdbex=quit

echo
echo "Demo 2 complete."
