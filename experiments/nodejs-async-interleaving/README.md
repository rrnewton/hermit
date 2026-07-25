# Node.js Async Completion-Order Determinism

This experiment demonstrates Hermit controlling scheduling across Node.js
`worker_threads`.

Eight workers block on one shared atomic start gate. Once released, each worker
performs deterministic computation and posts 24 round-completion messages
through its own event loop. The main thread records the cross-worker message
delivery order. Per-worker message order is validated, and no randomness,
network input, filesystem input, or wall-clock time influences the trace:

```text
NONDET_SOURCE: async scheduling
```

The dual assertion is intentionally fail-closed:

1. Repeated native Node.js runs must produce at least two complete trace hashes.
2. Repeated `hermit run --strict` runs must produce exactly one trace hash.

The harness verifies the event count and completion marker, bounds every
process with a timeout, and stores generated logs under the ignored Cargo
`target/` directory. Node runs with `--jitless` so the demonstration focuses
on application worker/event-loop scheduling rather than background JIT work.

## Run

From the repository root:

```bash
experiments/nodejs-async-interleaving/run.sh
```

The defaults use eight workers, 24 events per worker, 12 native runs, and five
strict runs. They can be adjusted without editing the source:

```bash
NATIVE_RUNS=20 STRICT_RUNS=8 WORKERS=12 ROUNDS=32 \
  experiments/nodejs-async-interleaving/run.sh
```

Set `HERMIT` or `NODE` to use non-default binaries.

## Expected Result

```text
NONDET_SOURCE: async scheduling
native runs: 12, unique traces: >1
strict runs: 5, unique traces: 1
strict trace sha256: <digest>
PASS: native async order varied while strict Hermit output was byte-identical
```

## Observed Result

On 2026-07-22, Hermit
`5d3b2a35870a1d2e1d78a098219cfa7c1929aa33` with Node.js v16.20.2 produced:

```text
NONDET_SOURCE: async scheduling
native runs: 12, unique traces: 12
strict runs: 5, unique traces: 1
strict trace sha256: bed18b7975e7ebc6a6ff71617f48bfa3d9107fa4857ac018aadaf63e4f788fd4
PASS: native async order varied while strict Hermit output was byte-identical
```

Both full harness invocations produced the same strict trace hash while every
native invocation produced 12 distinct traces. Native stderr contains only
Node's expected `--jitless` warning. Strict stderr also contains the
host-specific Hermit warning that CPUID faulting is not supported, repeated as
worker threads start. That limitation does not affect the demonstrated async
completion trace, but this result does not claim full CPUID determinization on
this host.
