# Ptrace `--verify` examples from three manifest cells

This document records commands and output from three ptrace manifest cells examined on
2026-08-27. The fresh runs used:

- source tree `c370c1371a1507ff46463596e216af7944250de5`;
- `target/release/hermit` built from ancestor
  `9e949d945872a52340b3b0b4a46cee50956b7955`; and
- the host kernel and machine available for that run.

The commands below are written from the repository root. They create ignored scratch
directories so the guest executable is visible inside Hermit's isolated `/tmp`.
Counts and failure locations are observations from this run, not constants that a later
binary or host must reproduce.

## What a clean `--verify` result does not establish

The syscall comparison checks recorded syscall return values. It does not, by itself,
compare every byte written through pointer arguments or every byte another writer can put
on a shared channel. `--verify-strict` compares the canonical INFO stream, and current
Hermit can add hashes for supported syscall buffers when `compare_io_buffers=true`, but
the result still covers only what was recorded.

This limitation has been measured in this project. A netlink `recvmsg` returned `1468` in
both executions, so `--verify` reported `bitwise_parity=true`, while `--detlog-stack`
showed that the received content differed. See
[Classifying a `--verify` divergence](DIVERGENCE_CLASSES.md#3-pure-observation).

Accordingly, this document reports the comparison fields alongside every clean result.
“Matched” means that the named comparison matched; it is not an unrestricted claim that
all guest-visible state was identical.

## `c-programs/dbt-unsupported-syscall`, `verify/ptrace`

### What the guest does

[`tests/c/dbt_unsupported_syscall.c`](../tests/c/dbt_unsupported_syscall.c) invokes
`restart_syscall` directly. Strict ptrace execution refuses that unsupported syscall.
This is a refusal before verification completes, not a two-run divergence.

### Runnable command

Prepare the guest:

```bash
mkdir -p ignored
scratch=$(mktemp -d "$PWD/ignored/ptrace-restart-syscall.XXXXXX")
mkdir -p "$scratch/home" "$scratch/xdg-config" "$scratch/fixtures"
cc -std=c11 -O2 -g -Wall -Wextra -Werror tests/c/dbt_unsupported_syscall.c -o "$scratch/guest"
```

Run the cell's verification command:

```bash
env LC_ALL=C TZ=UTC HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/xdg-config" E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR="$scratch/fixtures" target/release/hermit --log info run --base-env=minimal --backend ptrace --strict --verify-strict --verify --verify-json "$scratch/verify.json" --env LC_ALL=C --env TZ=UTC --env HOME="$scratch/home" --env XDG_CONFIG_HOME="$scratch/xdg-config" --env E2E_TMPDIR=/tmp/hermit-e2e --env E2E_FIXTURE_DIR="$scratch/fixtures" -- "$scratch/guest"
run_rc=$?
printf 'exit=%s\n' "$run_rc"
jq . "$scratch/verify.json"
```

### Real output

```text
:: Run1...
HERMIT_POLICY_REFUSAL class=policy-refusal
Error: Hermit refused the run: a fail-closed policy stopped it before completion.
exit=122
```

The report was:

```json
{
  "verified": false,
  "bitwise_parity": false,
  "verdict": "no_result",
  "no_result_reason": { "kind": "not_run" },
  "comparison": null,
  "compared_log_messages": null,
  "first_divergent_scheduler_turn": null,
  "first_divergent_virtual_nanoseconds": null,
  "first_divergent_record": null,
  "first_divergent_syscall": null
}
```

The verification wrapper reports the refusal but not the unsupported syscall name. The
same strict ptrace execution without `--verify` exposes the underlying event:

```bash
env LC_ALL=C TZ=UTC HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/xdg-config" E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR="$scratch/fixtures" target/release/hermit --log info run --base-env=minimal --backend ptrace --strict --env LC_ALL=C --env TZ=UTC --env HOME="$scratch/home" --env XDG_CONFIG_HOME="$scratch/xdg-config" --env E2E_TMPDIR=/tmp/hermit-e2e --env E2E_FIXTURE_DIR="$scratch/fixtures" -- "$scratch/guest"
```

```text
INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall #32: munmap(...) = Ok(0)
INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: restart_syscall() = ?
ERROR detcore: [detcore, dtid 3] unsupported syscall: restart_syscall() = ?
HERMIT_POLICY_REFUSAL class=policy-refusal
```

Calibration: 32 syscalls completed. The next inbound syscall was
`restart_syscall`, which was refused before it received a completed-syscall number.
Run 1 did not finish, so there was no reference pair, no comparison, and no divergence
coordinate.

The same run printed `ARCH_SET_CPUID returned ENODEV` and explicitly continued without
CPUID interception. That was a host limitation, not the terminal cause: execution
continued until the named `restart_syscall` refusal.

## `system-utils/procfs-sanitized-paths`, `verify/ptrace`

### What the guest does

[`tests/e2e/system-utils/procfs-sanitized-paths.sh`](../tests/e2e/system-utils/procfs-sanitized-paths.sh)
builds and runs one guest that reads eighteen procfs paths and checks the invariants
promised by their sanitizers.

This cell was examined because its historical qualification used
`--no-detlog-io-buffers` and `--no-rcb-time`. The historical failure without those
options did not reproduce in the fresh run below, so this section records a
non-reproduction rather than presenting the cell as a current failing example.

### Runnable command with the manifest options

Prepare the guest:

```bash
mkdir -p ignored
scratch=$(mktemp -d "$PWD/ignored/ptrace-procfs-sanitized.XXXXXX")
mkdir -p "$scratch/home" "$scratch/xdg-config" "$scratch/tmp" "$scratch/fixtures"
env LC_ALL=C TZ=UTC HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/xdg-config" E2E_TMPDIR="$scratch/tmp" E2E_FIXTURE_DIR="$scratch/fixtures" tests/e2e/system-utils/procfs-sanitized-paths.sh --prepare
```

Run with both options:

```bash
timeout --kill-after=2s 15s env LC_ALL=C TZ=UTC HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/xdg-config" E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR="$scratch/fixtures" target/release/hermit --log info run --base-env=minimal --backend ptrace --strict --verify-strict --no-detlog-io-buffers --no-rcb-time --verify --verify-json "$scratch/verify.json" --env LC_ALL=C --env TZ=UTC --env HOME="$scratch/home" --env XDG_CONFIG_HOME="$scratch/xdg-config" --env E2E_TMPDIR=/tmp/hermit-e2e --env E2E_FIXTURE_DIR="$scratch/fixtures" -- tests/e2e/system-utils/procfs-sanitized-paths.sh --run
run_rc=$?
printf 'exit=%s\n' "$run_rc"
jq . "$scratch/verify.json"
```

### Real output with the options

```text
Done processing logs, no substantive differences found (1247 | 1247 INFO messages compared).
:: comparison=BitwiseInfoV1 relaxations=--no-detlog-io-buffers
:: Success: deterministic. Determinism verified. NOTE: syscall output-buffer CONTENT was not compared because --no-detlog-io-buffers was given, so a divergence confined to a buffer whose length is stable would not have been seen; drop that flag to include it.
exit=0
```

The report said `verified=true`, `verdict=matched`, `bitwise_parity=false`, and
`compare_io_buffers=false`; every divergence coordinate was null. The banner must be read
with the warning it printed: output-buffer content was outside this comparison.

### Removing both options

The same invocation was run 30 times after removing `--no-detlog-io-buffers` and
`--no-rcb-time`. This is the full Hermit invocation used for each attempt; `attempt`
ranged from 1 through 30:

```bash
attempt=1
timeout --kill-after=2s 15s env LC_ALL=C TZ=UTC HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/xdg-config" E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR="$scratch/fixtures" target/release/hermit --log info run --base-env=minimal --backend ptrace --strict --verify-strict --verify --verify-json "$scratch/no-optouts-$attempt.json" --env LC_ALL=C --env TZ=UTC --env HOME="$scratch/home" --env XDG_CONFIG_HOME="$scratch/xdg-config" --env E2E_TMPDIR=/tmp/hermit-e2e --env E2E_FIXTURE_DIR="$scratch/fixtures" -- tests/e2e/system-utils/procfs-sanitized-paths.sh --run
jq . "$scratch/no-optouts-$attempt.json"
```

Fresh result: **30/30 matched**. The final attempt printed:

```text
Done processing logs, no substantive differences found (1334 | 1334 INFO messages compared).
:: comparison=BitwiseInfoV1 relaxations=none
:: Success: deterministic. Determinism verified.
```

Its JSON reported `verified=true`, `verdict=matched`, `bitwise_parity=true`,
`compare_io_buffers=true`, and null divergence coordinates. This did not reproduce the
older failure. Commit `af987f591778d3325c0615a2264ff86e351c0254` records that the
earlier experiment saw output-buffer comparison diverge after six clean repetitions and
RCB-derived time diverge after three clean repetitions, while both options passed 20/20.
The raw failing command output from that experiment was not retained, so this document
does not substitute a reconstructed historical result.

A fresh single ptrace run completed syscall 370, then invoked `exit_group`, ran 84
scheduler turns, and ended after 7.516250000 virtual seconds.

## `determinism-stress/order-violation`, `chaos/ptrace`

### What the guest does

[`tests/chaos/order_violation.c`](../tests/chaos/order_violation.c) starts three threads
that race on `global_str`. A seed can produce `Hello world!` with guest exit 0 or
`ERROR! global_str is null at use.` with guest exit 1. `--verify-allow=both` permits
either outcome class; it does not permit the two executions at one seed to differ.

### Runnable command

Prepare the guest:

```bash
mkdir -p ignored
scratch=$(mktemp -d "$PWD/ignored/ptrace-order-violation.XXXXXX")
mkdir -p "$scratch/home" "$scratch/xdg-config" "$scratch/fixtures"
cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread -Wno-unused-parameter tests/chaos/order_violation.c -o "$scratch/guest"
```

The following is the full Hermit invocation used for every seed. The measurement ran
seeds 0 through 31 for each of three attempts:

```bash
attempt=1
seed=0
timeout --kill-after=2s 15s env LC_ALL=C TZ=UTC HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/xdg-config" E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR="$scratch/fixtures" target/release/hermit --log info run --base-env=minimal --backend ptrace --strict --verify-strict --verify --verify-allow=both --verify-json "$scratch/attempt-$attempt-seed-$seed.json" --chaos --sched-heuristic=random --seed="$seed" --env LC_ALL=C --env TZ=UTC --env HOME="$scratch/home" --env XDG_CONFIG_HOME="$scratch/xdg-config" --env E2E_TMPDIR=/tmp/hermit-e2e --env E2E_FIXTURE_DIR="$scratch/fixtures" -- "$scratch/guest"
```

### Real output and pass count

The complete-cell result was **1 PASS out of 3 attempts**:

```text
attempt 1: PASS, 32/32 seed comparisons matched, 27 exit-0 outcomes and 5 exit-1 outcomes
attempt 2: FAIL, 31/32 seed comparisons matched
attempt 3: FAIL, 30/32 seed comparisons matched
```

Attempt 2 failed at seed 14:

```json
{
  "verified": false,
  "bitwise_parity": false,
  "verdict": "diverged",
  "compared_log_messages": { "left": 188, "right": 187 },
  "guest_exit_code": 1,
  "first_divergent_scheduler_turn": 9,
  "first_divergent_virtual_nanoseconds": 1767225600008835560,
  "first_divergent_record": 187,
  "first_divergent_syscall": 47
}
```

Attempt 3 failed at seeds 6 and 9. Both reports contained:

```json
{
  "verified": false,
  "bitwise_parity": false,
  "verdict": "diverged",
  "compared_log_messages": { "left": 174, "right": 175 },
  "guest_exit_code": 1,
  "first_divergent_scheduler_turn": 8,
  "first_divergent_virtual_nanoseconds": 1767225600008074490,
  "first_divergent_record": 174,
  "first_divergent_syscall": 8
}
```

The failure output named the comparison failure:

```text
Divergent syscall context:
  run 1, log message 178: INFO detcore: DETLOG [syscall][detcore, dtid 2] inbound syscall: futex(...) = ?
  run 2, log message 178: INFO detcore: DETLOG [syscall][detcore, dtid 2] inbound syscall: futex(...) = ?
Done processing logs, differences found.
:: Failure: nondeterministic.
HERMIT_INTERNAL_FAILURE class=cli-error
Error: Verification found a mismatch between run 1 and run 2 (logs retained).
```

This reproduces the instability that caused commit
`da85fa31e1c667a534d12414efbf94f738565174` to demote the cell. That historical
measurement passed 2 of 3 attempts; its failure was seed 15 at scheduler turn 9 with
185/186 INFO messages. A separate exact-head validation failed at seed 9, turn 8, with
173/174 INFO messages. The fresh run above is worse, at 1 of 3 passing attempts, and
provides retained JSON coordinates from the actual commands shown here.

A fresh single-run seed-15 reference completed syscall 47, ran 10 scheduler turns, and
ended after 9,335,560 virtual nanoseconds. The fresh divergences occurred at syscall 47
and syscall 8, respectively: one at the end of the reference-sized execution and one much
earlier.
