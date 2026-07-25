# Fail-Closed Test Status

Status: all currently applicable integration tests pass fail-closed, 2026-07-22

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

Detcore now handles `pread64` deterministically and disables `rseq` by
returning `ENOSYS`. Batch two adds resource-ordered `lseek` passthrough, fixed
success for advisory `fadvise64`, and PID-namespace `getpid`/`gettid` results.
Batch three adds explicit handling for `ioctl`, `tgkill`, `mkdir`, `setitimer`,
`clock_settime`, `getrlimit`, `kill`, and `setsockopt`. Batch four closes the
final 13 exceptions with namespace-stable identity calls, ordered socket and
filesystem operations, kernel-backed pending-signal queries, and explicit
resource, timer, and arbitrary-binary support. The measured applicable pass
count is now 89/89 with no known failures.

| Test target or category | Fail-closed pass | Known failure | Ignored | Mode N/A |
| --- | ---: | ---: | ---: | ---: |
| `arbitrary_binaries` | 1 | 0 | 0 | 1 |
| `cli` | 0 | 0 | 0 | 10 |
| `clock_determinism` | 1 | 0 | 0 | 0 |
| `epoll_determinism` | 5 | 0 | 0 | 0 |
| `hermit_modes` | 63 | 0 | 8 | 0 |
| `ipc_determinism` | 1 | 0 | 0 | 0 |
| `mmap_determinism` | 5 | 0 | 0 | 0 |
| `procfs_determinism` | 6 | 0 | 0 | 0 |
| `random_determinism` | 1 | 0 | 0 | 0 |
| `record_replay` | 0 | 0 | 0 | 17 |
| `signal_determinism` | 5 | 0 | 0 | 0 |
| `stress_suite` | 0 | 0 | 3 | 0 |
| `thread_sync_determinism` | 1 | 0 | 0 | 0 |
| Hermit library and binary unit tests | 0 | 0 | 0 | 33 |
| **Total** | **89** | **0** | **11** | **61** |

The ratchet now enables all 89 applicable fail-closed integration tests. A
full serial ratchet run verifies the complete enabled inventory.

The complete test-level matrix is represented by the table plus these
machine-readable exception lists:

- [`fail_closed_known_failures.tsv`](../hermit-cli/tests/fail_closed_known_failures.tsv)
  records failing target/test pairs and their first observed blockers. It now
  contains no debt rows: the `pread64` and `rseq` work raised coverage from 3
  to 36 tests, batch two reached 69, batch three reached 76, and batch four
  enabled the remaining 13 tests.
- [`fail_closed_allowed_ignores.tsv`](../hermit-cli/tests/fail_closed_allowed_ignores.tsv)
  records the eight PMU-dependent mode tests and three explicit stress tiers.
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
both exception files, rejects duplicate or stale entries, rejects new ignored
tests, and runs each applicable unlisted test by exact name with fail-closed
enabled. Therefore:

1. Every new applicable integration test must pass fail-closed on its first CI
   run. It receives no exemption by default.
2. A regression in an enabled test is a release blocker.
3. When a syscall is modeled, remove the affected known-failure rows in the
   same change. The tests then join the enabled set automatically.
4. Adding a known failure or allowed ignore expands debt and requires explicit
   review with a concrete syscall or hardware reason. It is not a routine way
   to make CI green.
5. Changes to either exception list are part of the ratchet's review surface.
   Counts may only move from failure/ignored to pass unless expansion is
   deliberately approved.

The self-hosted CI job runs the ratchet after the regular Hermit integration
suite when mount namespaces are available.

## Current Limitation

This metric is a lower bound on unsupported-syscall exposure, not a claim of
complete fail-closed enforcement. Optimized Detcore runs subscribe to selected
syscalls. An unsubscribed syscall executes in the kernel without reaching the
unsupported-syscall panic. The current coverage audit identifies 291 such
missing release entries; see
[`ai_docs/syscall-coverage-map.md`](../ai_docs/syscall-coverage-map.md).

A future true fail-closed mode must subscribe to all syscalls (or install an
equivalent deny policy). Until then, the ratchet prevents regressions in the
calls that Detcore does observe and holds coverage at 89/89 for the currently applicable integration inventory.
