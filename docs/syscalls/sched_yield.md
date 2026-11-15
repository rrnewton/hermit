---
schema: syscall-pages/v1
syscall: sched_yield
nr_x86_64: 24
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_yield&body=Discussion+for+the+%60sched_yield%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_yield.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_yield%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 16
---

## Summary

`sched_yield` asks the kernel to give up the CPU so another runnable thread can run; it takes no arguments and always returns 0 on Linux. Under Hermit's sequentialized scheduler, Detcore does not perform the syscall at all: it turns the call into a yield point of its own deterministic scheduler and returns 0.

## Linux semantics

`sched_yield()` moves the calling thread to the end of the run queue for its static priority, and another thread runs if one is runnable. The Linux implementation always succeeds and returns 0. The only effect is on scheduling: what the caller observes afterwards is which other threads ran in the meantime, not anything the call returns.

What "yield" means depends on the caller's scheduling policy. For `SCHED_FIFO` and `SCHED_RR` threads it has a precise meaning: the thread goes to the back of the list for its priority. Under `SCHED_OTHER` (CFS, and EEVDF from Linux 6.6), the kernel only gives up the rest of the current slice, so the caller may well be picked again at once. The syscall has no way to name the thread to yield to. In a single-threaded process, or when nothing else on the CPU is runnable, the call returns at once with no visible effect.

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

The return value is fixed: Linux always returns 0. The call is still not a deterministic operation, because its one effect is on scheduling, and the host scheduler decides which thread runs next. How long it waits, what the other CPUs are running and how CFS/EEVDF accounts virtual runtime all change from run to run. So when a program uses `sched_yield` in a spin-wait loop, the number of iterations, and how its threads interleave, differ between runs.

The call repeats exactly when the caller is the only runnable thread in its process and nothing it observes depends on threads in other processes. Then it is a no-op that returns 0. That makes `linux_determinism` `conditional`: the result is fixed, and the interleaving that follows is deterministic only when no other thread can run.

## Hermit design

Target policy: `model`. Detcore owns the decision: when threads are sequentialized, `sched_yield` is a scheduling event of the deterministic scheduler, not a kernel call. The dispatch arm is `detcore/src/lib.rs:2602`, and the handler is `handle_sched_yield` at `detcore/src/syscalls/threads.rs:1540`:

- The handler branches only when `cfg.sequentialize_threads` is set (`detcore/src/syscalls/threads.rs:1545`). In every branch below it returns `Ok(0)` and never performs the syscall (`detcore/src/syscalls/threads.rs:1577-1578`).
- In chaos mode with timer preemption disabled (`max_timeslice` is `None`), it draws a fresh random priority for the caller through `random_priority_changepoint_request` (`detcore/src/syscalls/threads.rs:1559-1562`). The comment above it explains why: with fixed priorities, a thread that spins on `sched_yield` at the lowest priority number was always picked again and starved the thread it waited for (https://github.com/facebookexperimental/hermit/issues/81).
- When replaying preemptions outside chaos mode (`replay_preemptions_from` set), it resets the timeslice if a maximum timeslice is set, then sends `sched_yield_request` (`detcore/src/syscalls/threads.rs:1563-1570`).
- In chaos mode with a timeslice, or when replaying a schedule (`replay_schedule_from`), it sends a plain `yield_request` (`detcore/src/syscalls/threads.rs:1571-1573`).
- Otherwise it ends the timeslice through `end_timeslice_for_sched_yield` (`detcore/src/syscalls/threads.rs:1575`, `detcore/src/lib.rs:761-763`). That path takes the next timeslice and sends either a priority changepoint or, for an explicit yield with no schedule being replayed, `sched_yield_request` (`detcore/src/lib.rs:782-794`).
- Without sequentialization the call goes to the kernel through `record_or_replay` (`detcore/src/syscalls/threads.rs:1580`). That mode makes no determinism promise for thread interleaving, so forwarding is consistent with it.

The record/replay tool layer does not subscribe to `sched_yield`. It reaches the catch-all live injection in the recorder (`hermit-cli/src/recorder.rs:648`) and in the replayer (`hermit-cli/src/replayer.rs:575`). That is harmless for the return value, which is always 0.

Why `model` and not the alternatives:

- `passthrough` would let the host scheduler decide the interleaving, which is exactly the nondeterminism Hermit removes. It would also give the sequentialized scheduler no point at which to switch threads, so a spin-wait loop would only make progress when a timer preemption fired.
- `emulate` as a bare `return 0` would be deterministic, but would starve spin-waiters: the https://github.com/facebookexperimental/hermit/issues/81 hang that the chaos branch fixes is that failure.
- `refuse` would break every spin lock and every runtime that yields while waiting.

What remains before the code fully realizes `model`:

- **Yield semantics across scheduler modes.** The handler is tagged `TODO-HUMAN-REVIEW(#258)` (`detcore/src/syscalls/threads.rs:1538`, https://github.com/rrnewton/hermit/issues/258), asking for review of the "one-turn exclusion" semantics in every scheduler mode. The four branches send three different request kinds. Whether each one guarantees that another runnable thread actually runs before the caller is picked again is not stated in the code. The tests below make that property explicit.
- **Replayed preemption schedules.** The `replay_preemptions_from` branch is the only one that resets the timeslice. I found no test that records a schedule and replays a guest that yields; T3 asks for one.

`linux_determinism` stays `conditional`. Detcore removes the interleaving source by making the yield a deterministic scheduler event.

## Recommended tests

- T1 (covers none): a spin-wait loop that calls `sched_yield` while a worker thread publishes a value must finish, and must be bitwise identical under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/sched_yield_progress.c` (cell `c-programs/sched-yield-progress`, `tests/e2e/manifests/c-programs.yaml:7792`, verify ci on ptrace).
- T2 (covers none): the same spin-wait loop under `--chaos --max-timeslice=disabled` with several seeds must make progress without timer preemption (the https://github.com/facebookexperimental/hermit/issues/81 path).
  Existing candidate: `chaos_sched_yield_makes_progress_without_timer_preemption` at `hermit-cli/tests/chaos_sched_yield_progress.rs:120`, which passes no `--backend` flag and so runs on the default backend.
- T3 (covers none): record a preemption schedule for a two-thread guest that yields in a loop, then replay it with `replay_preemptions_from`. The replayed interleaving must match the recorded one, and every yield must return 0.
- T4 (covers none): a parent that loops on `sched_yield` while a child thread calls `exit_group` must exit cleanly in run mode and in record and replay.
  Existing candidates: `tests/rust/sched_yield.rs`, run by `default_cargo_sched_yield` (`hermit-cli/tests/hermit_modes.rs:913`) and by `record_rs_sched_yield` (`hermit-cli/tests/record_replay.rs:2323`). The rr-suite entry `rr_sched_yield_to_lower_priority` is ignored (`hermit-cli/tests/fail_closed_allowed_ignores.tsv:164`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (under sequentialization Detcore turns sched_yield into a deterministic scheduler yield and returns 0 without performing the syscall); linux_determinism stays conditional; no flags; 4 tests listed with existing candidates; no coverage token recorded, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/sched_yield_progress.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_yield`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_yield&body=Discussion+for+the+%60sched_yield%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_yield.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_yield%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `sched_yield`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
