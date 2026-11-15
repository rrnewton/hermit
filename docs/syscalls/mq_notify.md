---
schema: syscall-pages/v1
syscall: mq_notify
nr_x86_64: 244
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/299
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_notify&body=Discussion+for+the+%60mq_notify%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_notify.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_notify%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_unsupported_async_ipc_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`mq_notify(mqdes, sevp)` registers the calling process for a one-shot notification when a message arrives on an empty POSIX message queue, or removes that registration. Detcore refuses it with a fixed `ENOSYS` before the kernel sees it (`detcore/src/lib.rs:2237-2241`), as it refuses the whole `mq_*` family; that is what a kernel built without `CONFIG_POSIX_MQUEUE` returns.

## Linux semantics

At most one process can be registered on a queue at a time. The notification fires when a message arrives on a queue that was empty and no thread is waiting in `mq_receive`/`mq_timedreceive`; a waiting receiver takes the message instead and the registration stays. Firing removes the registration, so the process must call `mq_notify` again to be told about the next message. The notification is delivered asynchronously, at a moment chosen by the sender's progress, not by the registered process.

glibc implements `SIGEV_THREAD` in user space: it passes the kernel a netlink socket and a cookie, and a helper thread in the process starts the user's function when the kernel writes to that socket.

No strace line was captured for this call (see Additional information).

### Flags and options

- F1: `sigev_notify == SIGEV_SIGNAL`: send `sigev_signo` to the registered process, with `si_code == SI_MESGQ`, the sender's PID and UID, and `sigev_value`. An invalid signal number gives `EINVAL`.
- F2: `sigev_notify == SIGEV_THREAD` at the syscall level: `sigev_signo` names a netlink socket descriptor and `sigev_value.sival_ptr` points to a cookie the kernel writes to that socket when the notification fires. A closed descriptor gives `EBADF` and a descriptor that is not a socket gives `ENOTSOCK`.
- F3: `sigev_notify == SIGEV_NONE`: register without any notification; the registration still blocks other processes and is removed when a message arrives.
- F4: `sevp == NULL`: remove the caller's registration. If the caller is not the registered process, the call still returns 0 and changes nothing.
- F5: another process is already registered: `EBUSY`.
- F6: one-shot firing: only on an empty-to-non-empty transition with no waiting receiver; firing removes the registration.
- F7: errors: `EBADF` if `mqdes` is not a message-queue descriptor; `EINVAL` for an unknown `sigev_notify`; `EFAULT` for a bad `sevp` pointer; `ENOMEM`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. The notification arrives when another process sends to the empty queue, so the moment it is delivered, relative to what the registered process is doing, depends on host scheduling. Whether a given message produces a notification also depends on whether a receiver happened to be waiting at that instant. For `SIGEV_SIGNAL` the signal interrupts the process at an arbitrary point. For `SIGEV_THREAD` a new thread is started at an arbitrary point. Registration can fail with `EBUSY` because a process outside the guest registered first.

### Nondeterminism sources

- Asynchronous delivery timing, driven by other processes' sends
- Whether a receiver is waiting when a message arrives, which decides whether the notification fires
- Competition for the single registration slot with processes outside the guest

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_POSIX_MQUEUE`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. A program that needs a message queue has to fall back to another IPC channel, such as a pipe, a Unix-domain socket or a mutex-guarded queue in shared memory; this body names no specific program that does so. Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `mq_notify` (`detcore/src/syscall_classification.rs:413`) in the arm that returns `SyscallClassification::Determinized` (`detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` includes it (`detcore/src/syscall_classification.rs:1064`), with the rest of the `mq_*` family (`detcore/src/syscall_classification.rs:1060-1065`).
- In `handle_syscall_event`, the match on the classification (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2237-2241`, which returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall. The host kernel never runs it, so no registration exists and no asynchronous signal or netlink message can reach the guest from a queue. The arm does not consult `panic_on_unsupported_syscalls`, so the result is the same with or without fail-closed mode.
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`).

Why `refuse` and not the alternatives:

- `mq_open` is refused (`detcore/src/syscall_classification.rs:1060`), so no guest can obtain a message-queue descriptor through Hermit, and a notification could never fire for a guest-owned queue.
- `passthrough` would let the host kernel deliver signals or netlink messages at times Detcore does not choose, which defeats deterministic signal delivery. The predicate's doc comment (`detcore/src/syscall_classification.rs:1034-1047`) calls the legacy pass-through nondeterministic and a container-isolation hole.
- `model` would need a Detcore-owned queue model first, plus delivery of the notification at a deterministic point in the schedule. It only makes sense as part of modelling all six `mq_*` calls. The legacy NEEDS-HANDLER recommendation predates this family-wide refusal.
- `ENOSYS` matches a real kernel without `CONFIG_POSIX_MQUEUE`.

Guest-visible difference from Linux: F1-F7 all return `ENOSYS`, including the argument errors, because the refusal precedes every check. This is the intended refusal.

Existing test evidence at f4055eb9cae4: the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts, in its loop at `detcore/src/syscall_classification.rs:2143`, that `mq_notify` classifies as Determinized and is in the predicate. It does not run a guest and does not check the errno. No e2e program or integration test calls `mq_notify`.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mq_notify, ...)` on an invalid descriptor (-1) and on an ordinary pipe descriptor, with `SIGEV_SIGNAL` (a valid and an invalid signal), `SIGEV_THREAD` with a non-socket descriptor, `SIGEV_NONE`, an unknown `sigev_notify`, a NULL `sevp` and a bad `sevp` pointer. Each call must return -1 with `errno == ENOSYS`, and the output must be identical across both runs.
- T2 (covers F1, F6): after a refused `SIGEV_SIGNAL` registration with a handler installed, the guest must receive no signal during the rest of the run; the handler's counter stays 0.
- T3 (covers none): record and replay a guest that calls `mq_notify`; replay must produce the same `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/299#issuecomment-5872859856
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/299: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mq_notify records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/299 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns a fixed ENOSYS for every mq_* call at detcore/src/lib.rs:2237-2241, the result of a kernel without CONFIG_POSIX_MQUEUE; no registration exists, so no asynchronous signal or thread can be triggered from a queue); linux_determinism stays nondeterministic; 7 flag behaviours and 3 tests listed; no guest-level test calls mq_notify yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_POSIX_MQUEUE, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no questions removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mq_notify`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_notify&body=Discussion+for+the+%60mq_notify%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_notify.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_notify%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mq_notify" (https://github.com/rrnewton/hermit/issues/299), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/299.

The 2026-07-24 evidence below says Hermit routed `mq_notify` through a temporary pass-through block. That was true then; at f4055eb9cae4 the call is refused with `ENOSYS` (see Hermit design).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mq_notify`, and by confirming hermit currently routes `mq_notify` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
(not triggered by tested programs; classification from man page + kernel behavior)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/299#issuecomment-5872859856
