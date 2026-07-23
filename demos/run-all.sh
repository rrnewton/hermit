#!/usr/bin/env bash
#
# Run the portable demos (1-3) in order under a single shared scratch directory.
# Demo 4 requires user-accessible CPU performance counters and demo 5 requires
# QEMU plus git-ignored Linux boot images, so both are opt-in.

set -euo pipefail

demo_suite_failure() {
  local rc=$?
  printf '\n=== Demo suite: FAILURE (exit %d) — see errors above ===\n' "$rc" >&2
}
trap demo_suite_failure ERR

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

with_analyze=0
with_qemu=0
for arg in "$@"; do
  case "$arg" in
    --with-analyze) with_analyze=1 ;;
    --with-qemu) with_qemu=1 ;;
    --all) with_analyze=1; with_qemu=1 ;;
    *)
      echo "usage: $0 [--with-analyze] [--with-qemu] [--all]" >&2
      exit 2
      ;;
  esac
done

# Build once and share one scratch directory across the demos.
export DEMO_TMP="${DEMO_TMP:-$(mktemp -d -t hermit-demo.XXXXXX)}"

bash "$DEMO_DIR/01-deterministic-run.sh"
export DEMO_SKIP_BUILD=1
bash "$DEMO_DIR/02-record-replay.sh"
bash "$DEMO_DIR/03-chaos-concurrency.sh"

if [ "$with_analyze" -eq 1 ]; then
  bash "$DEMO_DIR/04-schedule-bisection.sh"
fi

if [ "$with_qemu" -eq 1 ]; then
  bash "$DEMO_DIR/05-qemu-boot.sh"
fi

printf '\n=== Demo suite: SUCCESS — all requested demos passed ===\n'
