#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

order_guest=$(compile_c \
  tests/e2e/determinism-stress/guests/signal_order.c signal-order)
show_native_variation "concurrent signal delivery order" "$order_guest"
verify_guest "concurrent signal delivery order" "$order_guest"

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
  verify_guest "signal scenario: $scenario" "$guest" "$scenario"
done

stress_success "signal delivery ordering"
