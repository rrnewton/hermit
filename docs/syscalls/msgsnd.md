---
schema: syscall-pages/v1
syscall: msgsnd
nr_x86_64: 69
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/307
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msgsnd&body=Discussion+for+the+%60msgsnd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsgsnd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msgsnd%60%2C+or+in+what+the+page+says%3F%0A
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

`msgsnd(msqid, msgp, msgsz, msgflg)` appends a message to a System V message queue, and waits while the queue is full unless `IPC_NOWAIT` is set. Hermit refuses it: Detcore returns a fixed `ENOSYS` without forwarding the call (`detcore/src/lib.rs:2237-2241`), as on a kernel built without `CONFIG_SYSVIPC`. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: Linux man-pages msgop(2).

## Linux semantics

`msgp` points to a `struct msgbuf` holding a `long mtype` followed by `msgsz` bytes of text. A queue accepts a message while its total bytes, and its message count, stay within `msg_qbytes`. Otherwise the sender waits, or fails with `EAGAIN` under `IPC_NOWAIT`.

A queue is a host kernel object addressed by an integer id. It lives in the caller's IPC namespace, outlives the process that created it, and is shared with every process in that namespace that has permission. Ids come from `msgget`.

If a receiver is already waiting for a matching type, the kernel hands the message straight to it and wakes it. A successful send sets `msg_lspid` to the caller's pid, `msg_stime` to the current wall-clock time, and increments `msg_qnum`.

A blocked `msgsnd` that a signal handler interrupts fails with `EINTR`. It is never restarted, even with `SA_RESTART` (signal(7)).

The 2026-07-24 host strace under Additional information, `msgsnd(-1, ...) = -1 EINVAL`, shows the invalid-id path on the host.

### Flags and options

- F1: blocking send (no `IPC_NOWAIT`): if the message does not fit within `msg_qbytes`, wait until space frees, the queue is removed (`EIDRM`), or a signal arrives (`EINTR`, never restarted)
- F2: `IPC_NOWAIT`: fail with `EAGAIN` instead of waiting when the queue lacks space
- F3: `mtype` less than 1: `EINVAL`
- F4: `msgsz` negative or larger than `MSGMAX` (`/proc/sys/kernel/msgmax`): `EINVAL`. A zero-length message is allowed.
- F5: `msqid` negative or naming no queue: `EINVAL`. The queue removed while the caller waits: `EIDRM`.
- F6: caller lacks write permission on the queue (and has no `CAP_IPC_OWNER`): `EACCES`
- F7: `msgp` not readable: `EFAULT`. Kernel memory exhausted: `ENOMEM`.
- F8: delivery and bookkeeping: a message for a waiting receiver goes straight to it; success updates `msg_lspid`, `msg_stime`, `msg_qnum` and the byte count. `msgflg` bits other than `IPC_NOWAIT` have no effect on a send.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification.

Whether a send waits, and when it returns, depends on how fast the processes that receive from the queue make progress. Which of several competing senders goes first depends on host scheduling. The queue is shared with every process in the IPC namespace, including processes outside the guest, and the metadata a send writes (`msg_lspid`, `msg_stime`) is a host pid and a wall-clock time. The call repeats only when one deterministic schedule owns every sender and receiver and nothing outside touches the queue.

### Nondeterminism sources

- Blocking on a full queue, which depends on receiver progress in other processes
- The order in which competing senders and receivers are scheduled, and signal timing (`EINTR`)
- Host processes that share the queue, and host limits (`msgmax`, `msgmnb`)
- Wall-clock time and host pids written into the queue metadata

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `msgsnd` as Determinized (`detcore/src/syscall_classification.rs:416`, in the arm that ends at `detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) names it at `detcore/src/syscall_classification.rs:1067`.
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

Per-call target for `msgsnd`:

- The message is copied into Detcore's queue. A send fits only while both the queued bytes plus `msgsz` and the queued message count plus one stay within `msg_qbytes`, as `msg_fits_inqueue` checks (F1, F2).
- A message for a parked receiver whose selector matches goes straight to it, as `pipelined_send` does, and that receiver becomes runnable (F8).
- Success sets `msg_lspid` to the caller's virtual pid and `msg_stime` to virtual time, and updates `msg_qnum` and the byte count (F8).

Errno order (owner decision 6), following `ksys_msgsnd` and `do_msgsnd` in Linux `ipc/msg.c`: reading `mtype` from `msgp` comes first (`EFAULT`); then `msgsz` above MSGMAX or negative, or `msqid` below 0, gives `EINVAL`; then `mtype` below 1 gives `EINVAL`; then copying the text gives `EFAULT` or `ENOMEM`; then the lookup gives `EINVAL`; then a missing write permission gives `EACCES`; then a queue removed in the meantime gives `EIDRM`. Only then is the message queued, or the call fails with `EAGAIN` under `IPC_NOWAIT`, or it waits. A wait ends in success, `EIDRM` or `EINTR`.

Blocking (owner decision 14): today no form of `msgsnd` blocks, because every call returns `ENOSYS` at once (`detcore/src/lib.rs:2237`-`2241`), so nothing sleeps in the kernel while holding the turn. Under the model the call is never forwarded. Detcore runs the operation against its own state as a non-blocking probe. If Linux would wait, the thread parks in the deterministic scheduler, and it becomes runnable to probe again when a guest `msgrcv` frees enough space in the queue, when an `IPC_RMID` removes the object (`EIDRM`), or when a signal arrives (`EINTR`, never restarted). Until that scheduler wait exists, the blocking form keeps today's `ENOSYS`. A family that works only with `IPC_NOWAIT` is not a Linux configuration a guest expects, so the message-queue calls should keep `ENOSYS` as a unit until the wait lands; the shared-memory calls never wait and can land first.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

Today every behaviour in `### Flags and options` is replaced by the same `-1`/`ENOSYS` with no side effect. Under the target each one behaves as on Linux, against the Detcore namespace.

## Recommended tests

- T1 (covers F2, F3, F4, F5, F6, F7): under `hermit run --verify` on ptrace, on a guest queue: a send succeeds and `IPC_STAT` then shows `msg_qnum` 1; `mtype` 0 gives `EINVAL`; `msgsz` 8193 and `msgsz` -1 give `EINVAL`; `msqid` -1 and an identifier the guest never created give `EINVAL`; a `NULL` `msgp` gives `EFAULT` even with `msqid` -1, because `mtype` is read first. Filling the queue to 16384 bytes and sending once more with `IPC_NOWAIT` gives `EAGAIN`, and so does a send after 16384 zero-length messages, because the message count is limited too. After the guest drops to uid 1000 with no capabilities (owner decision 4), a send to a 0600 queue owned by uid 0 gives `EACCES`. Compare with the native run in a fresh IPC namespace, except pids and times.
- T2 (covers F1, F8): a sender blocked on a full queue completes when another thread receives a message; a receiver already waiting gets a new message at once; `msg_lspid` and `msg_stime` hold a virtual pid and a virtual time; `IPC_RMID` wakes a blocked sender with `EIDRM`; a `SIGALRM` handler interrupts a blocked send with `EINTR`. The output is the same in both `--verify` runs. The harness creates a queue outside Hermit and passes its id to the guest: the guest's send gives `EINVAL`, and the host queue still holds 0 messages with `msg_lspid` unchanged.
- T3 (covers none): the same results in fail-closed mode and under the compatibility opt-out, and a record followed by a replay reproduces them.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/307#issuecomment-5872877565
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/307: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; msgsnd records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/307 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (detcore/src/lib.rs:2237-2241 returns a fixed ENOSYS for the whole System V message-queue family without forwarding; msgget is refused too, so no guest-obtained id exists); linux_determinism stays nondeterministic; 8 flag behaviours and 3 tests listed; no test calls msgsnd at this SHA, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 14, 4 and 13 applied; target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the msgsnd target (msg_fits_inqueue limit on bytes and count, pipelined_send hand-off), the kernel errno order (decision 6) and the decision-14 rule (non-blocking probe plus scheduler wait; the message-queue calls keep ENOSYS as a unit until that wait exists); T1-T3 rewritten as behaviour tests, ids kept, T1 now covers F2-F7 and T2 keeps F1 and F8; no new F/T ids; P3 review notes marked superseded; no open questions existed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `msgsnd`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msgsnd&body=Discussion+for+the+%60msgsnd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsgsnd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msgsnd%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: msgsnd" (https://github.com/rrnewton/hermit/issues/307), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/307.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=msgsnd`, and by confirming hermit currently routes `msgsnd` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
msgsnd(-1, {mtype=0, mtext=""}, 0, 0) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/307#issuecomment-5872877565

### P3 review notes (2026-10-03, hermit f4055eb9cae4)

- The legacy recommended action **NEEDS-HANDLER** predates the deterministic `ENOSYS` refusal whose code is tagged `TODO-HUMAN-REVIEW(#731)` (https://github.com/rrnewton/hermit/issues/731). At this SHA the System V message-queue family is refused by `is_unsupported_async_ipc_syscall`, so the target is `refuse`, not a new handler.
- The 2026-07-24 strace section above says Hermit routed this call through a temporary pass-through and that a harness reached the host kernel under `hermit run`. That is history: at this SHA Detcore refuses the call before it reaches the host.
- A later upgrade to `model` remains possible if a workload needs this interface; Hermit design lists what it would need.
- Superseded 2026-10-04 by owner decision 9: the System V message-queue family is to be modelled, so the first and third notes no longer describe the target; see `Target (owner decision 9)` under Hermit design.
