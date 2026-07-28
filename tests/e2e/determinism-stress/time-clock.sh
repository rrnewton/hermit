#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=tests/e2e/determinism-stress/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"

clock_guest=$(compile_c tests/c/clock_determinism.c clock-determinism -lrt)
verify_guest "gettimeofday and clock_gettime matrix" "$clock_guest"

show_native_variation "timestamped date output" "$repo_root/examples/date.sh"
verify_guest "timestamped date output" "$repo_root/examples/date.sh"

stress_success "time, clocks, sleeps, and timestamps"
