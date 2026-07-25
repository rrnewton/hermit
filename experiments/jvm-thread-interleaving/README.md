# JVM Thread Interleaving Determinism

This experiment is a focused end-to-end demonstration of Hermit controlling
JVM application-thread scheduling.

`ThreadInterleaving.java` starts a configurable number of Java threads,
blocks them on the same start latch, and then has them repeatedly contend on a
synchronized trace. Every thread performs deterministic computation and emits
a stable `thread:round` token. The only input that chooses token order is
thread scheduling:

```text
NONDET_SOURCE: thread scheduling
```

The harness runs the same compiled class repeatedly in two modes:

1. Native OpenJDK must produce at least two different complete trace hashes.
2. `hermit run --strict` must produce exactly one trace hash across all runs.

It also verifies the exact event count, fails on a missing completion marker,
bounds every run with a timeout, and stores generated classes and logs under
the ignored Cargo `target/` directory.

## Run

From the repository root:

```bash
experiments/jvm-thread-interleaving/run.sh
```

The defaults use 12 threads, 48 trace events per thread, 12 native runs, and
five strict runs. They can be adjusted without editing the source:

```bash
NATIVE_RUNS=20 STRICT_RUNS=8 THREADS=16 ROUNDS=64 \
  experiments/jvm-thread-interleaving/run.sh
```

Set `HERMIT`, `JAVA`, or `JAVAC` to use non-default binaries. The Java
process uses interpreter mode, a fixed-size heap, and Serial GC so the observed
difference is application-thread scheduling rather than background JIT or
parallel-GC work.

## Expected Result

A successful run reports multiple native hashes and one strict hash:

```text
NONDET_SOURCE: thread scheduling
native runs: 12, unique traces: >1
strict runs: 5, unique traces: 1
strict trace sha256: <digest>
PASS: native scheduling varied while strict Hermit output was byte-identical
```

## Observed Result

On 2026-07-22, Hermit
`5d3b2a35870a1d2e1d78a098219cfa7c1929aa33` with OpenJDK 8u492 produced:

```text
NONDET_SOURCE: thread scheduling
native runs: 12, unique traces: 12
strict runs: 5, unique traces: 1
strict trace sha256: 0fd50c26b2b720ac71d1a47891c6c2068e2663ce8cbf28a82a2081720523768c
PASS: native scheduling varied while strict Hermit output was byte-identical
```

All native runs completed without diagnostics. Strict runs completed
successfully but emitted the host-specific warning that CPUID faulting is not
supported. That limitation does not affect the demonstrated application-thread
trace, but this result does not claim full CPUID determinization on this host.
