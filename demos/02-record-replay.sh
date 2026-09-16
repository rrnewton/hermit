#!/usr/bin/env bash
#
# Demo 2: record and replay.
#
# Record an execution into an isolated data directory, inspect the recording,
# and replay it to completion -- with and without GDB. Keep the recording
# directory, executable, inputs, and Hermit revision unchanged between recording
# and replay.

set -euo pipefail

# shellcheck source=demos/lib/display.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/display.sh"

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 2: Record And Replay"
demo_header "$DEMO_LABEL"
echo 'Hermit records an execution into an isolated data directory, lists the recording'
echo 'in text and JSON, and replays it to completion with --autopilot. It can also'
echo 'record and immediately verify a replay. Without --autopilot, hermit replay'
echo 'starts a replay gdbserver and GDB client; the demo drives a noninteractive GDB'
echo 'session that continues the guest to completion. Keep the recording directory,'
echo 'executable, inputs, and Hermit revision unchanged between recording and replay.'
echo ''
echo '=========================================='

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# This demo exercises the RELEASE binary, which is common.sh's default and the
# profile every other demo uses. It previously substituted the debug build,
# because release replay failed EFAULT while replaying the bootstrap exec. That
# defect is fixed: the root cause was a 4 KiB clone stack overflowing during
# release replay mount setup, repaired in reverie by
# ce841d744cc74b1627ac52b42f711b33b1c72a45, which this change's pin now
# includes. The invalid envp and the EFAULT were symptoms of memory corruption a
# layer down, which is also why only the release profile showed them -- the
# margin was never there and debug happened to fit.
#
# No binary selection or existence check belongs here. common.sh already builds
# the release binary, exports HERMIT to it, and exits with a clear message when
# it is missing; a second copy of that logic is how the substitution persisted.
echo "Hermit record/replay binary: $HERMIT"

export DEMO_DATA_DIR="$DEMO_TMP/recordings"
mkdir -p "$DEMO_DATA_DIR"

demo_banner "Record /bin/echo, list the recording, and replay it"
safehermit --log=error record start \
  --data-dir="$DEMO_DATA_DIR" -- /bin/echo recorded
safehermit record list --data-dir="$DEMO_DATA_DIR"
safehermit record list --json --data-dir="$DEMO_DATA_DIR"
safehermit --log=error replay --autopilot --data-dir="$DEMO_DATA_DIR"

demo_banner "Record and immediately verify a replay (temp recording auto-deleted)"
# --verify compares the deterministic execution log, which is empty at
# --log=error; hermit therefore requires --log=info (or more verbose) here.
safehermit --log=info record start --verify \
  --data-dir="$DEMO_TMP/verified-recording" -- /bin/echo verified-recording

demo_banner "Replay under GDB (noninteractive: continue to completion)"
# Without --autopilot, replay starts a replay gdbserver and GDB client. This
# noninteractive session connects, continues the guest, and quits after
# /bin/echo completes. The trailing --gdbex=quit is required: once the guest
# exits, GDB has no more -ex commands to run and would otherwise drop to its
# interactive prompt and block on stdin, so `hermit replay` (which waits on the
# GDB client) would hang until the external timeout killed it. For interactive
# debugging, omit the --gdbex options and the external timeout.
timeout 90 "$SAFEHERMIT" "$HERMIT" --log=error replay \
  --data-dir="$DEMO_DATA_DIR" \
  --gdbex='set confirm off' \
  --gdbex='set pagination off' \
  --gdbex=continue \
  --gdbex=quit

demo_success
