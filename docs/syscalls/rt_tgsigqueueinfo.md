---
schema: syscall-pages/v1
syscall: rt_tgsigqueueinfo
nr_x86_64: 297
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/410
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_tgsigqueueinfo&body=Discussion+for+the+%60rt_tgsigqueueinfo%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_tgsigqueueinfo.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_tgsigqueueinfo%60%2C+or+in+what+the+page+says%3F%0A
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

`rt_tgsigqueueinfo(tgid, tid, sig, info)` sends signal `sig` to one thread, `tid`, which must belong to thread group `tgid`, together with a caller-supplied `siginfo_t`. `pthread_sigqueue` is built on it. Detcore has a typed handler (`detcore/src/syscalls/signal.rs:760`). The handler sends the call to the kernel inside the sender's scheduler turn. When the target is another thread, the handler then tells the scheduler that a signal is pending for that thread.

## Linux semantics

`do_rt_tgsigqueueinfo` checks the arguments in this order:

1. A `tgid` or `tid` that is not positive gives `EINVAL`.
2. The `siginfo_t` is copied in. A bad pointer gives `EFAULT`, and the kernel overwrites `si_signo` with `sig`.
3. A non-negative `si_code`, or `SI_TKILL`, gives `EPERM` unless the target is the calling thread itself. These are the codes the kernel reserves for itself and for `kill`/`tgkill`.
4. `do_send_specific` looks the thread up. If `tid` is not a live thread of `tgid` the call fails with `ESRCH`. It then checks `sig` (`EINVAL` if invalid) and the caller's permission to signal the target (`EPERM`). For `sig == 0` it stops there.
5. Otherwise the signal is queued on the target thread's private pending set.

The host strace under Additional information shows signal 0 sent to the caller's own thread (`tgid == tid`) returning 0.

### Flags and options

- F1: target selection: the signal goes to thread `tid` only, and only if `tid` is a live thread in thread group `tgid`. It is queued on that thread's private pending set, and that thread handles it.
- F2: `sig == 0`: no signal is sent. The call only checks that the thread exists and that the caller may signal it.
- F3: invalid signal number (negative, or above 64): `EINVAL`
- F4: `tgid <= 0` or `tid <= 0`: `EINVAL`
- F5: the `si_code` rule: to a thread other than the caller, a `si_code` that is `>= 0` or equal to `SI_TKILL` gives `EPERM`. Negative codes such as `SI_QUEUE` are accepted. To the caller itself, any code is accepted.
- F6: no thread `tid` in group `tgid`, including a `tid` from another group and a thread that has exited: `ESRCH`
- F7: permission: sending to a process with different credentials, without `CAP_KILL`, gives `EPERM`. Threads of the same group may always signal each other.
- F8: unreadable `info` pointer: `EFAULT`
- F9: `siginfo_t` contents:
  - `si_signo` is replaced by `sig`.
  - `si_code`, `si_pid`, `si_uid` and `si_value` reach the handler as the caller wrote them. The kernel does not fill in the sender's identity.
  - When the kernel does not know the layout for that `si_code`, non-zero bytes past the common fields give `E2BIG`.
- F10: realtime signals (`SIGRTMIN` to `SIGRTMAX`):
  - Each call queues a separate instance with its own `siginfo_t`.
  - Lower signal numbers are delivered first, and instances of one number are delivered in the order they were queued.
  - When the per-user pending-signal limit (`RLIMIT_SIGPENDING`) is reached, the call fails with `EAGAIN`.
- F11: standard signals (1 to 31): if the same signal is already pending on the thread, the new one and its `siginfo_t` are dropped, and the call still returns 0.
- F12: blocked and ignored signals: a signal the target has blocked stays pending on that thread until it is unblocked. A signal whose disposition is to be ignored is discarded, and the call returns 0.
- F13: when delivery happens:
  - Sent to the caller itself and unblocked, the signal is delivered before the call returns to user space.
  - Sent to another thread, it is delivered when that thread next returns to user space. A blocking system call in that thread is interrupted with `EINTR`, or restarted under `SA_RESTART`.
- F14: fatal default action: a signal whose default action terminates the process kills the whole thread group, not only the target thread

## Determinism analysis

`linux_determinism: nondeterministic`. A signal sent to the caller itself is delivered at a fixed point, before the call returns. But the call exists to signal other threads, and for those the point where the target is interrupted depends on scheduling (F13): how far the target has run, and whether it is inside a blocking call. Two such signals sent by different threads are queued in an order the host scheduler decides (F10). `EAGAIN` (F10) depends on how many signals are pending for the same host user across all of that user's processes, including processes outside the program.

### Nondeterminism sources

- The point in another thread's execution where delivery interrupts it (F13)
- The queueing order of signals sent concurrently from different threads (F10, F11)
- The per-user pending-signal count behind `EAGAIN`, shared with other processes of the same host user (F10)
- Races between the send and the target thread exiting, which decide between success and `ESRCH` (F6)

## Hermit design

Target policy: `model`. The kernel performs the send, and Detcore fixes when it happens and when the target can observe it. What the code at f4055eb9cae4 does:

- The dispatch arm calls `handle_rt_tgsigqueueinfo` (`detcore/src/lib.rs:2744`).
- The handler sends the call to the kernel through `record_or_replay` (`detcore/src/syscalls/signal.rs:765`). Every Linux check in F1 to F12 and F14 is therefore the kernel's own. The send happens inside the sender's scheduler turn, so sends from different threads are queued in schedule order, which removes the order source in F10 and F11. The handler's doc comment states that reasoning: "the guest-supplied siginfo is deterministic input, so forwarding the kernel call is deterministic" (`detcore/src/syscalls/signal.rs:758`). The handler carries the code tag `TODO-HUMAN-REVIEW(#812)` (`detcore/src/syscalls/signal.rs:755`), which refers to https://github.com/rrnewton/hermit/pull/812.
- On success the handler calls `notify_cross_task_signal` with the `tid` as the target (`detcore/src/syscalls/signal.rs:769`). That function notifies the scheduler only when the signal is non-zero and the target is not the sender (`should_notify_cross_task_signal`, `detcore/src/syscalls/signal.rs:831`). The scheduler records the pending signal only if the target is parked in `waitid` or in restartable internal I/O polling (`notify_signal_pending`, `detcore/src/scheduler.rs:3511`). That is how a sibling blocked in Detcore learns of the signal (F13).
- Realtime signals are covered by that notification. Its comment records that an earlier `Signal::try_from` gate dropped all 31 realtime signals, so waiters hung, and that the gate was removed (`detcore/src/syscalls/signal.rs:741`; the same history is at `detcore-model/src/schedule.rs:134`).
- `rt_sigqueueinfo` sends a process-directed signal through this call when the configuration flag `backend_requires_thread_directed_process_signals` is set (`detcore/src/syscalls/signal.rs:806`).

Why `model` and not the alternatives: `passthrough` without the scheduler notification would leave a sibling parked in Detcore unaware of its signal, which is the hang the notification fixes. `emulate` would mean reimplementing signal queueing and delivery. `refuse` would break `pthread_sigqueue` and the runtimes that use it.

What remains before the code fully realizes `model`:

- **Targets parked elsewhere (F13).** The scheduler records a pending signal only for a target parked in `waitid` or in internal I/O polling (`detcore/src/scheduler.rs:3511`). This body did not establish what happens to a target parked on another Detcore resource, such as a futex wait or a sleep. Either another path wakes it, or the signal waits until the target is scheduled for some other reason. T8 measures it.
- **`EAGAIN` from the host limit (F10).** The pending-signal count includes processes outside the guest that run as the same host user, and Hermit does not model it. A guest that fills the realtime queue can see a different count of successes from run to run.

`linux_determinism` stays `nondeterministic`: that describes Linux. Under Hermit the order and delivery point come from the schedule, except for the gaps above.

## Recommended tests

- T1 (covers F1, F9, F13): a thread queues `SIGUSR1` with `SI_QUEUE` to itself, with an `SA_SIGINFO` handler installed. The handler runs before the call returns, and the handler sees the `si_code` the caller wrote.
  Existing coverage: `tests/c/dbt_self_sigqueue.c:47` (cell `c-programs/dbt-self-sigqueue`, `tests/e2e/manifests/c-programs.yaml:1113`, verify ci on ptrace), and `tests/rust/tkill.rs:117` to `tests/rust/tkill.rs:139` (bin `rustbin_tkill`, `tests/Cargo.toml:188`), which also checks `si_code`. To add: `si_signo` set to `SIGUSR2` arrives as `SIGUSR1`, and `si_value`, `si_pid` and `si_uid` arrive as written.
- T2 (covers F2, F3, F4): signal 0 to the caller returns 0. Signal 65 and signal -1 give `EINVAL`. `tgid` 0, `tid` 0 and negative values give `EINVAL`.
- T3 (covers F5, F7): to a sibling thread, `si_code` `SI_USER` (0) and `SI_TKILL` give `EPERM`, and `SI_QUEUE` succeeds. To the caller itself, `SI_USER` succeeds. Where the guest can run a child under another uid, a send to that child without `CAP_KILL` gives `EPERM`. Otherwise the test reports that this check was skipped.
- T4 (covers F6): a real `tid` with the wrong `tgid` gives `ESRCH`, and so does the `tid` of a thread already joined. Run under `--verify`.
- T5 (covers F8): `info` of `NULL` or `(void *)1` gives `EFAULT`.
- T6 (covers F10): block `SIGRTMIN` and `SIGRTMIN+1`, queue three instances with distinct `si_value`s across both numbers, then unblock. The handler sees them lowest number first and in queueing order within a number. With `RLIMIT_SIGPENDING` lowered, the send past the limit gives `EAGAIN`. That result depends on the host user's other pending signals, so the test reports the limit it used.
- T7 (covers F11, F12): block `SIGUSR1`, send it twice with different `si_value`s, then unblock: the handler runs once, with the first `si_value`. With `SIGUSR1` set to `SIG_IGN`, the call returns 0 and `sigpending` shows nothing.
- T8 (covers F13): a sibling thread blocks in each of `waitid`, a pipe `read`, a futex wait and `nanosleep`. The main thread sends it `SIGRTMIN` through `pthread_sigqueue`. The sibling's handler runs, and its call returns `EINTR` (or restarts under `SA_RESTART`). The output is identical across `--verify` runs, with no hang.
- T9 (covers F14): in a forked child, send `SIGTERM` with its default action to a sibling thread. The parent's `waitpid` reports the whole child as killed by `SIGTERM`.
- T10 (covers F1, F10, F13): record a two-thread program in which each thread `pthread_sigqueue`s realtime signals to the other and prints what its handler receives, then replay it. Replay must print the same sequence.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/410: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel performs the send inside the sender's scheduler turn; cross-thread sends notify the scheduler); linux_determinism stays nondeterministic; 14 flag behaviours and 10 tests listed; open questions: wakeup of a target parked on a resource other than waitid or internal I/O polling, and EAGAIN from the host-shared RLIMIT_SIGPENDING count; no coverage token, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rt_tgsigqueueinfo`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_tgsigqueueinfo&body=Discussion+for+the+%60rt_tgsigqueueinfo%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_tgsigqueueinfo.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_tgsigqueueinfo%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: rt_tgsigqueueinfo" (https://github.com/rrnewton/hermit/issues/410), legacy variant A1.

### Open questions

- Wakeup scope (F13): the scheduler records a cross-thread pending signal only for a target parked in `waitid` or in restartable internal I/O polling (`detcore/src/scheduler.rs:3511`). Does a target parked on another Detcore resource, such as a futex wait or `nanosleep`, get woken some other way, or does it wait until it is scheduled for another reason? T8 would measure this.
- `EAGAIN` policy (F10): should Hermit model a guest-local pending-signal limit? Today the realtime queue limit counts the host user's signals from outside the guest. This is an owner policy question.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=rt_tgsigqueueinfo` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `rt_tgsigqueueinfo` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
rt_tgsigqueueinfo(3069897, 3069897, 0, {}) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
