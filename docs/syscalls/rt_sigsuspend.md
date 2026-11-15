---
schema: syscall-pages/v1
syscall: rt_sigsuspend
nr_x86_64: 130
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/409
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigsuspend&body=Discussion+for+the+%60rt_sigsuspend%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigsuspend.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigsuspend%60%2C+or+in+what+the+page+says%3F%0A
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

Atomically replaces the calling thread's signal mask and suspends the thread until a signal is delivered whose action is to run a handler or to terminate the process. Detcore handles it (`handle_rt_sigsuspend`, `detcore/src/syscalls/signal.rs:396`): it validates the arguments, checks whether a signal is already deliverable under the new mask, and then runs the real blocking call outside the scheduler's runnable set, so the kernel still performs the atomic mask swap and the restore. A wait that no signal can ever end is reported as a deadlock.

Manual reference: Linux man-pages sigsuspend(2).

## Linux semantics

`rt_sigsuspend(mask, sigsetsize)` checks `sigsetsize` first: anything other than the kernel's signal-set size (8 bytes on x86_64) gives `EINVAL` before `mask` is read. It then copies `mask` in, giving `EFAULT` if it is NULL or unreadable. `SIGKILL` and `SIGSTOP` are removed from the new mask, which is installed as the thread's blocked set.

The thread then sleeps until a signal is pending that the new mask does not block. If one is already pending, it does not sleep at all. When a handled signal is delivered, its handler runs with the temporary mask (plus the handler's `sa_mask`) in effect; afterwards the original mask is restored and the call returns -1 with `EINTR`. It never returns success. A signal whose action terminates the process ends the process instead. A signal whose disposition is to ignore it, explicitly or by default (`SIGCHLD`, `SIGWINCH`, `SIGURG`), does not end the wait. A stop signal stops the process, and after `SIGCONT` the thread is still waiting.

The wake-up is a property of the whole process's signal traffic: which signal ends the wait, and when, depends on the senders.

### Flags and options

- F1: `sigsetsize` other than 8: `EINVAL`, before `mask` is read
- F2: `mask` NULL or unreadable: `EFAULT`
- F3: no deliverable signal pending: the temporary mask is installed atomically and the thread sleeps. A handled signal that the mask does not block runs its handler under the temporary mask; then the original mask is restored and the call returns -1 with `EINTR`.
- F4: a signal not blocked by the temporary mask is already pending: the call does not sleep. The handler runs at once, then `EINTR` with the original mask restored.
- F5: a signal whose action terminates the process (default action of `SIGTERM`, `SIGINT` and the like, or `SIGKILL`) ends the process; the call never returns
- F6: signals that do not end the wait: `SIGKILL` and `SIGSTOP` in `mask` are ignored (cannot be blocked); a signal with an ignore disposition is discarded; a stop signal stops the process, and the wait continues after `SIGCONT`
- F7: no signal can ever arrive: the thread waits forever

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Argument errors (F1, F2) are deterministic, and so is the immediate case (F4) given the pending set at entry. The blocking case (F3) is not: when it ends, and which signal ends it, depends on when other threads, processes, timers or the terminal raise signals, and the handler then runs at a point in the program's execution that depends on that timing. This call is the classic wait-for-signal primitive, so in practice it is the blocking case that matters. It repeats only if every signal source is inside one deterministic schedule.

### Nondeterminism sources

- Arrival time and identity of the signal that ends the wait
- Order of several signals that become deliverable together
- Whether a signal is already pending at entry, which depends on the timing of earlier sends

## Hermit design

Target policy: `model`. The kernel keeps the signal mask, the pending set and the atomic swap; Detcore decides when the waiting thread may continue and detects waits that cannot end. The code at hermit f4055eb9cae4:

- The typed arm `Syscall::RtSigsuspend(s) => self.handle_rt_sigsuspend(guest, s).await` (`detcore/src/lib.rs:2726`) reaches `handle_rt_sigsuspend` (`detcore/src/syscalls/signal.rs:396`). Its doc comment (`detcore/src/syscalls/signal.rs:391`) gives the reason for running the real call: the kernel must swap and restore the mask atomically.
- F1: `validate_kernel_sigset_size` returns `EINVAL` first (`detcore/src/syscalls/signal.rs:403`; helper at `detcore/src/syscalls/signal.rs:118`).
- F2: a NULL `mask` returns `EFAULT` without touching memory (`detcore/src/syscalls/signal.rs:404`). An unreadable `mask` is caught by `read_kernel_sigset` (`detcore/src/syscalls/signal.rs:408`), which probes the address with an injected `rt_sigprocmask(how = -1)` so the kernel's own access check produces `EFAULT`, instead of a `PTRACE_PEEKDATA` read that can succeed on a `PROT_NONE` page (`detcore/src/syscalls/signal.rs:131`).
- F4: Detcore injects `rt_sigpending` into a scratch stack slot (`detcore/src/syscalls/signal.rs:409`). If any pending signal is outside the temporary mask, the real call runs through `record_or_replay_blocking` as an ordinary external blocking operation (`detcore/src/syscalls/signal.rs:421`; helper at `detcore/src/syscalls/helpers.rs:111`).
- F3: otherwise the real call runs through `record_or_replay_rt_sigsuspend` (`detcore/src/syscalls/signal.rs:428`; helper at `detcore/src/syscalls/helpers.rs:124`) under the resource `BlockingRtSigsuspend`, documented as an operation that "cannot complete without a signal" (`detcore/src/resources.rs:319`). The scheduler places the thread in its own pool of blocked `rt_sigsuspend` threads (`detcore/src/scheduler.rs:4454`). Other threads keep running; the signal that ends the wait is sent by a thread that Detcore has already serialized.
- F7: when nothing else can run and no timer can fire, the scheduler stops the run with a terminal deadlock verdict whose message includes "thread(s) waiting in rt_sigsuspend with no possible signal" (`detcore/src/scheduler.rs:3786`). The comment at `detcore/src/scheduler.rs:3643` explains that this pool, unlike real external IO, has no spontaneous completion, so the scheduler moves on to timers and the deadlock check.
- F5, F6: delivery, termination, ignore dispositions and stop/continue are the kernel's.
- Record/replay: `rt_sigsuspend` is in the record/replay subscription list (`detcore/src/lib.rs:1213`).

What remains before the code fully realizes `model`:

- **Pending but ignored signals.** The F4 check treats any pending signal outside the temporary mask as an immediate completion (`detcore/src/syscalls/signal.rs:421`). On Linux a pending signal whose disposition is ignore does not end the wait (F6), so in that case the call would keep waiting while Detcore has classified it as an ordinary blocking operation rather than as an `rt_sigsuspend` wait, and the deadlock check could not name it. This is a reading of the code, not a measurement; T5 checks it.
- **Signals Hermit takes for itself.** `SIGTRAP` and `SIGSTKFLT` are appropriated (`detcore/src/syscalls/signal.rs:46`): a guest waiting for either cannot be woken as on Linux. The same comment says realtime signals 34 to 64 fail at the reverie ptrace boundary by a separate mechanism (`detcore/src/syscalls/signal.rs:63`).
- **Most behaviours have no e2e coverage.** The scenarios that exercise F1, F2, F3, F4 and F7 in `tests/c/signal_determinism.c` run only from `hermit-cli/tests/signal_determinism.rs`, which cannot carry a coverage token (see Additional information). The e2e cell for that file runs only the `itimer-delivery` scenario.

## Recommended tests

- T1 (covers F1, F2): raw `syscall(SYS_rt_sigsuspend, ...)`: size 7 and size 9 give `EINVAL` even with an inaccessible `mask`; a NULL `mask` and an inaccessible `mask` with size 8 give `EFAULT`.
  Existing coverage: `tests/c/signal_delivery_sequence.c:272` (size 9 before the pointer) and `tests/c/signal_delivery_sequence.c:322` (inaccessible pointer), cell `c-programs/signal-delivery-sequence` (`tests/e2e/manifests/c-programs.yaml:17172`, verify ci on ptrace). The `sigsuspend-invalid-arguments` scenario (`tests/c/signal_determinism.c:573`) checks size 7 and pointer 1, but runs only from `sigsuspend_invalid_arguments_preserve_linux_error_order` (`hermit-cli/tests/signal_determinism.rs:281`). No test passes a NULL `mask`.
- T2 (covers F3): with `SIGUSR1` blocked and handled, a forked child sends `SIGUSR1` to the parent after 1 ms while the parent waits in `sigsuspend` with `SIGUSR1` unblocked. The call returns -1 with `EINTR`, the handler ran exactly once, and `SIGUSR1` is blocked again afterwards. Repeat with a sibling thread sending through `pthread_kill`.
  Existing candidate: the `blocking-sigsuspend` scenario (`tests/c/signal_determinism.c:416`), run only by `blocking_sigsuspend_releases_the_scheduler` (`hermit-cli/tests/signal_determinism.rs:300`).
- T3 (covers F4): with `SIGUSR1` blocked, handled and raised, `sigsuspend` with `SIGUSR1` unblocked returns `EINTR` at once, the signal is no longer pending, and the mask is restored.
  Existing coverage: the `itimer-delivery` scenario waits in `sigsuspend` with `SIGALRM` already pending (`tests/c/signal_determinism.c:122`) and is what the e2e cell `c-programs/signal-determinism` runs (`tests/e2e/manifests/c-programs.yaml:8132`, verify ci on ptrace). The `pending-sigsuspend` scenario (`tests/c/signal_determinism.c:500`) checks the mask and pending set explicitly, but runs only from `pending_signal_completes_sigsuspend_and_restores_mask` (`hermit-cli/tests/signal_determinism.rs:308`).
- T4 (covers F5): a child waits in `sigsuspend` with an empty mask and default dispositions; the parent sends `SIGTERM`. The parent's `waitpid` reports termination by `SIGTERM`, identically in both `--verify` runs.
- T5 (covers F6): with `SIGCHLD` blocked and pending at its default disposition, `sigsuspend` with an empty mask must keep waiting until a later handled `SIGUSR1` arrives, and then return `EINTR` once. A `mask` that includes `SIGKILL` must not stop a `SIGKILL` from ending the process. `SIGSTOP` followed by `SIGCONT` must leave the thread waiting.
- T6 (covers F7): `sigsuspend` with a full mask and no other thread. Hermit must stop with the deadlock verdict that names `rt_sigsuspend`, instead of hanging.
  Existing candidate: the `blocking-sigsuspend-no-signal` scenario (`tests/c/signal_determinism.c:604`), run only by `sigsuspend_without_signal_reports_terminal_deadlock` (`hermit-cli/tests/signal_determinism.rs:316`).
- T7 (covers F3, F4): record and replay the T2 and T3 guests; replay must reproduce the same handler count and output.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/409: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel performs the atomic mask swap; Detcore runs the real wait outside the runnable set, fast-paths an already-pending signal and reports signal-less waits as deadlock); linux_determinism stays nondeterministic; 7 flag behaviours and 7 tests; most behaviours are tested only from hermit-cli/tests/signal_determinism.rs, which cannot carry coverage; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rt_sigsuspend`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigsuspend&body=Discussion+for+the+%60rt_sigsuspend%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigsuspend.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigsuspend%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: rt_sigsuspend" (https://github.com/rrnewton/hermit/issues/409), legacy variant A1.

`hermit-cli/tests/signal_determinism.rs` builds the backend flag with `format!` (`hermit-cli/tests/signal_determinism.rs:191`) and has a DBT-only test (`hermit-cli/tests/signal_determinism.rs:290`), so under the coverage rules its tests cannot carry a coverage token for this body. Moving the `blocking-sigsuspend`, `pending-sigsuspend`, `sigsuspend-invalid-arguments` and `blocking-sigsuspend-no-signal` scenarios into e2e manifest cells would let them count.

The 2026-07-24 strace evidence below describes Hermit before Detcore had a handler for this call; it is kept as history.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=rt_sigsuspend` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `rt_sigsuspend` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
(not invoked directly for safety; classification from man page + kernel behavior)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
