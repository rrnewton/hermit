#!/usr/bin/env bash
# Ground-truth chaos reliability: run each NEW buck_chaos test via the REAL
# cargo harness N times. Only tests that pass every iteration are kept.
set -uo pipefail
cd /home/newton/work/dev-hermit/worktrees/slot87 || exit 1
N=3
TESTS=(
  chaos_buck_getcpu chaos_buck_memory_pressure chaos_buck_print_memaddrs
  chaos_buck_printf_with_threads chaos_buck_sigtimedwait_no_timeout
  chaos_buck_sigtimedwait_timeout_0s chaos_buck_sigtimedwait_timeout_1s
  chaos_buck_sysinfo_uptime chaos_buck_thread_exhaustion
  chaos_buck_lit_hello_world_c chaos_buck_lit_networking
  chaos_buck_lit_hello_world_rust chaos_buck_rust_stack_ptr
  chaos_buck_rust_heap_ptrs chaos_buck_rust_rdtsc
  chaos_buck_rustbin_clock_gettime chaos_buck_rustbin_exit_group
  chaos_buck_rustbin_futex_timeout chaos_buck_rustbin_futex_wait_child
  chaos_buck_rustbin_nanosleep chaos_buck_rustbin_socketpair
  chaos_buck_rustbin_thread_random
)
declare -A PASS
for t in "${TESTS[@]}"; do PASS[$t]=0; done
for i in $(seq 1 $N); do
  for t in "${TESTS[@]}"; do
    if cargo test -p hermit --test hermit_modes -- --exact "$t" >/dev/null 2>&1; then
      PASS[$t]=$(( ${PASS[$t]} + 1 ))
    fi
  done
  echo "iter $i done"
done
echo "=== CHAOS REAL-TEST RELIABILITY ($N iters) ==="
for t in "${TESTS[@]}"; do
  v=$([[ ${PASS[$t]} -eq $N ]] && echo KEEP || echo DROP)
  printf "%-40s %-5s %s/%s\n" "$t" "$v" "${PASS[$t]}" "$N"
done
