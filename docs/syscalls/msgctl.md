---
schema: syscall-pages/v1
syscall: msgctl
nr_x86_64: 71
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/304
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msgctl&body=Discussion+for+the+%60msgctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsgctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msgctl%60%2C+or+in+what+the+page+says%3F%0A
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

`msgctl(msqid, cmd, buf)` reads or changes the metadata of a System V message queue, removes it, or reports system-wide limits and usage. Hermit refuses it: Detcore returns a fixed `ENOSYS` without forwarding the call (`detcore/src/lib.rs:2237-2241`), as on a kernel built without `CONFIG_SYSVIPC`. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: Linux man-pages msgctl(2).

## Linux semantics

`cmd` selects one of seven operations, listed below. The queue operations act on a host kernel object addressed by an integer id and shared with every process in the caller's IPC namespace. `IPC_INFO` and `MSG_INFO` need no queue at all: they report the limits and usage of the whole IPC namespace.

`struct msqid_ds` holds the owner and mode (`msg_perm`), the wall-clock times of the last send, receive and change (`msg_stime`, `msg_rtime`, `msg_ctime`), the current byte and message counts (`msg_cbytes`, `msg_qnum`), the byte limit (`msg_qbytes`), and the pids of the last sender and receiver (`msg_lspid`, `msg_lrpid`).

The 2026-07-24 host strace under Additional information, `msgctl(-1, IPC_RMID, NULL) = -1 EINVAL`, shows the invalid-id path on the host.

### Flags and options

- F1: `IPC_STAT`: copy the queue's `msqid_ds` to `buf`. Needs read permission (`EACCES` otherwise).
- F2: `IPC_SET`: set `msg_perm.uid`, `msg_perm.gid`, the low 9 bits of `msg_perm.mode`, and `msg_qbytes` from `buf`, and update `msg_ctime`. The caller must be the owner or creator, or have `CAP_SYS_ADMIN` (`EPERM` otherwise). Raising `msg_qbytes` above `MSGMNB` needs `CAP_SYS_RESOURCE` (`EPERM` otherwise).
- F3: `IPC_RMID`: remove the queue at once and wake every waiting sender and receiver with `EIDRM`. Same permission rule as `IPC_SET`.
- F4: `IPC_INFO`: fill `buf`, read as a `struct msginfo`, with the system-wide limits (`msgmax`, `msgmnb`, `msgmni`, ...). Returns the index of the highest used entry in the kernel's queue array.
- F5: `MSG_INFO`: like `IPC_INFO`, but `msgpool`, `msgmap` and `msgtql` report the number of queues, messages and bytes in use across the IPC namespace
- F6: `MSG_STAT`: like `IPC_STAT`, but `msqid` is an index into the kernel's queue array, and the call returns the id of the queue at that index
- F7: `MSG_STAT_ANY` (Linux 4.17): like `MSG_STAT`, without the read-permission check
- F8: errors: an unknown `cmd`, or a `msqid` naming no queue: `EINVAL` (`EIDRM` in a race with removal). `buf` not accessible for `IPC_STAT`, `IPC_SET`, `MSG_STAT` or the info commands: `EFAULT`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The answer depends on `cmd`:

- `IPC_RMID` and `IPC_SET` depend only on the queue's existence and permissions, which a guest that owns the queue controls. They repeat when nothing outside the guest touches the queue, although `IPC_RMID` wakes waiting processes in an order the host scheduler chooses.
- `IPC_STAT`, `MSG_STAT` and `MSG_STAT_ANY` return wall-clock times (`msg_ctime` is always set) and host pids, so their output does not repeat across runs.
- `IPC_INFO` and `MSG_INFO` report host-wide limits and usage that other processes change.

### Nondeterminism sources

- Wall-clock times and host pids in `msqid_ds` (`msg_stime`, `msg_rtime`, `msg_ctime`, `msg_lspid`, `msg_lrpid`)
- Message and byte counts that depend on the progress of other senders and receivers
- System-wide limits (`/proc/sys/kernel/msgmax`, `msgmnb`, `msgmni`) and namespace-wide usage counts, which include queues owned by processes outside the guest
- Array indices and ids, which depend on every queue created in the namespace

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `msgctl` as Determinized (`detcore/src/syscall_classification.rs:418`, in the arm that ends at `detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) names it at `detcore/src/syscall_classification.rs:1069`.
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

Per-call target for `msgctl`:

- `IPC_STAT`, `MSG_STAT` and `MSG_STAT_ANY` (F1, F6, F7) return Detcore's queue record with virtual pids and times and the decision-4 uid and gid. `MSG_STAT` takes a slot index of the Detcore namespace.
- `IPC_INFO` (F4) returns the defaults above (`msgmax` 8192, `msgmnb` 16384, `msgmni` 32000, and the other `struct msginfo` constants Linux reports), never the host's. `MSG_INFO` (F5) counts only guest queues, messages and bytes. Both return the highest slot index in use.
- `IPC_SET` (F2) changes only Detcore's record. `IPC_RMID` (F3) removes the queue at once and makes every parked sender and receiver return `EIDRM`. `msgctl` itself never waits, but it should land with `msgsnd` and `msgrcv`, which need the scheduler wait of owner decision 14.

Errno order (owner decision 6), following `ksys_msgctl`, `msgctl_stat` and `msgctl_down` in Linux `ipc/msg.c`: a negative `msqid` or `cmd` gives `EINVAL` before anything else. `IPC_INFO` and `MSG_INFO` can then fail only with `EFAULT` on the copy-out. The stat commands look the queue up (`EINVAL`), check read permission (`EACCES`, skipped for `MSG_STAT_ANY`) and copy out last (`EFAULT`). `IPC_SET` copies `buf` in first (`EFAULT`); `IPC_SET` and `IPC_RMID` then look the queue up (`EINVAL`) and require the owner, the creator or `CAP_SYS_ADMIN` (`EPERM`); `IPC_SET` then rejects a `msg_qbytes` above MSGMNB without `CAP_SYS_RESOURCE` (`EPERM`). Any other `cmd` gives `EINVAL`.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.
- A forwarded `IPC_INFO` or `MSG_INFO` needs no queue and would report the host's limits and usage; the model reports the defaults and the guest's own counts instead.

Today every behaviour in `### Flags and options` is replaced by the same `-1`/`ENOSYS` with no side effect. Under the target each one behaves as on Linux, against the Detcore namespace.

## Recommended tests

- T1 (covers F1, F4, F5, F6, F7, F8): under `hermit run --verify` on ptrace, on a guest queue holding two 10-byte messages: `IPC_STAT` gives the expected mode, `msg_qnum` 2, `msg_cbytes` 20 and `msg_qbytes` 16384; `IPC_INFO` gives `msgmax` 8192, `msgmnb` 16384 and `msgmni` 32000; `MSG_INFO` gives `msgpool` 1, `msgmap` 2 and `msgtql` 20; `MSG_STAT` and `MSG_STAT_ANY` on index 0 return the queue's id; `cmd` 99 and `cmd` -1 give `EINVAL`; `IPC_STAT` with a `NULL` buffer gives `EFAULT`. Compare with the native run in a fresh IPC namespace, except pids and times.
- T2 (covers F2, F3, F4): `IPC_SET` changes the mode and lowers `msg_qbytes`; raising it above 16384 succeeds as uid 0 and gives `EPERM` after the guest drops to uid 1000 with no capabilities (owner decision 4), as do `IPC_SET` and `IPC_RMID` on a queue owned by uid 0. `IPC_RMID` wakes a blocked receiver with `EIDRM`. The harness creates a queue outside Hermit and passes its id: the guest's `IPC_RMID` on it gives `EINVAL`, and the host queue still exists afterwards. The guest's `IPC_INFO` shows the defaults even when the host's `kernel.msgmnb` differs.
- T3 (covers none): the same results in fail-closed mode and under the compatibility opt-out, and a record followed by a replay reproduces them.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/304#issuecomment-5872870971
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/304: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; msgctl records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/304 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (detcore/src/lib.rs:2237-2241 returns a fixed ENOSYS for the whole System V message-queue family without forwarding, which also keeps IPC_INFO/MSG_INFO host limits out of the guest); linux_determinism stays conditional (depends on cmd); 8 flag behaviours and 3 tests listed; no test calls msgctl at this SHA, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 4 and 13 applied, decision 14 noted (msgctl lands with the waiting message-queue calls); target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the msgctl target and the kernel errno order (decision 6); T1-T3 rewritten as behaviour tests, ids and covers kept except T2 now also covers F4; no new F/T ids; P3 review notes marked superseded; no open questions existed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `msgctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msgctl&body=Discussion+for+the+%60msgctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsgctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msgctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: msgctl" (https://github.com/rrnewton/hermit/issues/304), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/304.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=msgctl`, and by confirming hermit currently routes `msgctl` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
msgctl(-1, IPC_RMID, NULL)      = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/304#issuecomment-5872870971

### P3 review notes (2026-10-03, hermit f4055eb9cae4)

- The legacy recommended action **NEEDS-HANDLER** predates the deterministic `ENOSYS` refusal whose code is tagged `TODO-HUMAN-REVIEW(#731)` (https://github.com/rrnewton/hermit/issues/731). At this SHA the System V message-queue family is refused by `is_unsupported_async_ipc_syscall`, so the target is `refuse`, not a new handler.
- The 2026-07-24 strace section above says Hermit routed this call through a temporary pass-through and that a harness reached the host kernel under `hermit run`. That is history: at this SHA Detcore refuses the call before it reaches the host.
- A later upgrade to `model` remains possible if a workload needs this interface; Hermit design lists what it would need.
- Superseded 2026-10-04 by owner decision 9: the System V message-queue family is to be modelled, so the first and third notes no longer describe the target; see `Target (owner decision 9)` under Hermit design.
