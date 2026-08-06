/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * getcpu_identity — backend-parity contract for the getcpu(2) determinization.
 *
 * Detcore virtualizes a single logical CPU on a single virtual NUMA node, so
 * the raw SYS_getcpu syscall must always report CPU 0 / node 0 and succeed,
 * regardless of which host CPU actually ran the guest. That constant answer is
 * what makes the value bitwise-identical across --verify repeat runs and under
 * record/replay, and it must be identical across backends: the DBI backend has
 * to match the golden ptrace reference exactly.
 *
 * This fixture exercises the raw syscall (not glibc's vDSO-accelerated
 * sched_getcpu, which does not route through syscall interception) across the
 * optional-output-pointer combinations that detcore's handler special-cases —
 * both pointers set, cpu-only, node-only — and repeats the query to prove the
 * answer does not drift. Any nonzero CPU/node or a differing value between
 * calls means the container leaked host topology or the backend diverged.
 */

#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Poison sentinel: if detcore fails to write the output, the check catches it. */
#define SENTINEL 0x7fu

static int query(unsigned *cpu_out, unsigned *node_out) {
  if (cpu_out) {
    *cpu_out = SENTINEL;
  }
  if (node_out) {
    *node_out = SENTINEL;
  }
  /* Third argument (tcache) has been ignored by the kernel since Linux 2.6.24. */
  return (int)syscall(SYS_getcpu, cpu_out, node_out, NULL);
}

int main(void) {
  unsigned cpu = SENTINEL;
  unsigned node = SENTINEL;
  /* Last values actually observed, so stdout carries the OBSERVATION rather
   * than only a verdict word. A divergence is then visible in the parity
   * comparison itself, not just in an exit code. */
  unsigned seen_cpu = SENTINEL;
  unsigned seen_node = SENTINEL;
  int sentinel_overwritten = 0;

  /* Repeat to prove the determinized answer is stable, not incidental. */
  for (int i = 0; i < 4; i++) {
    /* Both output pointers set. */
    cpu = SENTINEL;
    node = SENTINEL;
    if (query(&cpu, &node) != 0) {
      fprintf(stderr, "iter %d: getcpu(cpu,node) did not return 0\n", i);
      return 1;
    }
#ifdef HERMIT_TEST_GETCPU_PLANT_HOST_LEAK
    cpu = 7; /* plant a leaked host CPU; the 0/0 check must catch it */
#endif
#ifdef HERMIT_TEST_GETCPU_PLANT_UNWRITTEN
    cpu = SENTINEL; /* plant "handler never wrote"; the sentinel guard must catch it */
    node = SENTINEL;
#endif
    /* NON-VACUITY: the sentinel proves the kernel/handler actually wrote here.
     * Without it, a handler that writes nothing leaves the caller's zeroed
     * buffer looking like a perfect 0/0 answer. */
    if (cpu != SENTINEL || node != SENTINEL) {
      sentinel_overwritten = 1;
    }
    seen_cpu = cpu;
    seen_node = node;
    if (cpu != 0 || node != 0) {
      fprintf(stderr, "iter %d: getcpu(cpu,node) reported cpu=%u node=%u, expected 0/0\n",
              i, cpu, node);
      return 1;
    }

    /* cpu only (node pointer NULL): handler must still write cpu=0. */
    cpu = SENTINEL;
    if (query(&cpu, NULL) != 0 || cpu != 0) {
      fprintf(stderr, "iter %d: getcpu(cpu,NULL) reported cpu=%u, expected 0\n", i, cpu);
      return 1;
    }

    /* node only (cpu pointer NULL): handler must still write node=0. */
    node = SENTINEL;
    if (query(NULL, &node) != 0 || node != 0) {
      fprintf(stderr, "iter %d: getcpu(NULL,node) reported node=%u, expected 0\n", i, node);
      return 1;
    }
  }

  /* Emit the OBSERVED values. On this host a native run reports the real CPU
   * the scheduler happened to pick (measured: cpu=55 of 316, and it moves), so
   * these numbers are the difference between "determinized" and "leaked". */
  printf("getcpu observed cpu=%u node=%u iters=4 sentinel_overwritten=%s\n",
         seen_cpu, seen_node, sentinel_overwritten ? "yes" : "no");
  if (!sentinel_overwritten) {
    fprintf(stderr, "getcpu: no output pointer was ever written -- vacuous pass\n");
    return 1;
  }
  puts("getcpu-identity-ok");
  return 0;
}
