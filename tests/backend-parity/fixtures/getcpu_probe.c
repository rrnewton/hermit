/*
 * getcpu_probe: cross-backend parity contract for getcpu(2).
 *
 * getcpu returns the CPU and NUMA node the calling thread is currently running
 * on. The concrete CPU index is host-topology-dependent and would be a
 * nondeterminism channel if asserted directly, so this contract checks only
 * that the call succeeds and writes a value into the output word — never the
 * value itself. Under Hermit the guest is serialized onto a single logical CPU,
 * so the written index is stable, but the assertions here are deliberately
 * value-agnostic so the golden output is portable across host topologies.
 *
 * Two checks: getcpu() returns 0, and the sentinel output word was overwritten.
 */

#include <sched.h>
#include <stdio.h>

int main(void) {
    unsigned cpu = 0xDEADBEEFu;
    int ok = 0;

    if (getcpu(&cpu, NULL) == 0) {
        ok++;
    }
    if (cpu != 0xDEADBEEFu) {
        ok++;
    }

    printf("getcpu ok=%d\n", ok);
    return 0;
}
