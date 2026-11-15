---
schema: syscall-pages/v1
syscall: time
nr_x86_64: 201
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+time&body=Discussion+for+the+%60time%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60time%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 12
---

## Summary

`time(tloc)` returns the wall-clock time in whole seconds since the Unix epoch and, if `tloc` is not NULL, also stores it there. Under virtual time Detcore models the call from the guest's virtual clock (`detcore/src/syscalls/time.rs:188`); without virtual time (record/replay, `--no-virtualize-time`) it forwards the call to the host through the record/replay path (`detcore/src/lib.rs:2454`), so a recording captures the host value and replay returns it.

Manual reference: Linux man-pages time(2).

## Linux semantics

The syscall returns `ktime_get_real_seconds()`: the seconds part of `CLOCK_REALTIME`, read from the timekeeper's coarse value, which is updated at each tick. It can therefore lag the seconds of a `clock_gettime(CLOCK_REALTIME)` taken at the same moment by up to one tick around a second boundary. The value follows every change to the realtime clock: `settimeofday`, `clock_settime`, NTP steps and slews, so it can go backwards. On x86_64 `time_t` is 64 bits, and a valid time never falls in the -4095..-1 range the ABI reserves for errors.

If `tloc` is not NULL, the kernel writes the same value there with `put_user`; if that write faults, the call returns `EFAULT`. No other error exists.

On x86_64, glibc's `time()` normally does not enter the kernel: it calls `__vdso_time` in the vDSO, which reads the same coarse seconds from the vDSO data page. Only a raw `syscall(SYS_time, ...)`, or a libc built without vDSO support, runs the syscall instruction.

### Flags and options

- F1: `tloc == NULL`: the value is only returned.
- F2: `tloc` points to writable memory: the value is stored there and also returned; the two are equal.
- F3: `tloc` points to memory that cannot be written: `EFAULT`.
- F4: invocation through the vDSO entry `__vdso_time` (how glibc's `time()` reaches it on x86_64): same result as the syscall, but no syscall instruction runs, so a tracer sees nothing unless the vDSO is replaced.

## Determinism analysis

`linux_determinism: nondeterministic` (conditional -> nondeterministic). The result is the host's wall clock, which differs between any two runs, and can be stepped by the administrator or NTP at any moment. Only the `EFAULT` path (F3) is decided by the arguments alone. Two calls in one run also differ by the elapsed host time, and whether they fall in the same second depends on host scheduling.

### Nondeterminism sources

- Host wall-clock progression between runs and between calls
- Clock steps and slews from `settimeofday`, `clock_settime`, `adjtimex` and NTP
- Tick granularity of the coarse realtime clock, which decides where a second boundary falls relative to `clock_gettime`
- The vDSO path (F4), which bypasses any syscall-level interception

## Hermit design

Target policy: `model`. With virtual time the guest reads Detcore's virtual clock, which starts at the configured epoch and advances by modeled syscall costs, not host time. Code at hermit f4055eb9cae4:

- Dispatch (`detcore/src/lib.rs:2454`): with `virtualize_time` the call goes to `handle_time` (`detcore/src/syscalls/time.rs:188`); otherwise to `passthrough` (`detcore/src/lib.rs:2458`), which is `record_or_replay_preserving_tool_errors` (`detcore/src/lib.rs:372`). The comment above the clock arms (`detcore/src/lib.rs:2444`) states the rule: without virtual time each clock read needs a record/replay handler, so a recording captures the value the guest saw and replay returns it.
- `handle_time` reads the clock through `guest_clock_time` (`detcore/src/syscalls/time.rs:193`), truncates the nanoseconds to whole seconds (`detcore/src/syscalls/time.rs:194`), writes them to `tloc` when it is not NULL (`detcore/src/syscalls/time.rs:196`), and returns them (`detcore/src/syscalls/time.rs:201`). That gives F1 and F2. A failed write is returned to the guest through `?` (`detcore/src/syscalls/time.rs:198`), which is F3; the exact errno for an unmapped `tloc` on ptrace has not been checked here (T2).
- `time` is charged a fixed 10 µs of virtual time (`CLOCK_NS`, `detcore/src/syscall_time.rs:30`) and is listed as a clock read (`detcore/src/syscall_time.rs:216`), so it is charged even inside a backend bootstrap window, and successive reads advance.
- F4: the ptrace backend replaces the vDSO's `time` entry with a real `time` syscall whenever the tool subscribes to `time` (Reverie at the hermit submodule pin d766df20: `reverie-ptrace/src/vdso.rs:43`, `reverie-ptrace/src/vdso.rs:275`, `reverie-ptrace/src/vdso.rs:358`). Detcore's run-mode subscription includes it, so glibc's `time()` reaches `handle_time`.
- Because the seconds come from the same virtual nanosecond clock as `clock_gettime(CLOCK_REALTIME)`, Hermit has no coarse-tick lag: `time()` always equals the seconds of a `clock_gettime` read at the same virtual instant. That is a value Linux can also produce, so it is a fidelity simplification rather than a divergence.

Why `model` and not the alternatives: `passthrough` leaks the host clock, which is the main source of run-to-run difference; `refuse` would break almost every program, since `time()` is among the most common libc calls; `emulate` with a constant would stop time and break timeouts and elapsed-time arithmetic. Record/replay deliberately does not virtualize time (`hermit-cli/src/lib.rs:67`), and for that mode the passthrough-and-record design is the intended one.

## Recommended tests

- T1 (covers F1, F2): with a raw `syscall(SYS_time, ...)` under `hermit run --verify` on ptrace, call with `tloc == NULL` and with a valid `tloc`; check that the stored and returned values agree, that they equal the seconds of a following `clock_gettime(CLOCK_REALTIME)` or are one less, and that the output is identical across runs.
  Existing partial coverage: `tests/c/clock_passthrough.c:70` calls `SYS_time` with a valid `tloc` and checks the two agree, but only under record/replay and `--no-virtualize-time` (T4).
- T2 (covers F3): `syscall(SYS_time, (time_t *)1)` and a `tloc` in a read-only mapping both return `EFAULT`, with and without virtual time.
- T3 (covers F2, F4): glibc's `time(&t)` (the vDSO path) under virtual time returns the configured epoch and stores the same value.
  Existing candidate: `tod_time` at `detcore/tests/time/mod.rs:245`, which runs `libc::time` under `check_fn_with_config::<Detcore, _>` with `virtualize_time: true` and asserts the result equals `tloc` and the epoch (`detcore/tests/time/mod.rs:254`, `detcore/tests/time/mod.rs:257`). It is in the `tests_time` target (`detcore/Cargo.toml:51`).
- T4 (covers F2): record a guest that calls `time`, wait until the host clock passes the recorded value, then replay; replay must return the recorded value.
  Existing candidates: `record_captures_host_clock_reads_and_replay_returns_them` at `hermit-cli/tests/clock_passthrough.rs:126` (the wait is at `hermit-cli/tests/clock_passthrough.rs:150`), and `run_without_virtual_time_reads_the_host_clock` at `hermit-cli/tests/clock_passthrough.rs:170`, which checks that `--no-virtualize-time` reads the host clock.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (investigate -> model): under virtual time Detcore returns the whole seconds of the guest virtual clock and stores them in tloc, the ptrace vDSO rewrite routes glibc time() to the syscall, and without virtual time the call is recorded and replayed; linux_determinism conditional -> nondeterministic (host wall clock); 4 flag behaviours and 4 tests listed; no coverage token yet, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `time`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+time&body=Discussion+for+the+%60time%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60time%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `time`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
