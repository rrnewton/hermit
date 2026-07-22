# Fail-Closed Test Status

Status: `pread64` modeled and review-stack ratchet integrated, 2026-07-22

Hermit's fail-closed diagnostic converts an unsupported syscall that reaches
Detcore into a panic instead of silently passing it through. The integration
ratchet sets `HERMIT_FAIL_CLOSED=1`; `hermit run` consumes that internal test
environment variable as if `--panic-on-unsupported-syscalls` had been passed.
The normal command-line default remains unchanged.

## Baseline

The baseline used Hermit revision
`5d3b2a35870a1d2e1d78a098219cfa7c1929aa33` plus the integration tests present
in the working branch. Every `hermit-cli/tests/*.rs` target was run serially
with `HERMIT_FAIL_CLOSED=1`. The raw integration harness result was 37 passed,
86 failed, and 11 ignored. The policy classification below separates tests
that actually exercise the `hermit run` fail-closed path from tests for which
the mode is not applicable.

Detcore now handles `pread64` deterministically, including positional retries
for short regular-file reads and seeded reads from random devices. The 85
tests previously blocked at `pread64` proceed further, but each next reaches
the unsupported `rseq` registration performed by the system C library. A
representative `clock_determinism` run and a direct `/bin/true` run both
confirmed this sequence. The measured applicable pass count therefore remains
3/89 rather than the projected 88/89; the known-failure manifest now records
`Rseq` as the first current blocker for those tests.

| Test target or category | Fail-closed pass | Known failure | Ignored | Mode N/A |
| --- | ---: | ---: | ---: | ---: |
| `arbitrary_binaries` | 0 | 1 | 0 | 1 |
| `cli` | 0 | 0 | 0 | 16 |
| `clock_determinism` | 0 | 1 | 0 | 0 |
| `epoll_determinism` | 0 | 5 | 0 | 0 |
| `hermit_modes` | 3 | 60 | 8 | 0 |
| `ipc_determinism` | 0 | 1 | 0 | 0 |
| `mmap_determinism` | 0 | 5 | 0 | 0 |
| `procfs_determinism` | 0 | 6 | 0 | 0 |
| `random_determinism` | 0 | 1 | 0 | 0 |
| `record_replay` | 0 | 0 | 0 | 17 |
| `signal_determinism` | 0 | 5 | 0 | 0 |
| `stress_suite` | 0 | 0 | 3 | 0 |
| `thread_sync_determinism` | 0 | 1 | 0 | 0 |
| Hermit library and binary unit tests | 0 | 0 | 0 | 33 |
| **Total** | **3** | **86** | **11** | **67** |

The enabled fail-closed tests are:

- `hermit_modes/default_minimal_hello`
- `hermit_modes/no_hardware_minimal_hello_backtraces`
- `hermit_modes/no_hardware_stacktrace_signal`

The complete test-level matrix is represented by the table plus these
machine-readable exception lists:

- [`fail_closed_known_failures.tsv`](../hermit-cli/tests/fail_closed_known_failures.tsv)
  records every failing target/test pair and its first observed blocker. The
  original baseline had 85 failures at `pread64` and one at `ioctl`. After
  `pread64` handling, those 85 failures advance to `rseq`.
- [`fail_closed_allowed_ignores.tsv`](../hermit-cli/tests/fail_closed_allowed_ignores.tsv)
  records that the human-review stack has activated every formerly ignored test.
- Unit tests, `cli`, and `record_replay` do not execute Detcore's `hermit run`
  syscall policy. The record/replay case in `arbitrary_binaries` is also mode
  N/A. They remain covered by regular CI instead of inflating the fail-closed
  pass count.

## Ratchet Policy

Run the ratchet from the repository root:

```bash
./scripts/test-fail-closed.sh
```

Additional Cargo arguments can be forwarded before the test-harness separator,
which is useful for a local dependency override:

```bash
./scripts/test-fail-closed.sh --config 'patch."https://example.invalid/repo".crate.path="/path/to/crate"'
```

The runner discovers every integration target and test at runtime. It validates
the known-failure manifest, rejects duplicate or stale entries, and runs each
applicable unlisted test by exact name with fail-closed enabled. Source control
separately rejects ignored tests through `no_silent_skips`. Therefore:

1. Every new applicable integration test must pass fail-closed on its first CI
   run. It receives no exemption by default.
2. A regression in an enabled test is a release blocker.
3. When a syscall is modeled, remove the affected known-failure rows in the
   same change. The tests then join the enabled set automatically.
4. Adding a known failure expands debt and requires explicit review with a
   concrete syscall or hardware reason. It is not a routine way to make CI green.
5. Changes to the exception list are part of the ratchet's review surface.
   Counts may only move from failure to pass unless expansion is deliberately
   approved.

The self-hosted CI job runs the ratchet after the regular Hermit integration
suite when mount namespaces are available.

With the human-review stack applied, the ratchet reports 9 enabled tests, 96
documented failures, no ignored tests, and 257 mode-N/A tests. The additional
documented failures expose `rseq` as the next blocker for threaded strict-mode
workloads after `pread64` support.

## Current Limitation

This metric is a lower bound on unsupported-syscall exposure, not a claim of
complete fail-closed enforcement. Optimized Detcore runs subscribe to selected
syscalls. An unsubscribed syscall executes in the kernel without reaching the
unsupported-syscall panic. The current coverage audit identifies 291 such
missing release entries; see
[`ai_docs/syscall-coverage-map.md`](../ai_docs/syscall-coverage-map.md).

A future true fail-closed mode must subscribe to all syscalls (or install an
equivalent deny policy). Until then, the ratchet prevents regressions in the
calls that Detcore does observe and provides a visible path from 9/105 to full
coverage of the currently applicable integration inventory.
