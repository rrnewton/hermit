#!/usr/bin/env bash
#
# Run the portable demos (1-3) in order under a single shared scratch directory.
# Demo 4 (schedule bisection) is not included by default because it requires
# user-accessible CPU performance counters and can run for several minutes; pass
# --with-analyze to include it.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Build once and share one scratch directory across the demos.
export DEMO_TMP="${DEMO_TMP:-$(mktemp -d -t hermit-demo.XXXXXX)}"

bash "$DEMO_DIR/01-deterministic-run.sh"
export DEMO_SKIP_BUILD=1
bash "$DEMO_DIR/02-record-replay.sh"
bash "$DEMO_DIR/03-chaos-concurrency.sh"

if [ "${1:-}" = "--with-analyze" ]; then
  bash "$DEMO_DIR/04-schedule-bisection.sh"
fi

echo
echo "All requested demos completed."
