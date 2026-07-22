#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Super-validate: an aggressive determinism tier for nightly/manual use.
#
# It re-runs a curated set of fast guest programs under `hermit run --verify`
# with heap and stack hashing (--detlog-heap/--detlog-stack), which hashes the
# guest's heap and stack memory maps on every event. That catches memory-layout
# non-determinism that output-only --verify misses, at a large per-event cost.
#
# Because a single hashed run is cheap for these guests (~1-6s) but intermittent
# non-determinism is rare, the script cycles through the whole guest list
# repeatedly until a wall-clock budget is reached (default ~1 hour). Every guest
# in the list was verified deterministic across repeated hashed runs before being
# included; a failure here therefore indicates a real regression.
#
# Environment overrides:
#   SUPER_VALIDATE_MAX_SECONDS  wall-clock budget (default 3600).
#   SUPER_VALIDATE_SKIP_BUILD   set to 1 to reuse an existing target/ build.
#   SUPER_VALIDATE_GUEST_TIMEOUT per-run timeout (default 90s).

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly ROOT_DIR
cd "$ROOT_DIR" || exit 1

readonly MAX_SECONDS="${SUPER_VALIDATE_MAX_SECONDS:-3600}"
readonly GUEST_TIMEOUT="${SUPER_VALIDATE_GUEST_TIMEOUT:-90s}"
readonly HERMIT_BIN="$ROOT_DIR/target/debug/hermit"
declare -ar HERMIT_RUN_ARGS=(
    run
    --base-env=minimal
    --no-virtualize-cpuid
    --preemption-timeout=disabled
    --verify
    --detlog-heap
    --detlog-stack
)

# Curated guests, each verified deterministic across repeated hashed runs. These
# span heap/stack layout, virtual time (clock_gettime, nanosleep, rdtsc),
# scheduling, futexes, IPC, and racy multithreaded programs that Hermit must
# determinize. Guests that block when run standalone, need networking/a tty, or
# exceed the per-run budget are deliberately excluded.
declare -ar SUPER_GUESTS=(
    rustbin_heap_ptrs
    rustbin_stack_ptr
    rustbin_nanosleep
    rustbin_clock_gettime
    rustbin_rdtsc
    rustbin_sched_yield
    rustbin_futex_timeout
    rustbin_futex_wait_child
    rustbin_socketpair
    rustbin_pipe_basics
    rustbin_mem_race
    rustbin_thread_random
    rustbin_print_nanosleep_race
    rustbin_print_clock_nanosleep_monotonic_race
    rustbin_print_clock_nanosleep_monotonic_abs_race
    rustbin_print_clock_nanosleep_realtime_abs_race
)

function build_workspace {
    if [[ ${SUPER_VALIDATE_SKIP_BUILD:-0} == 1 ]]; then
        return 0
    fi
    printf "Building workspace (set SUPER_VALIDATE_SKIP_BUILD=1 to skip)...\n"
    if command -v with-proxy >/dev/null 2>&1; then
        with-proxy cargo build --workspace
    else
        cargo build --workspace
    fi
}

if ! build_workspace; then
    printf "❌ super-validate: workspace build failed\n" >&2
    exit 1
fi
if [[ ! -x $HERMIT_BIN ]]; then
    printf "❌ super-validate: missing hermit binary at %s\n" "$HERMIT_BIN" >&2
    exit 1
fi

# Confirm every guest binary exists before spending the time budget.
missing=0
for guest in "${SUPER_GUESTS[@]}"; do
    if [[ ! -x "$ROOT_DIR/target/debug/$guest" ]]; then
        printf "❌ super-validate: missing guest binary: %s\n" "$guest" >&2
        missing=1
    fi
done
((missing == 0)) || exit 1

declare -A guest_pass=()
declare -A guest_fail=()
for guest in "${SUPER_GUESTS[@]}"; do
    guest_pass["$guest"]=0
    guest_fail["$guest"]=0
done

total_runs=0
total_failures=0
cycles=0
start=$SECONDS

printf "super-validate: %s guests, budget %ss, per-run timeout %s\n\n" \
    "${#SUPER_GUESTS[@]}" "$MAX_SECONDS" "$GUEST_TIMEOUT"

while ((SECONDS - start < MAX_SECONDS)); do
    cycles=$((cycles + 1))
    for guest in "${SUPER_GUESTS[@]}"; do
        if ((SECONDS - start >= MAX_SECONDS)); then
            break
        fi
        bin="$ROOT_DIR/target/debug/$guest"
        total_runs=$((total_runs + 1))
        if timeout "$GUEST_TIMEOUT" \
            "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" -- "$bin" \
            >/dev/null 2>&1; then
            guest_pass["$guest"]=$((guest_pass["$guest"] + 1))
        else
            guest_fail["$guest"]=$((guest_fail["$guest"] + 1))
            total_failures=$((total_failures + 1))
            printf "❌ NON-DETERMINISM: %s (cycle %s, run %s)\n" \
                "$guest" "$cycles" "$total_runs"
        fi
    done
done

elapsed=$((SECONDS - start))

printf "\n=== super-validate summary ===\n"
printf "Elapsed: %ss over %s cycle(s); %s runs, %s failure(s)\n" \
    "$elapsed" "$cycles" "$total_runs" "$total_failures"
for guest in "${SUPER_GUESTS[@]}"; do
    printf "  %-48s %s passed, %s failed\n" \
        "$guest" "${guest_pass[$guest]}" "${guest_fail[$guest]}"
done

if ((total_failures == 0)); then
    printf "✅ super-validate: all %s runs deterministic\n" "$total_runs"
    exit 0
fi
printf "❌ super-validate: %s non-deterministic run(s)\n" "$total_failures"
exit 1
