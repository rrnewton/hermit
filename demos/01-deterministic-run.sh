#!/usr/bin/env bash
#
# Demo 1: deterministic run.
#
# Hermit preserves the guest exit status and output while replacing common
# nondeterministic inputs (random bytes, wall-clock time, address layout, and
# Python hash seeding) with virtual, reproducible values.

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

demo_banner "Basic execution"
run_hermit -- /bin/echo hello

demo_banner "Virtual random bytes are stable across runs"
for attempt in 1 2; do
  run_hermit -- /bin/sh -c 'od -An -N8 -tx1 /dev/urandom'
done

demo_banner "Virtual wall-clock time is stable across runs"
for attempt in 1 2; do
  run_hermit -- /bin/date +%s.%N
done

demo_banner "Python entropy and hash ordering match under Hermit"
export PYTHON="${PYTHON:-/usr/bin/python3}"
export PYTHON_DEMO='import os; print("random="+os.urandom(16).hex()); print("hash="+str(hash("hermit-demo"))); print("set="+",".join(set(["alpha","beta","gamma","delta","epsilon"])))'
echo "-- native (normally differs) --"
for attempt in 1 2; do
  "$PYTHON" -c "$PYTHON_DEMO"
done
echo "-- hermit (matches exactly) --"
for attempt in 1 2; do
  run_hermit -- "$PYTHON" -c "$PYTHON_DEMO" | tee "$DEMO_TMP/python-hermit-$attempt.txt"
done
cmp "$DEMO_TMP/python-hermit-1.txt" "$DEMO_TMP/python-hermit-2.txt"

demo_banner "Address layout is stable across runs"
for attempt in 1 2; do
  run_hermit -- "$HEAP_PTRS" | tee "$DEMO_TMP/heap-hermit-$attempt.txt"
done
cmp "$DEMO_TMP/heap-hermit-1.txt" "$DEMO_TMP/heap-hermit-2.txt"

demo_banner "Built-in --verify determinizes a racy multi-process guest"
# examples/race.sh forks two shells that print interleaved output, so the
# interleaving differs on every native run. Show that nondeterminism first:
echo "-- native race: output interleaving differs each run (checksum of output) --"
for attempt in 1 2; do
  race_out="$(/bin/bash "$RACE_SH")"
  printf 'native run %s: cksum=%s\n' "$attempt" "$(printf '%s' "$race_out" | cksum | cut -d' ' -f1)"
done
# --verify runs the guest twice under Hermit and compares exit status, output,
# and the deterministic execution log -- thousands of DETLOG/scheduler messages,
# not the empty "0 | 0" that results from a too-quiet log level. This step uses
# PMU-based preemption (see verify_hermit in common.sh) and therefore needs
# accessible CPU performance counters.
echo "-- hermit --verify (identical output + verified execution log) --"
verify_hermit -- /bin/bash "$RACE_SH"

echo
echo "Demo 1 complete. The guest must be idempotent: a first run that changes a"
echo "file, database, cache, or external service can legitimately change the second."
