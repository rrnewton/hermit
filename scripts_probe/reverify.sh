#!/usr/bin/env bash
# Re-verify promotion candidates + contradictions with the REAL test invocation
# (60s timeout, HERMIT_MODE env) and REPEAT chaos runs to catch flakiness before
# promoting anything into the committed test suite.
set -uo pipefail

ROOT="/home/newton/work/dev-hermit/worktrees/slot87"
HERMIT="$ROOT/target/debug/hermit"
BIN="$ROOT/target/debug"
BUILD="$(mktemp -d "$HOME/reverify.XXXXXX")"   # NOT /tmp (hermit isolates guest /tmp)
TIMEOUT=60       # match the shipped buck_chaos test's `timeout 60s`
CHAOS_REPS=2     # chaos is randomized: require all reps to pass
RECORD_REPS=2

cd "$ROOT" || exit 1
compile_c()        { cc -O0 -g -pthread -D_GNU_SOURCE -I "$(dirname "$1")" "$1" -o "$2" 2>/dev/null; }
compile_c_nolibc() { cc -g -nostdlib "$1" -o "$2" 2>/dev/null; }
compile_rust()     { rustc --edition=2024 -C debuginfo=1 "$1" -o "$2" 2>/dev/null; }

# name|kind|src  (registered names only; kind cargo => prebuilt in target/debug)
declare -a CANDS=(
  # NOTE: nanosleep_parallel & rust_mem_race (shipped buck_chaos_tests) already
  # CONFIRMED FLAKY 0/3 at 90s -> they time out under chaos+verify+PMU on this
  # host. Not retested here; documented in the matrix as pre-existing flake.
  # --- RECORD contradictions (shipped record tests, probe said FAIL) ---
  "rustbin_pipe_basics|cargo||record"
  "rustbin_poll_spin|cargo||record"
  # --- CHAOSPMU promotion candidates (probe PASS, registered, not yet in list) ---
  "getcpu|cdef|tests/c/getCpu.c|chaos"
  "memory_pressure|cdef|tests/c/memoryPress.c|chaos"
  "print_memaddrs|cdef|tests/c/print_memaddrs.c|chaos"
  "printf_with_threads|cdef|tests/c/printf_with_threads.c|chaos"
  "sigtimedwait_no_timeout|cdef|tests/c/sigtimedwait-no-timeout.c|chaos"
  "sigtimedwait_timeout_0s|cdef|tests/c/sigtimedwait-timeout-0s.c|chaos"
  "sigtimedwait_timeout_1s|cdef|tests/c/sigtimedwait-timeout-1s.c|chaos"
  "sysinfo_uptime|cdef|tests/c/sysinfo_uptime.c|chaos"
  "thread_exhaustion|cdef|tests/c/threadExhaustion.c|chaos"
  "lit_hello_world_c|cdef|detcore/tests/lit/hello_world_c/main.c|chaos"
  "lit_networking|cdef|detcore/tests/lit/networking/main.c|chaos"
  "lit_hello_world_rust|rustdef|detcore/tests/lit/hello_world_rs/main.rs|chaos"
  "rust_stack_ptr|rustdef|tests/rust/stack_ptr.rs|chaos"
  "rust_heap_ptrs|rustdef|tests/rust/heap_ptrs.rs|chaos"
  "rust_rdtsc|rustdef|tests/rust/rdtsc.rs|chaos"
  "rustbin_clock_gettime|cargo||chaos"
  "rustbin_exit_group|cargo||chaos"
  "rustbin_futex_timeout|cargo||chaos"
  "rustbin_futex_wait_child|cargo||chaos"
  "rustbin_nanosleep|cargo||chaos"
  "rustbin_socketpair|cargo||chaos"
  "rustbin_thread_random|cargo||chaos"
  "rustbin_sched_yield|cargo||chaos"
  # --- RECORD promotion candidates (probe PASS, not yet in record lists) ---
  "getcpu_rec|cdef|tests/c/getCpu.c|record"
  "hello_alarm_rec|cdef|tests/c/hello_alarm.c|record"
  "memory_pressure_rec|cdef|tests/c/memoryPress.c|record"
  "sigtimedwait_no_timeout_rec|cdef|tests/c/sigtimedwait-no-timeout.c|record"
  "sysinfo_uptime_rec|cdef|tests/c/sysinfo_uptime.c|record"
  "thread_exhaustion_rec|cdef|tests/c/threadExhaustion.c|record"
  "rustbin_clock_gettime_rec|cargo|rustbin_clock_gettime|record"
  "rustbin_futex_and_print_rec|cargo|rustbin_futex_and_print|record"
  "rustbin_futex_wake_some_rec|cargo|rustbin_futex_wake_some|record"
  "rustbin_socketpair_rec|cargo|rustbin_socketpair|record"
  "rustbin_bind_connect_race_rec|cargo|rustbin_bind_connect_race|record"
  "rustbin_network_hello_world_rec|cargo|rustbin_network_hello_world|record"
  "rustbin_interrogate_tty_rec|cargo|rustbin_interrogate_tty|record"
)

build_one() {  # name kind src -> echoes path or COMPILE_FAIL/MISSING
  local name="$1" kind="$2" src="$3"
  case "$kind" in
    cstable|cdef) local o="$BUILD/$name"; compile_c "$ROOT/$src" "$o"; [[ -x "$o" ]] && echo "$o" || echo COMPILE_FAIL ;;
    rustdef)      local o="$BUILD/$name"; compile_rust "$ROOT/$src" "$o"; [[ -x "$o" ]] && echo "$o" || echo COMPILE_FAIL ;;
    cargo)        local b="${src:-$name}"; [[ -x "$BIN/$b" ]] && echo "$BIN/$b" || echo MISSING ;;
  esac
}

chaos_once() {
  timeout ${TIMEOUT}s "$HERMIT" run --verify --chaos --base-env=empty \
    --preemption-timeout=1000000 --env=HERMIT_MODE=chaos -- "$1" >/dev/null 2>&1
}
record_once() {
  local dd; dd="$(mktemp -d "$BUILD/rr.XXXXXX")"
  local out
  out="$(HERMIT_MODE=record timeout ${TIMEOUT}s "$HERMIT" record start --verify \
    --record-timeout=60 --data-dir="$dd" -- "$1" 2>&1)"
  rm -rf "$dd"
  grep -q "Success: replay matched recording" <<<"$out"
}

printf "%-38s %-8s %-6s %s\n" WORKLOAD MODE RESULT DETAIL
for entry in "${CANDS[@]}"; do
  IFS='|' read -r name kind src mode <<<"$entry"
  disp="${name%_rec}"
  path="$(build_one "$name" "$kind" "$src")"
  if [[ "$path" == COMPILE_FAIL || "$path" == MISSING ]]; then
    printf "%-38s %-8s %-6s %s\n" "$disp" "$mode" "$path" "-"; continue
  fi
  if [[ "$mode" == chaos ]]; then
    pass=0
    for _ in $(seq 1 $CHAOS_REPS); do chaos_once "$path" && pass=$((pass+1)); done
    res=$([[ $pass -eq $CHAOS_REPS ]] && echo PASS || echo FLAKY)
    printf "%-38s %-8s %-6s %s\n" "$disp" "$mode" "$res" "$pass/$CHAOS_REPS"
  else
    pass=0
    for _ in $(seq 1 $RECORD_REPS); do record_once "$path" && pass=$((pass+1)); done
    res=$([[ $pass -eq $RECORD_REPS ]] && echo PASS || echo FAIL)
    printf "%-38s %-8s %-6s %s\n" "$disp" "$mode" "$res" "$pass/$RECORD_REPS"
  fi
done
rm -rf "$BUILD"
