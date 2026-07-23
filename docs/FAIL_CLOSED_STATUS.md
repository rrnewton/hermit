# Fail-Closed Test Status

Status: all catalogued strict failures closed; ratchet fully green (110/110 enabled), 2026-07-22

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

Debug builds subscribe to every syscall through `Subscription::all()`, so any
syscall without an explicit Detcore handler reached the fail-closed panic arm.
The latest batch gives intentional, kernel-backed dispositions to the remaining
trapped calls, closing the 20 previously catalogued strict failures and every
later blocker exposed once those cleared:

- process/thread/user identity (`getuid`/`geteuid`/`getgid`/`getegid`/
  `getresuid`/`getresgid`/`getgroups`/`getppid`/`getpgrp`/`getpgid`/`getsid`/
  `getcwd`/`capget`/`capset`), resource limits (`getrlimit`/`setrlimit`),
  scheduling priority (`getpriority`/`setpriority`), advisory hints
  (`membarrier`/`process_madvise`), filesystem mutation and introspection
  (`mkdir`/`mkdirat`/`rmdir`/`unlink`/`unlinkat`/`statfs`/`fstatfs`/`chdir`/
  `fchdir`/`chown`/`fchown`/`lchown`/`fchownat`/`chmod`/`fchmod`/`fchmodat`/
  `umask`/`rename`/`renameat`/`renameat2`/`link`/`linkat`/`symlink`/
  `symlinkat`/`truncate`/`ftruncate`/`fsync`/`fdatasync`/`sync`/`syncfs`/
  `fallocate`/`flock`), signal generation and query
  (`kill`/`tgkill`/`tkill`/`rt_sigpending`/`rt_sigsuspend`), interval and POSIX
  timers (`setitimer`/`getitimer`/`timer_create`/`timer_settime`/
  `timer_gettime`/`timer_getoverrun`/`timer_delete`), and socket configuration
  and introspection (`setsockopt`/`getsockopt`/`getsockname`/`getpeername`/
  `listen`/`shutdown`) all pass through, matching the behavior of an optimized
  release build where they are unsubscribed and run in the kernel.
- `clock_settime` returns `EPERM` while time is virtualized so the guest can
  never mutate the shared virtual/host clock; the virtualized clock reads
  (`clock_gettime`/`clock_getres`/`gettimeofday`/`time`) now pass through when
  time virtualization is disabled (for example under `--strace-only`) instead of
  panicking.

A full serial `./scripts/test-fail-closed.sh` run now passes: 110 enabled
tests, 0 known failures, 23 ignored, 30 mode N/A. The known-failure manifest is
empty.

| Test target or category | Fail-closed pass | Known failure | Ignored | Mode N/A |
| --- | ---: | ---: | ---: | ---: |
| `analyze` | 0 | 0 | 3 | 0 |
| `arbitrary_binaries` | 3 | 0 | 0 | 1 |
| `cli` | 0 | 0 | 0 | 10 |
| `clock_determinism` | 2 | 0 | 0 | 0 |
| `compression` | 1 | 0 | 0 | 0 |
| `epoll_determinism` | 5 | 0 | 0 | 0 |
| `fp_reduction_determinism` | 2 | 0 | 0 | 0 |
| `hashseed_determinism` | 1 | 0 | 0 | 0 |
| `hermit_modes` | 65 | 0 | 10 | 0 |
| `integration_matrix` | 1 | 0 | 0 | 1 |
| `ipc_determinism` | 1 | 0 | 0 | 0 |
| `language_runtime_determinism` | 0 | 0 | 6 | 0 |
| `mmap_determinism` | 5 | 0 | 0 | 0 |
| `procfs_determinism` | 6 | 0 | 0 | 0 |
| `python_stdlib` | 1 | 0 | 1 | 0 |
| `random_determinism` | 1 | 0 | 0 | 0 |
| `record_replay` | 0 | 0 | 0 | 17 |
| `signal_determinism` | 10 | 0 | 0 | 0 |
| `stress_suite` | 2 | 0 | 3 | 0 |
| `thread_scheduling_fairness` | 3 | 0 | 0 | 0 |
| `thread_sync_determinism` | 1 | 0 | 0 | 0 |
| Hermit library and binary unit tests | 0 | 0 | 0 | 33 |
| **Total** | **110** | **0** | **23** | **30** |

The ratchet now enables 110 fail-closed integration tests. The exact enabled
set is the applicable inventory not present in either exception manifest; a full
serial ratchet run verifies all 110.

The complete test-level matrix is represented by the table plus these
machine-readable exception lists:

- [`fail_closed_known_failures.tsv`](../hermit-cli/tests/fail_closed_known_failures.tsv)
  records every failing target/test pair and its first observed blocker. It is
  now empty: all previously catalogued blockers are modeled.
- [`fail_closed_allowed_ignores.tsv`](../hermit-cli/tests/fail_closed_allowed_ignores.tsv)
  records the PMU-dependent mode tests, the explicit stress tiers, and the
  optional-toolchain language-runtime/CPython tests that are `#[ignore]`d in
  source.
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
calls that Detcore does observe; the currently applicable integration inventory
(110 enabled tests) now passes in full.
