# Fail-closed default: Round 3 scope

Date: 2026-08-17

Measured Hermit commit: `cd9bd9e6b5fc7a5c1d709d17de9740ef01c970b1`
(`origin/main` at the time of the measurement)

This is a read-only scope of the owner's Round 3 directive. The experiment
changed the local `panic_on_unsupported_syscalls` clap default to `true`, ran
the brackets described below, and restored the source afterward. The
experimental flip was not committed.

## Conclusion

**This is a Round-3 product item, not a flag flip, and it costs zero envelope
today.**

The current 283 green cells already select fail-closed behavior through
`--strict`. Changing the ordinary-run default therefore changes none of those
cells. It does change deliberately supported non-strict compatibility behavior,
including real keyring and zero-copy pipe workloads. Round 3 must implement the
needed syscall behavior and preserve an explicit compatibility path for modes
that intentionally expose host time before changing the default.

## Owner directive

The owner's directive was:

> Round 3 is full sanity where we also fail closed by default and fix any tests
> that depend on unsupported syscalls by adding those syscalls. This may be a
> second score drop but we should be able to ratchet these back up first.

The second clause is essential. The acceptance condition is not merely changing
the default; it is implementing the syscall support needed by real programs and
then measuring the envelope again.

## Current envelope cost: 0 of 283

The scorecard at the measured commit contained 283 green cells:

| Mode | Green cells | Runner construction |
| --- | ---: | --- |
| verify | 280 | `ci/manifest-plan/src/runner.rs:714-746`; `--strict` at line 723 |
| replay | 1 | `ci/manifest-plan/src/runner.rs:748-774`; `--strict` at line 757 |
| chaos | 2 | `ci/manifest-plan/src/runner.rs:776-803`; `--strict` at line 786 |
| **Total** | **283** | **All three paths are already fail-closed** |

Consequently, changing the ordinary-run default alone drops **0 of 283** green
cells. This does not mean the product change is small: it means the current
green envelope already tests the stricter policy and therefore does not measure
the non-strict compatibility behavior that the default change removes.

## The open default was deliberate

`git blame` on `hermit-cli/src/bin/hermit/run.rs:1110-1125`, including the
assertion at line 1113 that ordinary run is open by default, leads to commit
`bf1cab333bdb50aeeb952d7df9e7d586687153b0`, PR #644, dated 2026-07-25:
**“Fail unsupported syscalls in explicit strict mode.”**

Its stated policy was:

> In normal mode, pass audited unsupported syscalls through and emit one
> sorted, deduplicated aggregate warning; explicit `--strict` and
> `--panic-on-unsupported-syscalls` fail immediately across the ptrace and DBI
> process trees, and ptrace `--verify` re-emits one aggregate warning.

The same commit explicitly records a real compatibility cost:

> Relax the DBI shell/pipe and KVM `ls` regressions from strict to verify only,
> since those real programs use now-fail-closed unsupported syscalls.

The open default is therefore not an accidental missing annotation. PR #644
made it a compatibility boundary and pinned that policy in a unit test. Round 3
deliberately supersedes that endpoint, but must account for the workloads the
earlier change protected.

## Classification is exhaustive

At the measured commit, the x86-64 syscall table contains:

| Classification | Count |
| --- | ---: |
| Determinized | 289 |
| PassThrough | 83 |
| Unsupported | 1 |
| Unclassified | 0 |

The sole explicitly `Unsupported` syscall is `restart_syscall`
(`detcore/src/syscall_classification.rs:815-823`). The exhaustive census is
enforced by `every_pinned_sysno_has_an_explicit_classification`
(`detcore/src/syscall_classification.rs:1749-1773`); the external-enum wildcard
panics instead of silently introducing an unclassified syscall
(`detcore/src/syscall_classification.rs:825-828`).

This classification count must not be confused with the default-sensitive
surface below. Thirteen syscalls are classified `Determinized` but intentionally
take a different path when fail-closed behavior is enabled.

## Default-sensitive surface: 1 Unsupported + 13 Determinized

| Classification | Syscalls | Why the default matters |
| --- | --- | --- |
| Unsupported | `restart_syscall` | Ordinary mode uses the configured unsupported-syscall fallback. |
| Determinized | `rseq` | Passes through in non-strict mode and returns deterministic `ENOSYS` in fail-closed mode (`detcore/src/lib.rs:1672-1681`). |
| Determinized | `add_key`, `request_key`, `keyctl` | Non-strict compatibility reaches the host keyring; strict mode returns deterministic `ENOSYS` (`detcore/src/syscall_classification.rs:1176-1193`, `detcore/src/lib.rs:1753-1768`). |
| Determinized | `splice`, `tee`, `vmsplice` | Non-strict compatibility reaches kernel pipe state; strict mode returns deterministic `ENOSYS` (`detcore/src/syscall_classification.rs:1157-1174`, `detcore/src/lib.rs:1769-1781`). |
| Determinized | `gettimeofday`, `time`, `clock_gettime`, `clock_getres`, `adjtimex`, `clock_adjtime` | They use deterministic handlers while time is virtualized, but route through unsupported-syscall policy when `--no-virtualize-time` is selected (`detcore/src/lib.rs:2033-2083`, `2095-2123`). |

The strict-only deterministic-refusal membership is explicitly the union of
`rseq`, the three keyring calls, and the three zero-copy pipe calls
(`detcore/src/syscall_classification.rs:1250-1256`). The six clock calls are a
separate compatibility case: they are implemented under normal virtual time and
become default-sensitive only when the user explicitly disables that model.

## Local default-flip measurement

The experiment changed only:

```rust
#[clap(long, default_value_t = true)]
pub panic_on_unsupported_syscalls: bool,
```

The source was restored after measurement.

### CLI unit suite

`cargo test -p hermit --bin hermit` changed from:

```text
baseline:       138 passed, 0 failed
flipped default: 122 passed, 16 failed
```

Those 16 failures are **not 16 tests that execute unsupported syscalls**. This
unit-test target parses, validates, and renders `RunOpts`; it does not run guest
programs. The changed default makes `Config::Display` include
`--panic-on-unsupported-syscalls` (`detcore-model/src/config.rs:798-800`), so
argument-rendering expectations change. The failures also include:

- the direct policy pin asserting that ordinary run is open by default
  (`hermit-cli/src/bin/hermit/run.rs:1110-1125`); and
- the validation bracket for `--passthru-opt`, whose meaning conflicts with a
  fail-closed default unless an explicit compatibility choice is made
  (`hermit-cli/src/bin/hermit/run.rs:1139-1146`).

The 16 failures measure CLI/default-policy migration work. They must not be
reported as 16 syscall-dependent workloads.

### Behavior tests that depend on the open default

Seven behavior tests or subcases were measured as changing under the flip:

| Test or subcase | Current behavior required | Syscall evidence |
| --- | --- | --- |
| `kernel_keyring_passes_through_in_non_strict_mode` | Non-strict Hermit matches the native host result. | `keyctl`; the rr guest also requires `add_key` + `keyctl`. |
| `zero_copy_pipe_syscalls_fall_back_only_in_strict_mode` | Non-strict compatibility path succeeds. | `splice`, `tee`, `vmsplice`. |
| `run_dbt_aggregates_unsupported_syscalls_and_strict_rejects_them` | Non-strict DBT aggregates a warning instead of terminating. | `restart_syscall`. |
| host-clock subcase of `socket_receive_timestamps_use_logical_time` | `--no-virtualize-time` reaches host time. | `clock_gettime`. |
| ignored/occasional `rr_keyctl` | rr keyring compatibility succeeds. | `add_key`, `keyctl`. |
| ignored/occasional `rr_splice` | rr zero-copy pipe compatibility succeeds. | `splice`. |
| ignored/occasional `meaningful_flag_combinations_run_without_crashing` | Relaxed time-off configurations remain valid. | `clock_gettime`; 16 relaxed matrix cases become invalid under the flip. |

The measured unique syscall set was:

```text
restart_syscall
add_key keyctl
splice tee vmsplice
clock_gettime (only with --no-virtualize-time)
```

`request_key` and the other five clock calls are part of the same
default-sensitive policy surface even though this measurement did not find a
current test that required their open path. Current tests may also accept
deterministic `ENOSYS` for `request_key`; that is not evidence that deterministic
keyring semantics have been implemented.

## Per-syscall Round 3 work

The owner's instruction to add the syscalls translates into these product work
items:

1. **`restart_syscall`: DBT handling.** The sole explicitly Unsupported syscall
   needs correct subscription and backend handling rather than a default
   pass-through or unconditional refusal.
2. **Kernel keyrings: deterministic host-shared key state.** `add_key`,
   `request_key`, and `keyctl` expose shared kernel keyrings, serials, quotas,
   permissions, contents, and user-space upcalls. Implementing this family is
   more than choosing a stable errno if real rr keyring behavior must remain
   supported.
3. **Zero-copy pipes: deterministic blocking, pipe-buffer ownership, and guest
   page lifetime.** `splice`, `tee`, and `vmsplice` cannot be made deterministic
   by forwarding them unchanged; `vmsplice` can pin guest pages beyond the
   syscall boundary.
4. **`--no-virtualize-time`: explicit compatibility opt-out.** The six clock
   calls already have deterministic implementations while time virtualization
   is enabled. An explicit mode that asks for host time needs an explicit
   compatibility choice; a global fail-closed default must not silently turn
   that documented opt-out into termination.

### Reverie #443 is the `restart_syscall` thread

Open Reverie PR `rrnewton/reverie#443`, commit
`e7972364634aae3ef62705527c70a1c0556c5784`, is titled **“Honor
restart_syscall subscriptions.”** At the time of this scope it was open and
cleanly merging. It removes ptrace's unconditional `restart_syscall` allow rule
so the Tool subscription decides whether the call reaches Detcore, while
retaining `rt_sigreturn` for Reverie's private signal-frame restoration path.

That PR and this Round 3 scope are the same work thread: `restart_syscall` is the
one explicitly Unsupported syscall in the exhaustive Hermit table, and a
fail-closed default is ineffective if a backend bypasses the Tool subscription
before Detcore can apply the policy. The PR improves subscription correctness;
it does not by itself implement the syscall behavior required by the owner's
instruction to add needed syscall support.

## Acceptance shape

Round 3 should be measured as a product transition, not accepted from a changed
default alone:

- implement the needed `restart_syscall`, keyring, and zero-copy pipe behavior;
- define the explicit compatibility behavior for `--no-virtualize-time`;
- invert the unit-test policy pin and update argv rendering intentionally;
- retain positive healthy controls and negative unsupported-syscall brackets
  across run and record/replay;
- verify that ptrace and every other in-scope backend actually route the calls
  through the shared policy;
- rerun the full envelope under the new default and review any score change.

The starting envelope cost is **0 of 283** because every currently green cell
already uses `--strict`. The implementation cost is nevertheless substantial,
and the historical compatibility policy was deliberate. This is why the work
belongs to Round 3 rather than an overnight flag flip.
