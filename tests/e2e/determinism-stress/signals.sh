#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

known_gaps_ok=false
if [[ ${1:-} == --known-gaps-ok ]]; then
  known_gaps_ok=true
  shift
fi
if (($# > 0)); then
  echo "usage: $0 [--known-gaps-ok]" >&2
  exit 2
fi

order_guest=$(compile_c \
  tests/e2e/determinism-stress/guests/signal_order.c signal-order)
show_native_variation "concurrent signal delivery order" "$order_guest"
failures=0
if ! verify_guest "concurrent signal delivery order" "$order_guest"; then
  failures=$((failures + 1))
fi

guest=$(compile_c tests/c/signal_determinism.c signal-determinism -lrt)
scenarios=(
  itimer-delivery
  itimer-exit
  blocking-sigsuspend
  masks-fork-clone
  blocking-read-interrupted
  blocking-read-restarted
  poll-sa-restart
  epoll-wait-sa-restart
  sigtimedwait-sa-restart
  handler-reentrance
  altstack-preservation
  pending-exec
)
for scenario in "${scenarios[@]}"; do
  if ! verify_guest "signal scenario: $scenario" "$guest" "$scenario"; then
    if [[ $known_gaps_ok == true && $scenario == blocking-sigsuspend ]]; then
      echo "XFAIL: blocking-sigsuspend has a known signal log-order divergence" >&2
    else
      failures=$((failures + 1))
    fi
  fi
done

((failures == 0)) || fail "$failures signal target(s) failed strict L2"
stress_success "signal delivery ordering"
