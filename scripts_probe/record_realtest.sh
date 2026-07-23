#!/usr/bin/env bash
# Ground-truth record reliability: run each NEW record test via the REAL cargo
# harness (which asserts process exit status, not just the "Success" string) N
# times. Only tests that pass every iteration are safe to keep (guards against
# the flaky reverie e3e2c96 clone SIGSEGV that crashes teardown after a
# logically-successful replay).
set -uo pipefail
cd /home/newton/work/dev-hermit/worktrees/slot87 || exit 1
N=3
TESTS=(
  expanded_c_record_replay_matrix
  record_rs_clock_gettime
  record_rs_futex_and_print
  record_rs_socketpair
  record_rs_bind_connect_race
  record_rs_network_hello_world
  record_rs_interrogate_tty
)
declare -A PASS
for t in "${TESTS[@]}"; do PASS[$t]=0; done
for i in $(seq 1 $N); do
  for t in "${TESTS[@]}"; do
    if cargo test -p hermit --test record_replay -- --exact "$t" >/dev/null 2>&1; then
      PASS[$t]=$(( ${PASS[$t]} + 1 ))
    fi
  done
  echo "iter $i done"
done
echo "=== RECORD REAL-TEST RELIABILITY ($N iters) ==="
for t in "${TESTS[@]}"; do
  v=$([[ ${PASS[$t]} -eq $N ]] && echo KEEP || echo DROP)
  printf "%-34s %-5s %s/%s\n" "$t" "$v" "${PASS[$t]}" "$N"
done
