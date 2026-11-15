---
schema: syscall-pages/v1
syscall: gettimeofday
nr_x86_64: 96
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+gettimeofday&body=Discussion+for+the+%60gettimeofday%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgettimeofday.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60gettimeofday%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 5
---

## Summary

`gettimeofday(tv, tz)` writes the wall-clock time as seconds and microseconds into `tv`, and the kernel's timezone record into `tz`. With virtual time on (the default), Detcore lets the host call run and then overwrites `tv` with the guest's virtual clock (`detcore/src/syscalls/time.rs:167`). With `--no-virtualize-time`, and in record/replay, the call goes to the host and the value is recorded (`detcore/src/lib.rs:2447`).

Manual reference: Linux man-pages gettimeofday(2).

## Linux semantics

The kernel reads `CLOCK_REALTIME` and truncates it to microseconds. Each argument is independent and may be NULL. The kernel handles `tv` first: it copies the time out and returns `EFAULT` if that copy fails, before it looks at `tz`. Then it copies the global `sys_tz` record (minutes west of Greenwich and the obsolete DST type) to `tz`. `sys_tz` is host state, set only by `settimeofday(NULL, tz)` with `CAP_SYS_TIME`, and is normally zero.

glibc normally serves `gettimeofday(3)` from the vDSO without entering the kernel. With a bad pointer, the vDSO faults with `SIGSEGV` where the syscall would return `EFAULT`. The ptrace backend replaces the vDSO entry with a real syscall, so under Hermit the libc wrapper reaches the handler as well.

### Flags and options

- F1: `tv` non-NULL: filled with the current `CLOCK_REALTIME` time, truncated to microseconds
- F2: `tv` NULL: no time is written; the call still succeeds and still handles `tz`
- F3: `tz` non-NULL: filled with the kernel's `sys_tz` (`tz_minuteswest`, `tz_dsttime`), host state that is normally all zeros
- F4: `tz` NULL: the usual call; nothing is written for the timezone
- F5: a bad `tv` or `tz` pointer gives `EFAULT`. The kernel writes `tv` before it checks `tz`, so a valid `tv` with a bad `tz` returns `EFAULT` after `tv` has been written.
- F6: the vDSO path: glibc's wrapper normally runs `__vdso_gettimeofday` in user space; under the ptrace backend that entry is replaced by the real syscall

## Determinism analysis

`linux_determinism: nondeterministic` (changed from the rendered `conditional`). Every successful call with `tv` non-NULL returns the live wall clock, which differs on every run and between calls. Only the `tv` NULL case repeats, and its `tz` result still reads host state.

### Nondeterminism sources

- The host wall clock: absolute value, rate, and NTP or administrator adjustments
- The timing of the call relative to the rest of the program
- The host's `sys_tz` record, copied into `tz` (F3)

## Hermit design

Target policy: `model` (changed from `investigate`). The guest sees a virtual wall clock derived from the configured epoch and Detcore's logical time. The code at this SHA mostly realizes it:

- With virtual time on, the typed arm at `detcore/src/lib.rs:2447` calls `handle_gettimeofday`. The handler samples the virtual clock first (`guest_clock_time`, `detcore/src/syscalls/time.rs:172`), lets the host call run through `record_or_replay` (`detcore/src/syscalls/time.rs:174`; in run mode that tool injects the real syscall, `detcore/src/record_or_replay.rs:47`), then overwrites `tv` with the virtual time when `tv` is non-NULL (`detcore/src/syscalls/time.rs:179`). F1 and F2 are modelled, and a bad `tv` still gets the kernel's `EFAULT` (F5).
- With `--no-virtualize-time`, the same arm passes the call through (`detcore/src/lib.rs:2451`, `passthrough` at `detcore/src/lib.rs:367`), so the guest reads the host clock. Record/replay never virtualizes time (header comment of `hermit-cli/tests/clock_passthrough.rs:9`): the recording keeps the host value and replay returns it.
- The epoch is the one intentionally nondeterministic input of `hermit run`. When `--epoch` is omitted, it is sampled once from the host clock before the guest starts and given to both halves of `--verify` (`capture_default_epoch`, `hermit-cli/src/bin/hermit/run.rs:3026`; the config field is documented at `detcore-model/src/config.rs:205`). So a single `--verify` run compares identical values, but two separate `hermit run` invocations see different absolute times unless `--epoch` is pinned.
- F6: the ptrace backend turns the vDSO entry into a real syscall (`reverie-ptrace/src/vdso.rs` in the Reverie checkout, outside this export), so the libc wrapper is handled the same way as the raw call.

What remains before the code fully realizes `model`, both from reading the code and not measured:

- **`tz` is passed through (F3).** The handler never writes `tz`, so the guest receives the host's `sys_tz`. That is almost always zero, but it is host state. Writing zeros when `tz` is non-NULL would make it a model.
- **Host time can leak on the error path (F5).** If `tv` is valid and `tz` is bad, the host call writes host time into `tv` and returns `EFAULT`. The `?` at `detcore/src/syscalls/time.rs:174` returns before the overwrite, so the guest's `tv` keeps the real host time.

`passthrough` would expose the host clock, and `refuse` would break almost every program, so neither is an alternative.

Target (owner decisions 13 and 6): `tv` reads virtual realtime, which is the clock the handler already writes (`detcore/src/syscalls/time.rs:172`, `detcore/src/syscalls/time.rs:179`; the clock names are those defined in the `clock_gettime` body). The checks follow the kernel's order (`SYSCALL_DEFINE2(gettimeofday)` in `kernel/time/time.c`):

1. A non-NULL `tv` is written; an unwritable one gives `EFAULT` and nothing else happens.
2. Then a non-NULL `tz` is written; an unwritable one gives `EFAULT`, and `tv` stays written.

So a valid `tv` with a bad `tz` returns `EFAULT` with the virtual time in `tv`. Today the host call writes host time into `tv`, and the `?` at `detcore/src/syscalls/time.rs:174` returns `EFAULT` before the virtual overwrite at `detcore/src/syscalls/time.rs:179`, so the host clock leaks. Under the target the handler writes `tv` from virtual realtime before it handles `tz`, and the host call never writes `tv`.

## Recommended tests

- T1 (covers F1, F4): repeated raw and libc `gettimeofday(&tv, NULL)` calls under `hermit run --verify` on ptrace with a pinned `--epoch` give identical output across runs and stay within the bounds of neighbouring `clock_gettime(CLOCK_REALTIME)` reads.
  Existing candidates: `clock_apis_are_deterministic_across_five_runs` at `hermit-cli/tests/clock_determinism.rs:290`, whose guest `tests/c/clock_determinism.c:79` checks the bounds; cell `system-utils/clock-determinism` (`tests/e2e/manifests/system-utils.yaml:4`); and `explicit_virtual_epoch_reproduces_identical_observed_time` at `hermit-cli/tests/clock_determinism.rs:266`.
- T2 (covers F1): without virtual time, and in record/replay, the guest reads the host clock and replay returns the recorded value.
  Existing candidates: `run_without_virtual_time_reads_the_host_clock` at `hermit-cli/tests/clock_passthrough.rs:170` and `record_captures_host_clock_reads_and_replay_returns_them` at `hermit-cli/tests/clock_passthrough.rs:126`; the guest calls `syscall(SYS_gettimeofday, &tv, NULL)` at `tests/c/clock_passthrough.c:66`.
- T3 (covers F2, F3): `gettimeofday(NULL, &tz)` and `gettimeofday(&tv, &tz)` succeed, and `tz` reads as all zeros under Hermit. The second assertion fails at this SHA if the host `sys_tz` is non-zero.
- T4 (covers F5): with a raw syscall, a bad `tv` returns `EFAULT`, and a valid `tv` with a bad `tz` returns `EFAULT`. In the second case, assert that `tv` holds the virtual time, as Linux leaves `tv` written and the target requires (owner decision 6), never the host time; that assertion should fail at this SHA.
- T5 (covers F6): call glibc `gettimeofday` (the vDSO path) and the raw syscall back to back on ptrace and check that both read the same virtual clock.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (virtual wall clock overwrites tv after the host call); linux_determinism conditional -> nondeterministic (live wall clock); 6 flag behaviours and 5 tests listed; gaps: tz is passed from the host and a valid tv with a bad tz leaks host time on EFAULT; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 (tv reads virtual realtime, as today) and 6 (kernel order tv then tz: a bad tz returns EFAULT with the virtual time already in tv, closing the host-time leak at the ? on time.rs:174); target_policy stays model; no new F/T ids (T4 now asserts the virtual time); removed the open question on writing tv before the tz EFAULT

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `gettimeofday`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+gettimeofday&body=Discussion+for+the+%60gettimeofday%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgettimeofday.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60gettimeofday%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `gettimeofday`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should the handler write a fixed `tz` (all zeros) instead of passing the host's `sys_tz` through? That is the smallest change that makes F3 a model.
