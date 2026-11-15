---
schema: syscall-pages/v1
syscall: msgget
nr_x86_64: 68
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/305
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msgget&body=Discussion+for+the+%60msgget%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsgget.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msgget%60%2C+or+in+what+the+page+says%3F%0A
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

`msgget(key, msgflg)` creates or looks up a System V message queue and returns its kernel-assigned identifier. Detcore refuses it: every call returns a fixed `ENOSYS` without reading the arguments or reaching the kernel (`detcore/src/lib.rs:2237`), as on a kernel built without System V IPC. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: [msgget(2)](https://man7.org/linux/man-pages/man2/msgget.2.html).

## Linux semantics

The identifier is an integer that encodes a slot index and a per-slot sequence number in the caller's IPC namespace, and it is namespace-global: any process in the namespace that knows it can send to or receive from the queue, subject to its permission bits. A queue lives until `msgctl(IPC_RMID)`, not until its creator exits. A new queue's byte limit `msg_qbytes` starts at `kernel.msgmnb`; `kernel.msgmni` limits the number of queues, and `kernel.msg_next_id` can preset the next identifier.

Host evidence (details under Additional information): a nonexistent key without `IPC_CREAT` failed with `ENOENT` (`msgget(0x7fffffff, 000)`).

### Flags and options

- F1: `key == IPC_PRIVATE`: always creates a new queue, whether or not `IPC_CREAT` is given.
- F2: `IPC_CREAT` with a non-private key: creates the queue if the key does not exist, otherwise returns the existing identifier.
- F3: `IPC_CREAT | IPC_EXCL`: fails with `EEXIST` if the key already exists.
- F4: no `IPC_CREAT`: returns the identifier of an existing key, or fails with `ENOENT` if there is none.
- F5: the low nine bits of `msgflg` are the permission mode of a new queue. When an existing key is looked up, they are checked against the queue's mode and a missing permission fails with `EACCES`.
- F6: creating a queue beyond `kernel.msgmni` fails with `ENOSPC`, and a failed allocation with `ENOMEM`. The new queue's `msg_qbytes` comes from `kernel.msgmnb`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The result is a function of the key, the flags and the IPC namespace's state: existing keys and permissions, the limits, and the allocation history that decides the next identifier. In a fresh private IPC namespace with a deterministic call order, identifiers and errors repeat; in a shared namespace other processes change all of these, and the queue's contents come from whoever sends to it. Under Hermit none of this reaches the guest, because the call is refused.

### Nondeterminism sources

- The kernel-assigned identifier, which depends on earlier allocations in the namespace and on `kernel.msg_next_id`
- Keys and permissions created by other processes, including concurrent creators racing with `IPC_CREAT | IPC_EXCL`
- `kernel.msgmni` and `kernel.msgmnb`, and system-wide usage
- The queue is state shared between processes, so its messages depend on other processes' `msgsnd` and `msgrcv` calls

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `msgget` as Determinized (`detcore/src/syscall_classification.rs:415`), in the `TODO-HUMAN-REVIEW(#731)` block for Linux native AIO, POSIX message queues and System V message queues (`detcore/src/syscall_classification.rs:387`).
- The predicate `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) matches it (`detcore/src/syscall_classification.rs:1066`), together with every other System V IPC call, the POSIX message queue calls and Linux native AIO. Its doc comment (`detcore/src/syscall_classification.rs:1034`) gives the reason: the System V families operate on global, key-addressed kernel objects that persist across runs and are shared with the whole host, so forwarding them is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_SYSVIPC` returns.
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2237`-`2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It is an arm of the `match classify_syscall(..)` at `detcore/src/lib.rs:2028`, ahead of the typed match, because the pinned Reverie has no typed form for these calls (comment at `detcore/src/lib.rs:2232`). It reads no argument and forwards nothing to the host. It does not depend on `panic_on_unsupported_syscalls`, which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1325`), the boundary that backends running guest syscalls outside this dispatcher consult to enforce the same refusal (`detcore/src/lib.rs:226`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the Determinized classification and the predicate membership for `Sysno::msgget` (`detcore/src/syscall_classification.rs:2156`, assertion at line 2170). It pins membership only; what a guest sees is exercised by the programs under Recommended tests.

Today's consequence for every flag: F1 to F6 all return `ENOSYS` under Hermit. A request Linux would grant creates nothing, and a request Linux rejects (`EEXIST`, `ENOENT`, `EACCES`, `ENOSPC`, `ENOMEM`) also gets `ENOSYS`. `msgsnd`, `msgrcv` and `msgctl` are refused by the same arm, so the whole System V message-queue family is absent to the guest.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `msgget`: a new queue is empty, its `msg_qbytes` is MSGMNB (16384), and its times are 0 except `msg_ctime`, the virtual creation time. F1 to F5 behave as on Linux against the Detcore namespace. For F6, the 32001st queue (MSGMNI) gives `ENOSPC`. `msgget` never waits, but it should land with `msgsnd`, `msgrcv` and `msgctl`, which need the scheduler wait of owner decision 14.

Errno order (owner decision 6), following `ksys_msgget`, `ipcget_public` and `newque` in Linux `ipc/msg.c` and `ipc/util.c`: with a non-private key the key is looked up first. Absent without `IPC_CREAT` gives `ENOENT`; present with `IPC_CREAT | IPC_EXCL` gives `EEXIST`, then a missing permission gives `EACCES` (`msgget` has no size check). Creation checks allocation (`ENOMEM`), then MSGMNI (`ENOSPC`).

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

The arm and the classification entries carry `TODO-HUMAN-REVIEW(#731)` and `TODO-HUMAN-REVIEW(PR-859)` markers (`detcore/src/lib.rs:2221`, `detcore/src/lib.rs:2235`, `detcore/src/syscall_classification.rs:1033`, `detcore/src/syscall_classification.rs:1049`). They ask for a human review of the refusal boundary; they do not report a known defect. Owner decision 9 (2026-10-04) settles that review: the family is to be modelled, so this refusal is temporary.

Every recommended test runs under `hermit run --verify` on ptrace and asserts the Linux result its text gives. Where the test says so, it also runs the program natively in a fresh IPC namespace (`unshare --user --map-root-user --ipc`), which starts empty with the default limits, as the model does, and compares the results other than pids and times. At hermit f4055eb9cae4 each of these tests fails with `ENOSYS`; an existing program named below that asserts `ENOSYS` checks today's refusal and must change when the model lands.

## Recommended tests

- T1 (covers F1, F5): in a fresh run, `msgget(IPC_PRIVATE, IPC_CREAT | 0600)` returns 0 and a second identical call returns 1; `msgctl(IPC_STAT)` on the first shows mode 0600, `msg_qnum` 0 and `msg_qbytes` 16384. Compare with the native run in a fresh IPC namespace, except pids and times.
  Existing candidate: `tests/c/sysv_ipc_refusal.c:55` (cell `c-programs/sysv-ipc-refusal`, `tests/e2e/manifests/c-programs.yaml:14033`, verify ci true with ptrace enabled at line 14053). It calls through the glibc wrapper, which is why the facts count no test file naming `msgget`. These programs assert today's `ENOSYS`; they must change to the Linux results above when the model lands.
- T2 (covers F2, F3, F4, F5): with a fixed key K, flags 0 gives `ENOENT`; `IPC_CREAT | 0600` creates the queue; a second `IPC_CREAT | 0600` returns the same identifier; `IPC_CREAT | IPC_EXCL | 0600` gives `EEXIST`; flags 0 returns the identifier. After the guest drops to uid 1000 with no capabilities (owner decision 4), a lookup of K gives `EACCES`. A host queue the harness creates with key K before the run is never found, and the host's `/proc/sysvipc/msg` gains no queue. Compare with the native run in a fresh IPC namespace, except pids and times.
- T3 (covers F1, F6): 32000 private queues are created and the 32001st gives `ENOSPC`. The host's `/proc/sysvipc/msg` has the same number of queues before and after, so the guest uses none of the host's `kernel.msgmni` budget.
- T4 (covers F1): record a guest that creates a queue and passes messages through it, then replay it; both phases see the same identifiers and messages. The `c-programs/sysv-ipc-refusal` cell disables record/replay, so this needs a replay cell or a cargo test.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/305#issuecomment-5872873150
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/305: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; msgget records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/305 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every msgget call without reading arguments or reaching the host, detcore/src/lib.rs:2237-2241); linux_determinism stays conditional; 6 flag behaviours and 4 tests listed; no duplicate; owner question recorded under Open questions (keep refuse, or model System V IPC); no coverage token, so ptrace_verification stays untested and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 4 and 13 applied, decision 14 noted (msgget lands with the waiting message-queue calls); target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the msgget target and the kernel errno order (decision 6); T1-T4 rewritten as behaviour tests, ids and covers kept except T2 now also covers F5; no new F/T ids; removed both open questions (answered by decision 9 and by the host IPC namespace finding, hermit-cli/src/bin/hermit/container.rs:442-453)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `msgget`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msgget&body=Discussion+for+the+%60msgget%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsgget.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msgget%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: msgget" (https://github.com/rrnewton/hermit/issues/305), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/305.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=msgget`, and by confirming hermit currently routes `msgget` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
msgget(0x7fffffff, 000)         = -1 ENOENT (No such file or directory)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/305#issuecomment-5872873150

### Notes

- The "temporary-passthrough" statement in the 2026-07-24 evidence, and its note that the harness ran under `hermit run` with exit 0, describe the legacy code; at hermit f4055eb9cae4 the call is refused (Hermit design).
