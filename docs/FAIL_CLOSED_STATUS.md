# Fail-Closed Test Status

Status: unsupported syscalls fail closed by default, 2026-08-04

An unsupported syscall that reaches Detcore terminates an ordinary `hermit run`
instead of silently passing through to the host. The explicit
`--allow-unsupported-syscalls` compatibility opt-out restores legacy host
forwarding, emits a warning, and cannot support a deterministic-execution claim.
`HERMIT_FAIL_CLOSED=1` remains accepted by the integration ratchet for backward
compatibility, but it now selects the command-line default.

## Baseline

The 2026-08-04 runtime inventory lists 657 integration tests: 277 are applicable
to this policy, 279 are ignored, and 101 exercise modes that do not use
Detcore's `hermit run` syscall policy. Four applicable exceptions remain for PMU
or scheduler failures. No valid exception is an unsupported syscall: **0/277
applicable cells depend on silently forwarding one**. A stale SQLite `fchown`
row referred to a renamed test and was removed with the default change; the
current SQLite cell passes fail closed.

The exact enabled set is the applicable inventory not present in either
exception manifest. The runner discovers and counts that set at execution time
rather than relying on a historical hand-maintained table. The exception lists
are:

- [`fail_closed_known_failures.tsv`](../hermit-cli/tests/fail_closed_known_failures.tsv)
  records every failing target/test pair and its first observed blocker.
- [`fail_closed_allowed_ignores.tsv`](../hermit-cli/tests/fail_closed_allowed_ignores.tsv)
  records every ignored applicable test and its concrete environment or runtime prerequisite.
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

Portable CI runs the ratchet after the regular Hermit integration suite when
mount namespaces are available.

## Current Limitation

This metric is a lower bound on unsupported-syscall exposure, not a claim of
complete fail-closed enforcement. Optimized Detcore runs subscribe to selected
syscalls. An unsubscribed syscall executes in the kernel without reaching the
unsupported-syscall panic. The current coverage audit identifies 291 such
missing release entries; see
[`ai_docs/syscall-coverage-map.md`](../ai_docs/syscall-coverage-map.md).

A future true fail-closed mode must subscribe to all syscalls (or install an
equivalent deny policy). Until then, the ratchet prevents regressions in the
calls that Detcore does observe and provides a visible path to full coverage of
the currently applicable integration inventory.
