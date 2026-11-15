---
schema: syscall-pages/v1
syscall: pause
nr_x86_64: 34
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pause&body=Discussion+for+the+%60pause%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpause.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pause%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`pause()` puts the calling thread to sleep until a signal arrives that either runs a handler or ends the process. It returns only after a handler has run, and then always with `-1`/`EINTR`. Detcore handles it in `handle_pause` (`detcore/src/syscalls/signal.rs:357`) as a sleep with no deadline, `LogicalTime::INDEFINITE`, which only a signal can end. A thread that pauses with nothing able to signal it is reported as a deadlock instead of hanging.

Manual reference: Linux man-pages pause(2).

## Linux semantics

`pause` takes no arguments. The thread sleeps interruptibly. A signal whose disposition is "ignore", explicitly or by default (`SIGCHLD`, `SIGWINCH`, `SIGURG`), is discarded without waking it. A blocked signal stays pending and does not wake it either.

A caught signal runs its handler, and `pause` then returns `-1` with `EINTR`. The kernel returns `ERESTARTNOHAND`, which is never restarted after a handler, even when the handler was installed with `SA_RESTART`. A signal whose default action terminates the process ends it inside `pause`.

A stop signal (`SIGSTOP`, `SIGTSTP`, ...) followed by `SIGCONT`, or a ptrace stop, does not end the call: with no handler run, `ERESTARTNOHAND` is restarted and the thread goes back to sleep. A host strace of an interrupted `pause()` (2026-10-03, glibc 2.34) shows this `ERESTARTNOHAND` return.

Because there are no arguments, the list below gives the behaviours selected by the disposition of the arriving signal; each is one a test must cover.

### Flags and options

- F1: a caught signal: the handler runs and `pause` returns `-1` with `EINTR`
- F2: `SA_RESTART` on that handler has no effect: `pause` still returns `EINTR` and is never restarted
- F3: an ignored signal (explicit `SIG_IGN`, or a default-ignore such as `SIGCHLD`) or a blocked signal does not wake the thread
- F4: a signal whose default action terminates the process ends it inside `pause`
- F5: a stop and continue, or a ptrace stop, with no handler run, restarts the sleep instead of returning
- F6: no signal ever arrives: the thread blocks forever on Linux

## Determinism analysis

`linux_determinism: conditional`. The return value is fixed: `-1`/`EINTR` whenever the call returns at all. What varies is when it returns, and which program point in other threads has been reached by then. That depends on when the waking signal is sent, which may come from a timer, another thread, another process, or outside the container. The call repeats when the waking signal is itself produced at a deterministic point, for example by a deterministic `alarm` or by a sibling thread under a fixed schedule.

## Hermit design

Target policy: `model`. Detcore models `pause` as a scheduler wait that only signal delivery can satisfy.

When threads are sequentialized (`config.sequentialize_threads`, set at `hermit-cli/src/bin/hermit/run.rs:3667`), the handler at `detcore/src/syscalls/signal.rs:357` does this:

- It builds `SleepUntil(LogicalTime::INDEFINITE)` with `sleep_request_abs` (`detcore/src/syscalls/time.rs:122`) and parks the thread through `parked_wait_request` with `ParkedWaitPolicy::PauseNoHandlerRestart` (`detcore/src/syscalls/signal.rs:368`; policy enum at `detcore/src/scheduler/parked.rs:46`). On ptrace that is a plain resource request (`detcore/src/tool_global/parked.rs:201`).
- The scheduler never advances virtual time onto an indefinite deadline (`detcore/src/scheduler.rs:4115`). If an indefinite waiter is all that is left and no external I/O is outstanding, it reports a terminal deadlock (`detcore/src/scheduler.rs:4142`). A Linux process would hang forever in that state; Hermit exits nonzero instead. The scheduler unit test `indefinite_waiter_alone_is_reported_as_a_deadlock` (`detcore/src/scheduler.rs:8710`) pins this.
- When a signal is attached to the waiter's request, the scheduler answers `SchedResponse::Signaled` (`detcore/src/scheduler.rs:4956`), and the handler returns `EINTR` (`detcore/src/syscalls/signal.rs:380`). The handler never restarts the call, matching `ERESTARTNOHAND` after a handler.
- A normal (non-signal) wake would mean the scheduler fired an indefinite deadline, so the handler panics with "Internal violation" (`detcore/src/syscalls/signal.rs:375`).

Without sequentialization, the call is passed to the kernel unchanged (`detcore/src/syscalls/signal.rs:387`).

Why `model`: `passthrough` would let a thread sleep inside the kernel where the scheduler cannot see it, so virtual time could not advance to a pending alarm. `emulate` with an immediate `EINTR` would invent a signal that Linux never delivered. `refuse` would break every program that waits for a signal.

What this body has not confirmed from the code: whether an ignored signal (explicit `SIG_IGN`, or a default-ignore such as `SIGCHLD`) reaches the waiter's request as an inbound signal and wakes `pause` early. Linux would not wake. T2 measures it.

## Recommended tests

- T1 (covers F1): install a `SIGALRM` handler without `SA_RESTART`, call `alarm(1)`, then `pause()`. It must return `-1` with `EINTR` after the handler ran, identically across `hermit run --verify` runs.
  Existing candidate: `tests/c/pause_alarm_interrupt.c` (cell `c-programs/pause-alarm-interrupt`, `tests/e2e/manifests/c-programs.yaml:4715`, verify ci on ptrace).
- T2 (covers F3): an ignored signal must not wake `pause`. Set `SIGUSR1` to `SIG_IGN`, have a child exit (default-ignored `SIGCHLD`) and send `SIGUSR1` from a sibling thread, then deliver `SIGALRM` to a handler. `pause` must return only after the handler, and the output must show the order.
- T3 (covers F2): with the handler installed with `SA_RESTART`, `pause` must still return `EINTR`.
- T4 (covers F6): a thread that calls `pause()` with no timer and no other thread or process able to signal it must make Hermit exit nonzero with its deadlock report, not hang. The header of `tests/c/pause_alarm_interrupt.c` notes that a manifest cell cannot yet declare an expected nonzero status, so this needs a cargo integration test or a harness change.
- T5 (covers F4): a fatal signal (`SIGTERM` with default disposition, sent by a sibling thread) during `pause` ends the process with that signal, with the same exit status in every run.
- T6 (covers F5): a child process sends `SIGSTOP` then `SIGCONT` to a parent sitting in `pause`, then `SIGALRM` fires into a handler. `pause` must return only once, with `EINTR`, after the handler and not at the continue. This body did not trace how Detcore's parked wait treats a group stop; the test records what Hermit does.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (an INDEFINITE scheduler sleep that only signal delivery ends with EINTR; a pause nothing can wake is reported as a deadlock); linux_determinism conditional; 6 signal-disposition behaviours and 6 tests listed; no coverage token yet, so tracking_status stays todo; one open question on whether ignored signals wake it under Hermit
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/pause_alarm_interrupt.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pause`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pause&body=Discussion+for+the+%60pause%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpause.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pause%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `pause`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The one test file that names the syscall, `tests/shared-futex-verify/qemu_net_init.c:322`, issues raw syscall 34 in a loop after `reboot(POWER_OFF)` inside a QEMU guest's init. It does not test `pause` semantics under Hermit.

### Open questions

- Does an ignored signal wake `pause` under Hermit? The handler turns any `Signaled` answer into `EINTR` (`detcore/src/syscalls/signal.rs:380`), and the answer is built from every inbound signal attached to the request (`detcore/src/scheduler.rs:4975`). This body did not trace whether a `SIG_IGN` or default-ignored signal is filtered out before it is attached. T2 answers it; if it does wake, that is a fidelity defect to file.
