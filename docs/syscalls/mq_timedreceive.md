---
schema: syscall-pages/v1
syscall: mq_timedreceive
nr_x86_64: 243
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/301
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_timedreceive&body=Discussion+for+the+%60mq_timedreceive%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_timedreceive.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_timedreceive%60%2C+or+in+what+the+page+says%3F%0A
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

`mq_timedreceive(mqdes, msg_ptr, msg_len, msg_prio, abs_timeout)` removes the oldest message of the highest priority from a POSIX message queue, waiting while the queue is empty until an absolute `CLOCK_REALTIME` deadline; glibc's `mq_receive` is the same call with no deadline. Detcore refuses it with a fixed `ENOSYS` before the kernel sees it (`detcore/src/lib.rs:2237-2241`), as it refuses the whole `mq_*` family; that is what a kernel built without `CONFIG_POSIX_MQUEUE` returns.

## Linux semantics

`mqdes` must be a message-queue descriptor opened for reading. The call returns the length of the message it copied into `msg_ptr`, and stores its priority through `msg_prio` when that pointer is not NULL. Taking a message from a full queue wakes one waiting sender. If several receivers wait on an empty queue, the kernel delivers each new message to one of them.

When the queue is empty, the call waits unless the descriptor is non-blocking. The deadline is an absolute time on `CLOCK_REALTIME`, so a change to the host's wall clock moves it. A signal interrupts the wait with `EINTR`, or the call restarts if the handler uses `SA_RESTART`.

No strace line was captured for this call (see Additional information).

### Flags and options

- F1: delivery order: the oldest message of the highest priority is returned; the return value is its length.
- F2: `msg_prio` non-NULL receives the message's priority; NULL discards it.
- F3: `msg_len` smaller than the queue's `mq_msgsize`: `EMSGSIZE`, even if the waiting message would fit.
- F4: queue empty on a blocking descriptor: wait until a sender adds a message.
- F5: queue empty on an `O_NONBLOCK` descriptor: `EAGAIN`.
- F6: `abs_timeout` non-NULL: an absolute `CLOCK_REALTIME` deadline for the wait; on expiry `ETIMEDOUT`. An invalid `timespec` gives `EINVAL`.
- F7: `abs_timeout` NULL: wait without a bound (glibc's `mq_receive`).
- F8: side effect on other processes: removing a message from a full queue wakes a waiting sender.
- F9: `mqdes` errors: `EBADF` if it is not a message-queue descriptor or not open for reading; `EFAULT` for a bad `msg_ptr`, `msg_prio` or `abs_timeout`.
- F10: a signal during a wait: `EINTR`, or a restart under `SA_RESTART`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Whether a receive waits, and which message it gets, depend on when other processes send, which depends on host scheduling. With several receivers, which one gets a message is decided by the kernel's wakeup order. The deadline is measured on the host wall clock, so a waiting receive can end with a message or with `ETIMEDOUT` depending on real elapsed time. The queue is shared with every process that opened it, possibly outside the guest, and persists across runs, so leftover messages from an earlier run can be received.

### Nondeterminism sources

- Cross-process progress of senders, and the kernel's choice among several waiting receivers
- The absolute deadline, compared against the host's `CLOCK_REALTIME`
- Queue contents shared with processes outside the guest, and persisting across runs
- Signal timing

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_POSIX_MQUEUE`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. A program that needs a message queue has to fall back to another IPC channel, such as a pipe, a Unix-domain socket or a mutex-guarded queue in shared memory; this body names no specific program that does so. Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `mq_timedreceive` (`detcore/src/syscall_classification.rs:412`) in the arm that returns `SyscallClassification::Determinized` (`detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` includes it (`detcore/src/syscall_classification.rs:1063`), with the rest of the `mq_*` family (`detcore/src/syscall_classification.rs:1060-1065`).
- In `handle_syscall_event`, the match on the classification (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2237-2241`, which returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall. The host kernel never runs it, so the guest never waits in the kernel and never takes a message from a host queue. The arm does not consult `panic_on_unsupported_syscalls`, so the result is the same with or without fail-closed mode.
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`).

Why `refuse` and not the alternatives:

- `mq_open` is refused (`detcore/src/syscall_classification.rs:1060`), so no guest can obtain a message-queue descriptor through Hermit. Refusing `mq_timedreceive` as well keeps the family consistent, and covers a descriptor inherited from outside the container.
- `passthrough` would let a thread wait in the kernel where Detcore's scheduler cannot see it, with a deadline on the host wall clock, and would admit messages from outside processes. The predicate's doc comment (`detcore/src/syscall_classification.rs:1034-1047`) calls the legacy pass-through nondeterministic and a container-isolation hole.
- `model` would need Detcore to own the queue contents and priority order, a scheduler-owned wait queue for empty queues, virtual-time deadlines and a deterministic choice among receivers, for all six `mq_*` calls at once. The legacy NEEDS-HANDLER recommendation predates this family-wide refusal.
- `ENOSYS` matches a real kernel without `CONFIG_POSIX_MQUEUE`.

Guest-visible difference from Linux: F1-F10 all return `ENOSYS`, including the argument errors, because the refusal precedes every check. Nothing is written through `msg_ptr` or `msg_prio`. This is the intended refusal.

Existing test evidence at f4055eb9cae4: the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts, in its loop at `detcore/src/syscall_classification.rs:2143`, that `mq_timedreceive` classifies as Determinized and is in the predicate. It does not run a guest and does not check the errno. No e2e program or integration test calls `mq_timedreceive`.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8, F9): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mq_timedreceive, ...)` on an invalid descriptor (-1) and on an ordinary pipe descriptor, with `msg_prio` NULL and non-NULL, with `msg_len` 0 and a large length, with a past deadline, a future deadline, an invalid `timespec` and a NULL deadline, and with a NULL `msg_ptr`. Each call must return -1 with `errno == ENOSYS`, the buffer and priority variable must keep their sentinel values, and the output must be identical across both runs.
- T2 (covers F6, F10): the same raw call with a deadline 1 second in the future and a pending `SIGALRM` must return `ENOSYS` at once, with no wait, no `ETIMEDOUT` and no `EINTR`, with and without fail-closed mode.
- T3 (covers none): record and replay a guest that calls `mq_timedreceive`; replay must produce the same `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/301#issuecomment-5872864387
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/301: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mq_timedreceive records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/301 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns a fixed ENOSYS for every mq_* call at detcore/src/lib.rs:2237-2241, the result of a kernel without CONFIG_POSIX_MQUEUE; mq_open is refused too, so no guest obtains a queue descriptor); linux_determinism stays nondeterministic; 10 flag behaviours and 3 tests listed; no guest-level test calls mq_timedreceive yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_POSIX_MQUEUE, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no questions removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mq_timedreceive`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_timedreceive&body=Discussion+for+the+%60mq_timedreceive%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_timedreceive.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_timedreceive%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mq_timedreceive" (https://github.com/rrnewton/hermit/issues/301), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/301.

The 2026-07-24 evidence below says Hermit routed `mq_timedreceive` through a temporary pass-through block. That was true then; at f4055eb9cae4 the call is refused with `ENOSYS` (see Hermit design).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mq_timedreceive`, and by confirming hermit currently routes `mq_timedreceive` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
(not triggered by tested programs; classification from man page + kernel behavior)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/301#issuecomment-5872864387
