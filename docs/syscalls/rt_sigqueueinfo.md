---
schema: syscall-pages/v1
syscall: rt_sigqueueinfo
nr_x86_64: 129
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/407
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigqueueinfo&body=Discussion+for+the+%60rt_sigqueueinfo%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigqueueinfo.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigqueueinfo%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 11
---

## Summary

`rt_sigqueueinfo(tgid, sig, uinfo)` sends a process-directed signal together with a caller-supplied `siginfo_t`, so the sender chooses `si_code` and `si_value`. It is the system call behind glibc's `sigqueue`. Detcore handles it like `kill` (`handle_rt_sigqueueinfo`, `detcore/src/syscalls/signal.rs:781`): it resolves the target process to its live threads, forwards the call to the kernel only when exactly one thread can receive it, and refuses a signal to a multithreaded process with `ENOSYS`.

Manual reference: Linux man-pages rt_sigqueueinfo(2).

## Linux semantics

The kernel first copies `uinfo` from the caller (`EFAULT` if it is unreadable) and overwrites its `si_signo` with `sig`. It then refuses with `EPERM` if the target is not the caller's own process and `si_code` is either non-negative or `SI_TKILL`: those codes are reserved for the kernel and for `kill`/`tkill`, so a process may forge them only towards itself. After that the call behaves like `kill(tgid, sig)` for one positive process ID: no matching process gives `ESRCH` (this includes `tgid <= 0`, which this call does not treat as a process group); a signal number outside 0..64 gives `EINVAL`; missing permission to signal the target gives `EPERM`.

Signal 0 sends nothing and only checks existence and permission; the observed host call `rt_sigqueueinfo(<self>, 0, {})` returned 0. For a real signal, the kernel queues it on the process with the caller's `siginfo_t`. A standard signal (1..31) that is already pending is not queued a second time. A realtime signal (34..64) is queued once per call, in order, until the receiver's `RLIMIT_SIGPENDING` is reached, after which the call fails with `EAGAIN`. The signal is delivered to one thread of the process that does not block it; the kernel picks that thread, and if every thread blocks it, it stays pending on the process.

### Flags and options

- F1: `sig == 0`: no signal is sent; returns 0 if the target exists and may be signalled, else `ESRCH` or `EPERM`
- F2: `sig` outside 0..64: `EINVAL`
- F3: `uinfo` unreadable: `EFAULT`, checked before the target and the signal number. The kernel replaces `si_signo` with `sig`; the receiver sees the caller's `si_code` and `si_value`.
- F4: target is not the caller's own process and `si_code >= 0` or `si_code == SI_TKILL`: `EPERM`
- F5: no process with ID `tgid`, including `tgid <= 0`: `ESRCH`. Target owned by another user without `CAP_KILL`: `EPERM`.
- F6: standard signals already pending are not queued again; realtime signals queue once per call until `RLIMIT_SIGPENDING`, then `EAGAIN`
- F7: process-directed delivery: the kernel chooses one thread of the target process that does not block the signal; if all block it, it stays pending on the process

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification and consistent with `kill`. The return value is a function of the arguments, the target's existence and credentials, and the pending-signal state, but three things are not: when the target runs the handler relative to its own progress, which thread of a multithreaded target the kernel chooses (F7), and whether a standard signal coalesces with one already pending (F6), which depends on how far the receiver got before the send. The target PID is also a host value outside a PID namespace.

### Nondeterminism sources

- Asynchronous delivery timing relative to the receiver's execution
- The kernel's choice of receiving thread in a multithreaded process
- Coalescing of standard signals with an earlier pending one, and the `RLIMIT_SIGPENDING` count, both timing-dependent
- Host process IDs, when the target is outside Hermit's PID namespace

## Hermit design

Target policy: `model`. The kernel stores and delivers the signal; Detcore decides which thread it reaches and when the send happens in the serialized schedule, and refuses the cases it cannot yet make deterministic. The code at hermit f4055eb9cae4:

- The typed arm `Syscall::RtSigqueueinfo(s) => self.handle_rt_sigqueueinfo(guest, s).await` (`detcore/src/lib.rs:2741`) reaches `handle_rt_sigqueueinfo` (`detcore/src/syscalls/signal.rs:781`). Its doc comment (`detcore/src/syscalls/signal.rs:777`) states the design: mirror `handle_kill`, and refuse ambiguous multithreaded process-directed delivery "until Detcore models eligible masks". The code carries the review tags `TODO-HUMAN-REVIEW(#812)` and `TODO-HUMAN-REVIEW(PR-1058)` (`detcore/src/syscalls/signal.rs:775`).
- Without thread sequentialization the call is forwarded unchanged (`detcore/src/syscalls/signal.rs:786`).
- F1: signal 0 is forwarded unchanged before any target resolution (`detcore/src/syscalls/signal.rs:790`), so the kernel produces 0, `ESRCH` or `EPERM`.
- `tgid <= 0` returns `ENOSYS` (`detcore/src/syscalls/signal.rs:795`), where Linux returns `ESRCH` (F5).
- Otherwise Detcore asks the scheduler for the live threads of `tgid` (`detcore/src/syscalls/signal.rs:798`; `process_signal_targets` at `detcore/src/scheduler.rs:3261` returns them sorted, or nothing when `tgid` is not a known thread-group leader). `deterministic_kill_target` (`detcore/src/syscalls/signal.rs:222`) maps no thread to `ESRCH`, one thread to that thread, and two or more to `ENOSYS`.
- F7: with exactly one live thread, the kernel's choice is forced. On ptrace the original call is forwarded as-is (`detcore/src/syscalls/signal.rs:800`); on backends that need thread-directed delivery (DBT, `hermit-cli/src/lib.rs:2469`) it is rewritten to `rt_tgsigqueueinfo` towards that thread, carrying the same `siginfo_t` pointer (`detcore/src/syscalls/signal.rs:806`).
- After a successful send to another thread, `notify_cross_task_signal` tells the scheduler that a signal is pending for the target (`detcore/src/syscalls/signal.rs:813`; condition at `detcore/src/syscalls/signal.rs:831`), so a target parked in a blocking wait is woken in the serialized schedule. Its comment notes that realtime signal numbers are carried as raw values and are not dropped (`detcore/src/syscalls/signal.rs:741`).
- F2, F3, F4, F6 and the credential half of F5 are decided by the kernel on the forwarded call.

What remains before the code fully realizes `model`:

- **Multithreaded targets are refused.** Any real signal to a process with more than one live thread returns `ENOSYS`, so `sigqueue` to a threaded program, including one whose runtime starts a helper thread before `main`, fails. `tests/rust/tkill.rs:89` documents this premise and cites https://github.com/rrnewton/hermit/issues/816 and https://github.com/rrnewton/hermit/issues/3419. Realizing F7 needs Detcore to choose the receiving thread from the threads' masks, as the doc comment says.
- **Error order differs from Linux.** Detcore resolves the target before the kernel reads `uinfo` or checks `sig`. By this reading of the code: a bad `uinfo` towards a missing process gives `ESRCH` instead of `EFAULT`; a bad `uinfo` or an invalid `sig` towards a multithreaded process gives `ENOSYS` instead of `EFAULT` or `EINVAL`; and `tgid <= 0` gives `ENOSYS` instead of `ESRCH`. T2 and T4 check these.
- **No KVM self-`SIGKILL` reservation.** `handle_kill` reserves an exact self-`SIGKILL` before the multithreaded refusal (`detcore/src/syscalls/signal.rs:643`); `handle_rt_sigqueueinfo` has no such step, so `SIGKILL` queued to one's own multithreaded process returns `ENOSYS`.
- **Signals Hermit takes for itself.** `SIGTRAP` and `SIGSTKFLT` are appropriated (`detcore/src/syscalls/signal.rs:46`), and the same comment says realtime signals 34 to 64 cannot be represented at the reverie ptrace boundary (`detcore/src/syscalls/signal.rs:63`), so F6's realtime queueing may not reach a handler as on Linux.

## Recommended tests

- T1 (covers F3, F7): a single-threaded guest installs an `SA_SIGINFO` handler for `SIGUSR1` and sends `rt_sigqueueinfo(getpid(), SIGUSR1, &info)` with `si_code = SI_QUEUE`; the call returns 0 and the handler runs once and sees `SI_QUEUE`. Extend it to set `si_signo` to a different value and `si_value` to a marker, and check that the handler sees `SIGUSR1` and the marker.
  Existing coverage: `tests/c/dbt_self_sigqueue.c:59` checks the return value and that the handler ran with `si_code == SI_QUEUE` (`tests/c/dbt_self_sigqueue.c:23`), cell `c-programs/dbt-self-sigqueue` (`tests/e2e/manifests/c-programs.yaml:1113`, verify ci on ptrace). `tests/rust/tkill.rs:141` does the same, but no e2e cell runs it.
- T2 (covers F1, F2, F5): towards self, signal 0 returns 0 and signal 65 returns `EINVAL`; towards an unused PID, signal 0 and `SIGUSR1` both return `ESRCH`; `tgid = 0` and `tgid = -1` return `ESRCH` on Linux. Record Hermit's `ENOSYS` for `tgid <= 0` as a compatibility defect.
- T3 (covers F4): a parent forks a child and sends `SIGUSR1` with `si_code = SI_USER` (0) and with `SI_TKILL`: both `EPERM`. With `SI_QUEUE` the child receives it and reports receipt through its exit status.
- T4 (covers F3): `uinfo = (void *)1` towards self returns `EFAULT`; towards an unused PID it must also return `EFAULT`. Record Hermit's result for the second case.
- T5 (covers F6): with `SIGUSR1` and `SIGRTMIN` blocked, queue each three times to self, then unblock: `SIGUSR1` runs its handler once and `SIGRTMIN` three times, with the three `si_value`s in order. Identical in both `--verify` runs.
- T6 (covers F7): a process with two threads, one blocking `SIGUSR1`, receives `rt_sigqueueinfo` from a child process. Linux delivers to the unblocked thread. Today Hermit returns `ENOSYS`; the test records that until Detcore models the masks, then asserts the delivering thread.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/407: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel delivers; Detcore resolves the target to its sole live thread like kill and refuses multithreaded targets with ENOSYS); linux_determinism stays nondeterministic; 7 flag behaviours and 6 tests; gaps: multithreaded refusal, error order (ENOSYS/ESRCH before EFAULT/EINVAL), tgid <= 0 gives ENOSYS; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rt_sigqueueinfo`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigqueueinfo&body=Discussion+for+the+%60rt_sigqueueinfo%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigqueueinfo.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigqueueinfo%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: rt_sigqueueinfo" (https://github.com/rrnewton/hermit/issues/407), legacy variant A1.

The 2026-07-24 strace evidence below describes Hermit before Detcore had a handler for this call; it is kept as history. The thread-directed sibling `rt_tgsigqueueinfo` is handled by `handle_rt_tgsigqueueinfo` (`detcore/src/syscalls/signal.rs:760`).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=rt_sigqueueinfo` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `rt_sigqueueinfo` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
rt_sigqueueinfo(3069897, 0, {}) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
