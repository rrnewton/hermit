---
schema: syscall-pages/v1
syscall: mq_timedsend
nr_x86_64: 242
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/302
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_timedsend&body=Discussion+for+the+%60mq_timedsend%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_timedsend.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_timedsend%60%2C+or+in+what+the+page+says%3F%0A
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

`mq_timedsend(mqdes, msg_ptr, msg_len, msg_prio, abs_timeout)` adds a message to a POSIX message queue, waiting while the queue is full until an absolute `CLOCK_REALTIME` deadline; glibc's `mq_send` is the same call with no deadline. Detcore refuses it with a fixed `ENOSYS` before the kernel sees it (`detcore/src/lib.rs:2237-2241`), as it refuses the whole `mq_*` family; that is what a kernel built without `CONFIG_POSIX_MQUEUE` returns.

## Linux semantics

`mqdes` must be a message-queue descriptor opened for writing. Messages are kept in priority order, and messages of equal priority in arrival order. If a receiver is already waiting, the kernel hands the message to it directly. If the queue was empty, no receiver was waiting and a process has registered with `mq_notify`, the send triggers that notification and removes the registration.

When the queue already holds `mq_maxmsg` messages, the call waits unless the descriptor is non-blocking. The deadline is an absolute time on `CLOCK_REALTIME`, so a change to the host's wall clock moves it. A signal interrupts the wait with `EINTR`, or the call restarts if the handler uses `SA_RESTART`.

No strace line was captured for this call (see Additional information).

### Flags and options

- F1: `msg_prio`: a priority from 0 to `MQ_PRIO_MAX - 1` (32767); higher priorities are received first. `msg_prio >= MQ_PRIO_MAX` gives `EINVAL`.
- F2: `msg_len`: 0 up to the queue's `mq_msgsize`; a longer message gives `EMSGSIZE`.
- F3: queue full on a blocking descriptor: wait until a receiver makes room.
- F4: queue full on an `O_NONBLOCK` descriptor: `EAGAIN`.
- F5: `abs_timeout` non-NULL: an absolute `CLOCK_REALTIME` deadline for the wait; on expiry `ETIMEDOUT`. An invalid `timespec` gives `EINVAL`.
- F6: `abs_timeout` NULL: wait without a bound (glibc's `mq_send`).
- F7: side effects on other processes: direct hand-off to a waiting receiver, and the one-shot `mq_notify` notification on an empty-to-non-empty transition with no waiting receiver.
- F8: `mqdes` errors: `EBADF` if it is not a message-queue descriptor or not open for writing; `EFAULT` for a bad `msg_ptr` or `abs_timeout`.
- F9: a signal during a wait: `EINTR`, or a restart under `SA_RESTART`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Whether a send waits depends on how fast other processes receive, which depends on host scheduling. The deadline is measured on the host wall clock, so a waiting send can end with success or with `ETIMEDOUT` depending on real elapsed time. The queue is shared with every process that opened it, possibly outside the guest, and persists across runs, so its fill level at the time of the call is not under the guest's control. The order in which concurrent senders' messages of equal priority are queued also depends on scheduling.

### Nondeterminism sources

- Cross-process progress of receivers, which decides whether the queue is full
- The absolute deadline, compared against the host's `CLOCK_REALTIME`
- Queue contents shared with processes outside the guest, and persisting across runs
- Arrival order of concurrent senders, and signal timing

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_POSIX_MQUEUE`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. A program that needs a message queue has to fall back to another IPC channel, such as a pipe, a Unix-domain socket or a mutex-guarded queue in shared memory; this body names no specific program that does so. Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `mq_timedsend` (`detcore/src/syscall_classification.rs:411`) in the arm that returns `SyscallClassification::Determinized` (`detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` includes it (`detcore/src/syscall_classification.rs:1062`), with the rest of the `mq_*` family (`detcore/src/syscall_classification.rs:1060-1065`).
- In `handle_syscall_event`, the match on the classification (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2237-2241`, which returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall. The host kernel never runs it, so the guest never waits in the kernel and never adds to a host queue. The arm does not consult `panic_on_unsupported_syscalls`, so the result is the same with or without fail-closed mode.
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`).

Why `refuse` and not the alternatives:

- `mq_open` is refused (`detcore/src/syscall_classification.rs:1060`), so no guest can obtain a message-queue descriptor through Hermit. Refusing `mq_timedsend` as well keeps the family consistent, and covers a descriptor inherited from outside the container.
- `passthrough` would let a thread wait in the kernel where Detcore's scheduler cannot see it, with a deadline on the host wall clock. The predicate's doc comment (`detcore/src/syscall_classification.rs:1034-1047`) calls the legacy pass-through nondeterministic and a container-isolation hole.
- `model` would need Detcore to own the queue contents, a scheduler-owned wait queue for full queues, virtual-time deadlines and `mq_notify` delivery, for all six `mq_*` calls at once. The legacy NEEDS-HANDLER recommendation predates this family-wide refusal.
- `ENOSYS` matches a real kernel without `CONFIG_POSIX_MQUEUE`.

Guest-visible difference from Linux: F1-F9 all return `ENOSYS`, including the argument errors, because the refusal precedes every check. This is the intended refusal.

Existing test evidence at f4055eb9cae4: the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts, in its loop at `detcore/src/syscall_classification.rs:2143`, that `mq_timedsend` classifies as Determinized and is in the predicate. It does not run a guest and does not check the errno. No e2e program or integration test calls `mq_timedsend`.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mq_timedsend, ...)` on an invalid descriptor (-1) and on an ordinary pipe descriptor, with `msg_prio` 0 and `MQ_PRIO_MAX`, with `msg_len` 0 and a large length, with a past deadline, a future deadline, an invalid `timespec` and a NULL deadline, and with a NULL `msg_ptr`. Each call must return -1 with `errno == ENOSYS`, and the output must be identical across both runs.
- T2 (covers F5, F9): the same raw call with a deadline 1 second in the future and a pending `SIGALRM` must return `ENOSYS` at once, with no wait, no `ETIMEDOUT` and no `EINTR`, with and without fail-closed mode.
- T3 (covers none): record and replay a guest that calls `mq_timedsend`; replay must produce the same `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/302#issuecomment-5872866747
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/302: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mq_timedsend records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/302 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns a fixed ENOSYS for every mq_* call at detcore/src/lib.rs:2237-2241, the result of a kernel without CONFIG_POSIX_MQUEUE; mq_open is refused too, so no guest obtains a queue descriptor); linux_determinism stays nondeterministic; 9 flag behaviours and 3 tests listed; no guest-level test calls mq_timedsend yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_POSIX_MQUEUE, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no questions removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mq_timedsend`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_timedsend&body=Discussion+for+the+%60mq_timedsend%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_timedsend.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_timedsend%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mq_timedsend" (https://github.com/rrnewton/hermit/issues/302), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/302.

The 2026-07-24 evidence below says Hermit routed `mq_timedsend` through a temporary pass-through block. That was true then; at f4055eb9cae4 the call is refused with `ENOSYS` (see Hermit design).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mq_timedsend`, and by confirming hermit currently routes `mq_timedsend` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
(not triggered by tested programs; classification from man page + kernel behavior)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/302#issuecomment-5872866747
