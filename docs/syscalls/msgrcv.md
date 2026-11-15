---
schema: syscall-pages/v1
syscall: msgrcv
nr_x86_64: 70
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/306
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msgrcv&body=Discussion+for+the+%60msgrcv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsgrcv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msgrcv%60%2C+or+in+what+the+page+says%3F%0A
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

`msgrcv(msqid, msgp, msgsz, msgtyp, msgflg)` removes a message from a System V message queue, selected by `msgtyp`, and waits until a matching message arrives unless `IPC_NOWAIT` is set. Hermit refuses it: Detcore returns a fixed `ENOSYS` without forwarding the call (`detcore/src/lib.rs:2237-2241`), as on a kernel built without `CONFIG_SYSVIPC`. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: Linux man-pages msgop(2).

## Linux semantics

The message is copied to `msgp` as a `long mtype` followed by at most `msgsz` bytes of text, and the call returns the number of text bytes copied. `msgtyp` and `msgflg` together choose which message is taken; the options are listed below.

A queue is a host kernel object addressed by an integer id, shared with every process in the caller's IPC namespace that has permission, and it outlives its creator. Removing a message wakes senders waiting for space. A successful receive sets `msg_lrpid` to the caller's pid, `msg_rtime` to the current wall-clock time, and decrements `msg_qnum`.

A blocked `msgrcv` that a signal handler interrupts fails with `EINTR`. It is never restarted, even with `SA_RESTART` (signal(7)).

The 2026-07-24 host strace under Additional information, `msgrcv(-1, ...) = -1 EINVAL`, shows the invalid-id path on the host.

### Flags and options

- F1: `msgtyp == 0`: take the first message in the queue
- F2: `msgtyp > 0`: take the first message whose type is `msgtyp`
- F3: `msgtyp > 0` with `MSG_EXCEPT` (Linux-specific): take the first message whose type is not `msgtyp`
- F4: `msgtyp < 0`: take the first message with the lowest type that is at most `-msgtyp`
- F5: blocking receive (no `IPC_NOWAIT`): wait until a matching message arrives, the queue is removed (`EIDRM`), or a signal arrives (`EINTR`, never restarted)
- F6: `IPC_NOWAIT`: fail with `ENOMSG` instead of waiting when no message matches
- F7: `MSG_NOERROR`: truncate a message longer than `msgsz`. Without it, such a message gives `E2BIG` and stays in the queue.
- F8: `MSG_COPY` (Linux 3.8, `CONFIG_CHECKPOINT_RESTORE`): copy, without removing, the message at ordinal position `msgtyp`. It needs `IPC_NOWAIT` and cannot be combined with `MSG_EXCEPT` (`EINVAL` otherwise); no message at that position gives `ENOMSG`; a kernel without `CONFIG_CHECKPOINT_RESTORE` gives `ENOSYS`.
- F9: errors: `msqid` naming no queue, or `msgsz` negative: `EINVAL`. No read permission (and no `CAP_IPC_OWNER`): `EACCES`. `msgp` not writable: `EFAULT`.
- F10: effects of a successful receive: the message is removed (except under `MSG_COPY`), waiting senders are woken, and `msg_lrpid`, `msg_rtime` and `msg_qnum` are updated

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification.

Which message a receiver gets, and whether and how long it waits, depends on when other processes send. When several receivers wait on one queue, which one is woken first depends on host scheduling. The queue is shared with every process in the IPC namespace, including processes outside the guest, and the metadata a receive writes (`msg_lrpid`, `msg_rtime`) is a host pid and a wall-clock time. The call repeats only when one deterministic schedule owns every sender and receiver and nothing outside touches the queue.

### Nondeterminism sources

- Message availability and order, which depend on sender progress in other processes
- Which competing receiver is woken, and signal timing (`EINTR`)
- Host processes that share the queue
- Wall-clock time and host pids written into the queue metadata

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `msgrcv` as Determinized (`detcore/src/syscall_classification.rs:417`, in the arm that ends at `detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) names it at `detcore/src/syscall_classification.rs:1068`.
- The guard arm in `handle_syscall_event` (`detcore/src/lib.rs:2237-2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every syscall that predicate names. The arm never injects the call, so the host kernel never sees it, whatever the arguments are, and no argument is validated first.
- The arm does not consult `panic_on_unsupported_syscalls`, so fail-closed mode and the compatibility opt-out return the same `ENOSYS` (`refused_only_when_fail_closed: false`).
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`), so backends that consult that boundary see the same refusal.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`, batch-7 loop at `detcore/src/syscall_classification.rs:2139-2171`) asserts the classification and the predicate membership. It does not launch Hermit, so it is not coverage for the flags below.

The doc comment (`detcore/src/syscall_classification.rs:1034-1047`) gives the reason: System V message queues are global, key-addressed kernel objects that persist across runs and are shared with the whole host. Forwarding these calls, as the legacy pass-through did, is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_SYSVIPC` returns, so a guest sees a consistent, if smaller, kernel.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `msgrcv`:

- Message selection follows `find_msg` and `testmsg`: `msgtyp` 0 takes the first message; a positive `msgtyp` takes the first of that type, or with `MSG_EXCEPT` the first of another type; a negative `msgtyp` takes the first message with the lowest type at most `-msgtyp` (F1 to F4).
- `MSG_COPY` (F8) behaves as on a kernel with `CONFIG_CHECKPOINT_RESTORE`: it copies, without removing, the message at ordinal position `msgtyp`.
- A successful receive removes the message, sets `msg_lrpid` to the caller's virtual pid and `msg_rtime` to virtual time, updates `msg_qnum`, and makes parked senders whose message now fits runnable (F10).

Errno order (owner decision 6), following `ksys_msgrcv` and `do_msgrcv` in Linux `ipc/msg.c`: `msqid` below 0 or a negative `msgsz` gives `EINVAL` first; then `MSG_COPY` with `MSG_EXCEPT` or without `IPC_NOWAIT` gives `EINVAL`, and preparing the copy buffer can give `ENOMEM`; then the lookup gives `EINVAL`; then a missing read permission gives `EACCES`; then a queue removed in the meantime gives `EIDRM`. A matching message longer than `msgsz` without `MSG_NOERROR` gives `E2BIG` and stays queued. No match gives `ENOMSG` under `IPC_NOWAIT` (and always under `MSG_COPY`), otherwise the call waits, ending in a message, `EIDRM` or `EINTR`. An unwritable `msgp` gives `EFAULT` last, after the message has already been removed from the queue.

Blocking (owner decision 14): today no form of `msgrcv` blocks, because every call returns `ENOSYS` at once (`detcore/src/lib.rs:2237`-`2241`), so nothing sleeps in the kernel while holding the turn. Under the model the call is never forwarded. Detcore runs the operation against its own state as a non-blocking probe. If Linux would wait, the thread parks in the deterministic scheduler, and it becomes runnable to probe again when a guest `msgsnd` queues a matching message, when an `IPC_RMID` removes the object (`EIDRM`), or when a signal arrives (`EINTR`, never restarted). Until that scheduler wait exists, the blocking form keeps today's `ENOSYS`. A family that works only with `IPC_NOWAIT` is not a Linux configuration a guest expects, so the message-queue calls should keep `ENOSYS` as a unit until the wait lands; the shared-memory calls never wait and can land first.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

Today every behaviour in `### Flags and options` is replaced by the same `-1`/`ENOSYS` with no side effect. Under the target each one behaves as on Linux, against the Detcore namespace.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F6, F7, F8, F9): under `hermit run --verify` on ptrace, on a guest queue holding messages of types 3, 1, 2 and 1: `msgtyp` 0 takes the type-3 message; `msgtyp` 1 takes the first type-1 message; `msgtyp` 1 with `MSG_EXCEPT` takes the type-2 message; `msgtyp` -2 takes the remaining type-1 message. With the queue empty, `IPC_NOWAIT` gives `ENOMSG`. A message longer than `msgsz` gives `E2BIG` and stays queued without `MSG_NOERROR`, and is truncated with it. `MSG_COPY | IPC_NOWAIT` at position 0 copies a message without removing it, a position past the end gives `ENOMSG`, and `MSG_COPY` alone or with `MSG_EXCEPT` gives `EINVAL`. `msqid` -1 and `msgsz` -1 give `EINVAL`. A `NULL` `msgp` gives `EFAULT`, and `msg_qnum` shows the message was removed anyway. After the guest drops to uid 1000 with no capabilities (owner decision 4), a receive from a 0600 queue owned by uid 0 gives `EACCES`. Compare with the native run in a fresh IPC namespace, except pids and times.
- T2 (covers F5, F10): a blocking receive completes when another thread sends a matching message; a sender blocked on a full queue completes when a receive frees space; `msg_lrpid` and `msg_rtime` hold a virtual pid and a virtual time; `IPC_RMID` wakes a blocked receiver with `EIDRM`; a `SIGALRM` handler interrupts a blocked receive with `EINTR`. The output is the same in both `--verify` runs. The harness creates a queue outside Hermit holding one message and passes its id to the guest: the guest's receive gives `EINVAL`, and the host queue still holds the message.
- T3 (covers none): the same results in fail-closed mode and under the compatibility opt-out, and a record followed by a replay reproduces them.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/306#issuecomment-5872875491
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/306: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; msgrcv records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/306 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (detcore/src/lib.rs:2237-2241 returns a fixed ENOSYS for the whole System V message-queue family without forwarding; msgget is refused too, so no guest-obtained id exists); linux_determinism stays nondeterministic; 10 flag behaviours and 3 tests listed; no test calls msgrcv at this SHA, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 14, 4 and 13 applied; target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the msgrcv target (find_msg selection, MSG_COPY as with CONFIG_CHECKPOINT_RESTORE), the kernel errno order (decision 6) and the decision-14 rule (non-blocking probe plus scheduler wait; the message-queue calls keep ENOSYS as a unit until that wait exists); T1-T3 rewritten as behaviour tests, ids kept, T1 now covers F1-F4 and F6-F9 and T2 keeps F5 and F10; no new F/T ids; P3 review notes marked superseded; no open questions existed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `msgrcv`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msgrcv&body=Discussion+for+the+%60msgrcv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsgrcv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msgrcv%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: msgrcv" (https://github.com/rrnewton/hermit/issues/306), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/306.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=msgrcv`, and by confirming hermit currently routes `msgrcv` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
msgrcv(-1, 0x406000, 0, 0, 0)   = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/306#issuecomment-5872875491

### P3 review notes (2026-10-03, hermit f4055eb9cae4)

- The legacy recommended action **NEEDS-HANDLER** predates the deterministic `ENOSYS` refusal whose code is tagged `TODO-HUMAN-REVIEW(#731)` (https://github.com/rrnewton/hermit/issues/731). At this SHA the System V message-queue family is refused by `is_unsupported_async_ipc_syscall`, so the target is `refuse`, not a new handler.
- The 2026-07-24 strace section above says Hermit routed this call through a temporary pass-through and that a harness reached the host kernel under `hermit run`. That is history: at this SHA Detcore refuses the call before it reaches the host.
- A later upgrade to `model` remains possible if a workload needs this interface; Hermit design lists what it would need.
- Superseded 2026-10-04 by owner decision 9: the System V message-queue family is to be modelled, so the first and third notes no longer describe the target; see `Target (owner decision 9)` under Hermit design.
