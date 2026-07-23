#!/usr/bin/env bash
# Final stability gate: run each promotion candidate REPS times. Only 5/5 gets
# promoted into the committed suite. Guards against chaos randomness AND the
# flaky reverie clone SIGSEGV (rev e3e2c96) seen in record mode.
set -uo pipefail
ROOT="/home/newton/work/dev-hermit/worktrees/slot87"
HERMIT="$ROOT/target/debug/hermit"
BIN="$ROOT/target/debug"
BUILD="$(mktemp -d "$HOME/stability.XXXXXX")"
TIMEOUT=60
REPS=5
cd "$ROOT" || exit 1
compile_c()    { cc -O0 -g -pthread -D_GNU_SOURCE -I "$(dirname "$1")" "$1" -o "$2" 2>/dev/null; }
compile_rust() { rustc --edition=2024 -C debuginfo=1 "$1" -o "$2" 2>/dev/null; }

# name|kind|src|mode   kind: cdef|rustdef|cargo   mode: chaos|record
declare -a C=(
  # chaos promotion candidates
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
  # record promotion candidates
  "getcpu|cdef|tests/c/getCpu.c|record"
  "hello_alarm|cdef|tests/c/hello_alarm.c|record"
  "memory_pressure|cdef|tests/c/memoryPress.c|record"
  "sigtimedwait_no_timeout|cdef|tests/c/sigtimedwait-no-timeout.c|record"
  "sysinfo_uptime|cdef|tests/c/sysinfo_uptime.c|record"
  "thread_exhaustion|cdef|tests/c/threadExhaustion.c|record"
  "rustbin_clock_gettime|cargo||record"
  "rustbin_futex_and_print|cargo||record"
  "rustbin_futex_wake_some|cargo||record"
  "rustbin_socketpair|cargo||record"
  "rustbin_bind_connect_race|cargo||record"
  "rustbin_network_hello_world|cargo||record"
  "rustbin_interrogate_tty|cargo||record"
)
build_one() {
  local name="$1" kind="$2" src="$3"
  case "$kind" in
    cdef)    local o="$BUILD/$name.c"; compile_c "$ROOT/$src" "$o"; [[ -x "$o" ]] && echo "$o" || echo COMPILE_FAIL ;;
    rustdef) local o="$BUILD/$name.r"; compile_rust "$ROOT/$src" "$o"; [[ -x "$o" ]] && echo "$o" || echo COMPILE_FAIL ;;
    cargo)   [[ -x "$BIN/$name" ]] && echo "$BIN/$name" || echo MISSING ;;
  esac
}
chaos_once()  { timeout ${TIMEOUT}s "$HERMIT" run --verify --chaos --base-env=empty --preemption-timeout=1000000 --env=HERMIT_MODE=chaos -- "$1" >/dev/null 2>&1; }
record_once() { local dd; dd="$(mktemp -d "$BUILD/rr.XXXXXX")"; local o; o="$(HERMIT_MODE=record timeout ${TIMEOUT}s "$HERMIT" record start --verify --record-timeout=30 --data-dir="$dd" -- "$1" 2>&1)"; rm -rf "$dd"; grep -q "Success: replay matched recording" <<<"$o"; }

printf "%-32s %-7s %-8s %s\n" WORKLOAD MODE VERDICT PASSES
for e in "${C[@]}"; do
  IFS='|' read -r name kind src mode <<<"$e"
  p="$(build_one "$name" "$kind" "$src")"
  if [[ "$p" == COMPILE_FAIL || "$p" == MISSING ]]; then printf "%-32s %-7s %-8s %s\n" "$name" "$mode" "$p" -; continue; fi
  pass=0
  for _ in $(seq 1 $REPS); do
    if [[ "$mode" == chaos ]]; then chaos_once "$p" && pass=$((pass+1)); else record_once "$p" && pass=$((pass+1)); fi
  done
  v=$([[ $pass -eq $REPS ]] && echo PROMOTE || echo REJECT)
  printf "%-32s %-7s %-8s %s\n" "$name" "$mode" "$v" "$pass/$REPS"
done
rm -rf "$BUILD"
