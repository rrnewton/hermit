#!/usr/bin/env bash
# Probe existing hermit guest workloads across run modes to build an honest
# record/replay + chaos coverage matrix.
#
# Modes probed (mirroring hermit-cli/tests/{hermit_modes,record_replay}.rs):
#   DEFAULT       run --allow-passthrough --no-sequentialize-threads --no-deterministic-io
#   VERIFY        run --verify (bitwise determinism across two runs)
#   CHAOS         run --chaos (stable-matrix style, no PMU preemption)
#   CHAOSPMU      run --verify --chaos --preemption-timeout=1000000 (buck_chaos style, needs PMU)
#   RECORD        record start --verify (record then replay --verify)
#   STRICT        run --strict --verify (task-literal strict determinism)
set -uo pipefail

ROOT="/home/newton/work/dev-hermit/worktrees/slot87"
HERMIT="$ROOT/target/debug/hermit"
BIN="$ROOT/target/debug"          # cargo guest binaries live here
BUILD="$(mktemp -d "$HOME/probe-workloads.XXXXXX")"  # NOT /tmp: hermit isolates guest /tmp
TIMEOUT=30

cd "$ROOT" || exit 1

# ---- compile helpers (mirror hermit_modes.rs) ----
compile_c()          { cc -O0 -g -pthread -D_GNU_SOURCE -I "$(dirname "$1")" "$1" -o "$2" 2>/dev/null; }
compile_c_nolibc()   { cc -g -nostdlib "$1" -o "$2" 2>/dev/null; }
compile_rust()       { rustc --edition=2024 -C debuginfo=1 "$1" -o "$2" 2>/dev/null; }

# ---- workload table: name|kind|source (relative to ROOT) ----
# kind: cstable / cdef / rustdef / nolibc / cargo / script
declare -a WL=(
  "getpid|cstable|tests/c/getpid.c"
  "uname|cstable|tests/c/uname.c"
  "sysinfo|cstable|tests/c/sysinfo.c"
  "wait_on_child|cstable|tests/c/wait_on_child.c"
  "nanosleep_parallel|cstable|tests/c/nanosleep-par.c"
  "clone|cdef|tests/c/clone.c"
  "getcpu|cdef|tests/c/getCpu.c"
  "hello_alarm|cdef|tests/c/hello_alarm.c"
  "hello_signals|cdef|tests/c/hello_signals.c"
  "just_spin|cdef|tests/c/just_spin.c"
  "memory_pressure|cdef|tests/c/memoryPress.c"
  "print_memaddrs|cdef|tests/c/print_memaddrs.c"
  "printf_with_threads|cdef|tests/c/printf_with_threads.c"
  "sigtimedwait_no_timeout|cdef|tests/c/sigtimedwait-no-timeout.c"
  "sigtimedwait_timeout_0s|cdef|tests/c/sigtimedwait-timeout-0s.c"
  "sigtimedwait_timeout_1s|cdef|tests/c/sigtimedwait-timeout-1s.c"
  "sysinfo_uptime|cdef|tests/c/sysinfo_uptime.c"
  "thread_exhaustion|cdef|tests/c/threadExhaustion.c"
  "lit_hello_world_c|cdef|detcore/tests/lit/hello_world_c/main.c"
  "lit_rt_sigaction|cdef|detcore/tests/lit/rt_sigaction/main.c"
  "lit_networking|cdef|detcore/tests/lit/networking/main.c"
  "network_bind|rustdef|tests/standalone/network_bind.rs"
  "lit_hello_world_rust|rustdef|detcore/tests/lit/hello_world_rs/main.rs"
  "rust_stack_ptr|rustdef|tests/rust/stack_ptr.rs"
  "rust_heap_ptrs|rustdef|tests/rust/heap_ptrs.rs"
  "rust_rdtsc|rustdef|tests/rust/rdtsc.rs"
  "rust_mem_race|rustdef|tests/rust/mem_race.rs"
  "minimal_hello|nolibc|tests/c/simple/hello_nostdlib.c"
  "pread64_nostdlib|nolibc|tests/c/simple/pread64_nostdlib.c"
)
# cargo guests already built in target/debug
declare -a CARGO=(
  rustbin_bind_connect_race rustbin_clock_gettime rustbin_clock_total_order
  rustbin_exit_group rustbin_futex_and_print rustbin_futex_timeout
  rustbin_futex_wait_child rustbin_futex_wake_some rustbin_heap_ptrs
  rustbin_interrogate_tty rustbin_nanosleep rustbin_network_hello_world
  rustbin_pipe_basics rustbin_poll rustbin_poll_spin
  rustbin_print_clock_nanosleep_monotonic_abs_race
  rustbin_print_clock_nanosleep_monotonic_race
  rustbin_print_clock_nanosleep_realtime_abs_race
  rustbin_print_nanosleep_race rustbin_sched_yield rustbin_socketpair
  rustbin_thread_random rustbin_rdtsc rustbin_stack_ptr
)

# ---- build all workloads ----
declare -A PATHS
for entry in "${WL[@]}"; do
  IFS='|' read -r name kind src <<<"$entry"
  out="$BUILD/$name"
  case "$kind" in
    cstable|cdef) compile_c "$ROOT/$src" "$out" ;;
    rustdef)      compile_rust "$ROOT/$src" "$out" ;;
    nolibc)       compile_c_nolibc "$ROOT/$src" "$out" ;;
  esac
  if [[ -x "$out" ]]; then PATHS[$name]="$out"; else PATHS[$name]="COMPILE_FAIL"; fi
done
for name in "${CARGO[@]}"; do
  if [[ -x "$BIN/$name" ]]; then PATHS[$name]="$BIN/$name"; else PATHS[$name]="MISSING"; fi
done

# ---- mode runners: echo PASS/FAIL/SKIP ----
run_default() {
  timeout $TIMEOUT "$HERMIT" run --base-env=minimal --no-virtualize-cpuid \
    --preemption-timeout=disabled --allow-passthrough \
    --no-sequentialize-threads --no-deterministic-io -- "$1" >/dev/null 2>&1
}
run_verify() {
  timeout $TIMEOUT "$HERMIT" run --verify --base-env=minimal --no-virtualize-cpuid \
    --preemption-timeout=disabled --allow-passthrough -- "$1" >/dev/null 2>&1
}
run_chaos() {
  timeout $TIMEOUT "$HERMIT" run --chaos --base-env=minimal --no-virtualize-cpuid \
    --preemption-timeout=disabled --allow-passthrough -- "$1" >/dev/null 2>&1
}
run_chaospmu() {
  timeout $TIMEOUT "$HERMIT" run --verify --chaos --base-env=empty \
    --preemption-timeout=1000000 -- "$1" >/dev/null 2>&1
}
run_record() {
  local dd; dd="$(mktemp -d "$BUILD/rr.XXXXXX")"
  local out
  out="$(HERMIT_MODE=record timeout $TIMEOUT "$HERMIT" record start --verify \
    --record-timeout=30 --data-dir="$dd" -- "$1" 2>&1)"
  rm -rf "$dd"
  grep -q "Success: replay matched recording" <<<"$out"
}
run_strict() {
  timeout $TIMEOUT "$HERMIT" run --strict --verify --base-env=minimal \
    --no-virtualize-cpuid --preemption-timeout=disabled -- "$1" >/dev/null 2>&1
}

probe() {
  local fn="$1" path="$2"
  if [[ "$path" == "COMPILE_FAIL" || "$path" == "MISSING" ]]; then echo "$path"; return; fi
  if $fn "$path"; then echo "PASS"; else echo "FAIL"; fi
}

# ---- run the matrix ----
printf "%-42s %-8s %-8s %-8s %-9s %-8s %-8s\n" WORKLOAD DEFAULT VERIFY CHAOS CHAOSPMU RECORD STRICT
ALLNAMES=()
for entry in "${WL[@]}"; do IFS='|' read -r name _ _ <<<"$entry"; ALLNAMES+=("$name"); done
ALLNAMES+=("${CARGO[@]}")

for name in "${ALLNAMES[@]}"; do
  p="${PATHS[$name]}"
  d=$(probe run_default   "$p")
  v=$(probe run_verify    "$p")
  c=$(probe run_chaos     "$p")
  cp=$(probe run_chaospmu "$p")
  r=$(probe run_record    "$p")
  s=$(probe run_strict    "$p")
  printf "%-42s %-8s %-8s %-8s %-9s %-8s %-8s\n" "$name" "$d" "$v" "$c" "$cp" "$r" "$s"
done

rm -rf "$BUILD"
