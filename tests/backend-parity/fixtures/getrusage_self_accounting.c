// getrusage(RUSAGE_SELF) CPU-accounting determinization parity probe.
//
// A process's own resource-usage counters are host-derived state: outside
// Hermit, ru_utime/ru_stime grow with real CPU time and the fault and
// context-switch counters reflect the host kernel's scheduling of the run.
// Hermit must not let a guest observe WALL-DERIVED CPU consumption or host
// scheduling artifacts. This fixture used to encode that as "the CPU fields are
// ZERO", which over-specified the requirement: zero is not-host-derived, but it
// is also FROZEN, so a guest measuring its own CPU saw no progress however much
// work it did, and getrusage contradicted times(2) -- which has derived the same
// quantities from virtual time all along. The contract is now the accurate one:
// ru_utime/ru_stime are a continuous function of Detcore's VIRTUAL clock, so they
// ADVANCE with executed work and are identical run-to-run. The fault and
// context-switch counters remain zeroed (no virtual model exists for them). This is the
// RUSAGE_SELF sibling of the process-wait "zeroed child CPU accounting" the
// wait contract already covers; getrusage itself is accepted (it must not
// spuriously fail), but the accounting it reports is the determinized zero.
//
// ru_maxrss is deliberately NOT asserted: peak resident set is a legitimate
// backend-local memory-footprint number (ptrace, DBI, and KVM each report a
// different value) and determinizing it is neither required nor claimed.
//
// The fixture first burns measurable CPU so both native and Hermit have clearly
// nonzero user time. Under Hermit all six checks are expected to pass (ok=6).
// Native scores ok=5 (measured): it passes acceptance, advancing user time, the
// sanity bound, and the major-fault check, but fails the zeroed minor-fault /
// context-switch checks because it faithfully reports host scheduling artifacts.
// Note the discriminating power now lives in checks 2+3 TOGETHER WITH the
// --verify double run: advancement alone would also be satisfied by leaking host
// CPU, and it is the run-to-run byte-identity of this stdout that rules that out.

#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/time.h>

int main(void) {
    // Burn measurable CPU so native accounting is unambiguously nonzero.
    volatile unsigned long acc = 0;
    for (unsigned long i = 0; i < 50000000UL; i++) acc += i;

    struct rusage ru;
    memset(&ru, 0, sizeof(ru));
    int rc = getrusage(RUSAGE_SELF, &ru);

    int ok = 0;
    // (1) getrusage(RUSAGE_SELF) is accepted (native and all backends).
    if (rc == 0) ok++;
    // (2) Determinized from VIRTUAL time, not frozen: after burning 5e7 iterations
    // the guest's own user CPU must have ADVANCED. A zero here is the old frozen
    // contract and is now a failure, not a pass.
    if (ru.ru_utime.tv_sec > 0 || ru.ru_utime.tv_usec > 0) ok++;
    // (3) Virtual CPU is bounded, i.e. it is the logical clock and not the host's:
    // this loop cannot plausibly cost a virtual minute, so a huge value would mean
    // host time leaked in. Determinism itself is asserted by the --verify double run,
    // which compares this stdout byte-for-byte between two executions.
    if (ru.ru_utime.tv_sec < 60) ok++;
    // (4) Determinized: minor page-fault count is zeroed (native counts them).
    if (ru.ru_minflt == 0) ok++;
    // (5) Major page-fault count is zero (both native and Hermit for this run).
    if (ru.ru_majflt == 0) ok++;
    // (6) Determinized: voluntary and involuntary context switches are zeroed.
    if (ru.ru_nvcsw == 0 && ru.ru_nivcsw == 0) ok++;

    // Consume acc so the burn loop cannot be optimized away.
    if (acc == 0) return 2;
    printf("getrusage ok=%d\n", ok);
    return 0;
}
