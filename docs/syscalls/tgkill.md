---
schema: syscall-pages/v1
syscall: tgkill
nr_x86_64: 234
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/497
  - https://github.com/rrnewton/hermit/issues/518
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+tgkill&body=Discussion+for+the+%60tgkill%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftgkill.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60tgkill%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 44
---

## Summary

`tgkill(tgid, tid, sig)` sends signal `sig` to the thread `tid`, which must belong to thread group `tgid`. It is the race-free replacement for `tkill`, and glibc's `pthread_kill` and `raise` use it. Detcore handles it (`detcore/src/lib.rs:2735`, `handle_tgkill` at `detcore/src/syscalls/signal.rs:675`): the send happens at the sender's scheduler turn, the guest's own TGID and TID go to the kernel unchanged because the guest runs in a fresh PID namespace, and after a successful cross-thread send Detcore tells the scheduler so a target parked in a scheduler-owned wait wakes up.

Manual reference: Linux man-pages tgkill(2), https://man7.org/linux/man-pages/man2/tgkill.2.html.

## Linux semantics

The kernel checks the arguments, looks up the thread, checks permission and then queues the signal on that thread's private pending set. A signal number of 0 performs only the existence and permission checks and sends nothing. Requiring `tgid` to match protects against a thread ID that was freed and reused by another process, which is the race `tkill` has.

Measured on the host (Linux 7.1.3, raw `syscall(SYS_tgkill, ...)`, see Additional information): `tgkill(self, self, 0)` gives 0; a `tgid` that does not own `tid` gives `ESRCH`; `tid` 0 and `tgid` -1 give `EINVAL`; signals 65 and -1 give `EINVAL`; signal 0 to PID 1 of the host gives `EPERM`; `SIGUSR1` to self while it is ignored gives 0.

What happens next depends on the target. A standard signal (1 to 31) that is already pending is not queued twice; a realtime signal is queued every time, up to `RLIMIT_SIGPENDING` (beyond which the call fails with `EAGAIN`). A signal the target blocks stays pending; an ignored signal is discarded; a caught signal runs the handler on the target thread. A signal whose default action is fatal, such as `SIGKILL`, terminates the whole process, not just the thread. A target blocked in an interruptible syscall is woken, and that syscall returns `EINTR` or restarts according to `SA_RESTART`. A signal sent to the calling thread is delivered before the call returns to user space. The receiver's `siginfo_t` has `si_code = SI_TKILL` and the sender's PID and UID.

### Flags and options

- F1: `sig == 0`: existence and permission probe only; nothing is sent.
- F2: a standard signal (1 to 31): queued on the target thread; a duplicate of an already pending standard signal is merged.
- F3: a realtime signal (`SIGRTMIN` to `SIGRTMAX`): queued every time; `EAGAIN` once `RLIMIT_SIGPENDING` is reached.
- F4: `sig` outside 0 to 64: `EINVAL`.
- F5: `tgid <= 0` or `tid <= 0`: `EINVAL`.
- F6: no thread `tid` in thread group `tgid`, including a thread that has exited: `ESRCH`.
- F7: the caller may not signal the target (different credentials, no `CAP_KILL`): `EPERM`.
- F8: target disposition: blocked signals stay pending, ignored signals are discarded, a caught signal runs its handler on the target thread, and a fatal default action ends the whole process.
- F9: self-directed send: delivered before the syscall returns to the caller.
- F10: a target blocked in an interruptible syscall is woken, and that syscall returns `EINTR` or restarts.
- F11: `siginfo_t` seen by the receiver: `si_code = SI_TKILL`, `si_pid` and `si_uid` of the sender.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification of https://github.com/rrnewton/hermit/issues/497 and its duplicate https://github.com/rrnewton/hermit/issues/518. The sent signal is an argument, but when it takes effect is not: the point in the target's execution where the handler runs, the order of signals from several senders, whether a blocking syscall in the target had started (and so returns `EINTR`) and whether the target had already exited (`ESRCH`) all depend on host scheduling. Natively, the thread IDs that a program passes are themselves allocated by the host, and reading them back into the program makes its behaviour depend on them.

### Nondeterminism sources

- PID and TID allocation, and target lifetime (a target that exits first gives `ESRCH`)
- The point in the target's execution where the signal is acted on, and its interaction with the target's signal mask
- Order of concurrent sends from several threads or processes
- Which blocking syscall, if any, the target is inside when the signal arrives
- Credentials and the per-user pending-signal limit (`RLIMIT_SIGPENDING`)

## Hermit design

Target policy: `model`, changed from the legacy `investigate` (legacy action NEEDS-HANDLER; a handler now exists). The kernel queues and delivers the signal, but Detcore decides when the send happens and owns the waits a signal must end:

- The call is forwarded unchanged through `record_or_replay` (`detcore/src/syscalls/signal.rs:688`). The handler's doc comment (`detcore/src/syscalls/signal.rs:673`) gives the argument: guest PID and TID values are stable in the fresh PID namespace, so the guest's numbers are already the kernel's and no translation is needed, and delivery is serialized by the scheduler. Every argument check (F1, F4 to F7) therefore comes from the kernel exactly as on Linux, against the guest's own process tree.
- After a successful send to another thread, `notify_cross_task_signal` (`detcore/src/syscalls/signal.rs:693`, function at `detcore/src/syscalls/signal.rs:734`) tells the scheduler that a signal is pending for the target, so a target parked on a scheduler-owned wait such as `waitid` wakes and its syscall returns `EINTR` (F10). The notification is skipped for signal 0 and for self-directed sends (`should_notify_cross_task_signal`, `detcore/src/syscalls/signal.rs:831`). The comment at `detcore/src/syscalls/signal.rs:741` records that an earlier `Signal::try_from` check silently dropped the notification for every realtime signal; the raw number is now carried, so F3 signals wake waiters too.
- Before the send, `reserve_kvm_self_sigkill_exit` fences an exact self-`SIGKILL`; it is a no-op unless a backend-specific configuration bit is set (`detcore/src/syscalls/signal.rs:578`).
- Unlike `handle_kill` (`detcore/src/syscalls/signal.rs:617`), which must pick one recipient thread for a process-directed signal, `tgkill` names its target exactly, so there is no recipient choice to make deterministic.

Reconciling the two legacy issues: https://github.com/rrnewton/hermit/issues/518 said native forwarding "can misaddress or reorder signals" because of Hermit's virtual PIDs. At this SHA the first half does not hold: the guest runs in its own PID namespace and its TIDs are the kernel's, so forwarding addresses the right thread. The second half is what the scheduler turn and the wake-up notification address. Both issues placed `tgkill` in the old pass-through list of `detcore/src/syscall_classification.rs`; that is superseded by the typed handler.

Why `model` and not the alternatives: `emulate` would mean Detcore queues and delivers signals itself, duplicating the kernel's signal state; `refuse` would break `pthread_kill`, `raise` and `abort`; plain forwarding without the scheduler notification is exactly the earlier defect in which a `pthread_kill` from a sibling thread left a `waitid` hung forever (comment at `tests/c/waitid_signal_interrupt.c:924`).

What remains: the point at which a running target acts on a pending signal is decided by the backend's signal-delivery path, not by this handler, so the determinism of F2, F8 and F9 rests on that path and is measured only by the tests below. `EAGAIN` from `RLIMIT_SIGPENDING` (F3) counts pending signals across all processes of the host user and is not virtualized; no test at this SHA exercises it.

## Recommended tests

- T1 (covers F1, F4, F5, F6, F7): raw calls under `hermit run --verify` on ptrace: signal 0 to self, a wrong `tgid`, `tid` 0, `tgid` -1, signals 65 and -1, and signal 0 to a process with different credentials; results must match the native values under Additional information.
- T2 (covers F2, F10): a sibling thread uses `pthread_kill` to interrupt a thread parked in `waitid`; the wait must return `EINTR` with the handler run, without spinning.
  Existing candidate: `waitid_live_sibling_thread_signal_interrupts_without_spinning` at `hermit-cli/tests/waitid_signal_interrupt.rs:1036`, which runs on ptrace (`hermit-cli/tests/waitid_signal_interrupt.rs:212`) and sends with `pthread_kill` at `tests/c/waitid_signal_interrupt.c:937`.
- T3 (covers F2, F8): two sender threads, released by a barrier, `pthread_kill` the main thread with `SIGUSR1` and `SIGUSR2`; the delivery order printed must be identical across runs.
  Existing candidate: `tests/e2e/determinism-stress/guests/signal_order.c` (cell `determinism-stress-c/signal-order`, `tests/e2e/manifests/determinism-stress-c.yaml:504`, verify ci on ptrace), which sends at line 27.
- T4 (covers F3): send `SIGRTMIN` three times to a thread that blocks it, unblock, and count three handler runs; send a realtime signal to a target parked in `waitid`; with `RLIMIT_SIGPENDING` lowered to 1, the second queued realtime signal gives `EAGAIN`.
- T5 (covers F8, F9): a thread sends a caught signal to itself and the handler must have run before `pthread_kill` returns; an ignored signal returns 0 and has no effect; a `SIGKILL` sent by a non-leader thread to itself ends the whole process.
  Existing candidate: `tests/c/hello_signals.c` (cell `c-programs/hello-signals`, `tests/e2e/manifests/c-programs.yaml:2247`, verify ci on ptrace), which calls `pthread_kill(pthread_self(), SIGILL)` at line 27 and checks the handler ran synchronously.
- T6 (covers F11): an `SA_SIGINFO` handler on the target checks `si_code == SI_TKILL`, `si_pid == getpid()` and `si_uid == getuid()`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/497: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/518 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (scheduler-turn send of the unchanged call in the guest PID namespace, plus scheduler wake-up of the target); linux_determinism stays nondeterministic; merged duplicate https://github.com/rrnewton/hermit/issues/518 section by section (its misaddressing claim no longer holds at this SHA); 11 flag behaviours and 6 tests listed; tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T3 covered by tests/e2e/determinism-stress/guests/signal_order.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `tgkill`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+tgkill&body=Discussion+for+the+%60tgkill%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftgkill.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60tgkill%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: tgkill" (https://github.com/rrnewton/hermit/issues/497), legacy variant A1. A duplicate with the same title, https://github.com/rrnewton/hermit/issues/518 (legacy variant A1, batch 5, ranks 201-250), was merged into this body on 2026-10-03; its comment history (0 comments at migration) stays on that issue, and the comment history of https://github.com/rrnewton/hermit/issues/497 stays there.

### 2026-07-24 host strace evidence

Both legacy issues probed `tgkill` on the measurement host (Linux 6.17 x86_64, glibc) with a direct `syscall()` harness using self-targeting signal 0, traced with strace. At that time (`detcore/src/syscall_classification.rs` at `dd60278f`) `tgkill` sat in the old pass-through list; the typed handler described above replaced that.

From https://github.com/rrnewton/hermit/issues/497 (`strace -f -e trace=tgkill`):

```
tgkill(3069897, 3069897, 0)     = 0
```

From https://github.com/rrnewton/hermit/issues/518 (`strace -qq -f -e trace=tgkill`; the harness exited 0):

```text
tgkill(3143657, 3143657, 0)     = 0
```

### 2026-10-03 host probe

Host: Linux 7.1.3 x86_64, unprivileged. Raw `syscall(SYS_tgkill, ...)`: self, self, 0 → 0; `tgid` + 100000, self, 0 → `ESRCH`; self, 0, 0 → `EINVAL`; -1, self, 0 → `EINVAL`; signal 65 → `EINVAL`; signal -1 → `EINVAL`; 1, 1, 0 → `EPERM`; self `SIGUSR1` while ignored → 0.
