---
schema: syscall-pages/v1
syscall: semctl
nr_x86_64: 66
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/427
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+semctl&body=Discussion+for+the+%60semctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsemctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60semctl%60%2C+or+in+what+the+page+says%3F%0A
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

`semctl(semid, semnum, cmd, arg)` queries or changes a System V semaphore set or one of its semaphores, or reports system-wide limits and usage. Detcore refuses it: every command returns a fixed `ENOSYS` without reading the arguments or reaching the kernel (`detcore/src/lib.rs:2237`). Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: [semctl(2)](https://man7.org/linux/man-pages/man2/semctl.2.html).

## Linux semantics

The fourth argument is a `union semun` whose meaning depends on `cmd`: an integer value, a pointer to `semid_ds` or `seminfo`, or an array of values. The status structure `semid_ds` carries the owner and creator ids, the mode, the number of semaphores, and the wall-clock times of the last `semop` and the last change (`sem_otime`, `sem_ctime`). Identifiers are namespace-global, so any process in the namespace that knows one can use it.

Host evidence (details under Additional information): an invalid identifier failed with `EINVAL` (`semctl(-1, 0, IPC_STAT, NULL)`).

### Flags and options

- F1: `IPC_STAT`: copy the set's `semid_ds` to `arg.buf`. Needs read permission (`EACCES` otherwise).
- F2: `IPC_SET`: set the owner uid and gid and the low nine mode bits from `arg.buf`, and update `sem_ctime`. Only the owner, the creator or a `CAP_SYS_ADMIN` caller may do it (`EPERM` otherwise).
- F3: `IPC_RMID`: remove the set at once. Every process waiting in `semop` on it wakes with `EIDRM`. Same permission rule as `IPC_SET`.
- F4: `IPC_INFO` and `SEM_INFO` (Linux-specific): copy the system limits, or the limits with current usage, to `arg.__buf` (`struct seminfo`), and return the highest slot index in use.
- F5: `SEM_STAT` and `SEM_STAT_ANY` (Linux-specific): like `IPC_STAT`, but `semid` is an index into the kernel's slot table, and the call returns the identifier of that slot. `SEM_STAT_ANY` (Linux 4.17 and later) skips the read-permission check.
- F6: `GETVAL`, `GETPID`, `GETNCNT`, `GETZCNT`: return the value of semaphore `semnum`, the pid of its last `semop`, or how many processes wait for it to increase or to reach 0.
- F7: `GETALL`: copy every value of the set to `arg.array`.
- F8: `SETVAL`: set semaphore `semnum` to `arg.val`, clear every process's `SEM_UNDO` adjustment for it, update `sem_ctime` and wake waiters whose condition now holds. A value below 0 or above `SEMVMX` fails with `ERANGE`.
- F9: `SETALL`: set every value from `arg.array`, with the same undo clearing, wakeups and `ERANGE` rule as `SETVAL`.
- F10: errors: an unknown `cmd`, a `semnum` outside the set, or an identifier (or index) that names no set gives `EINVAL`; a removed set gives `EIDRM`; an unreadable or unwritable `arg` pointer gives `EFAULT`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The commands that change the set (F2, F3, F8, F9) and the errors (F10) depend only on the arguments and the set's state. The query commands depend on what other processes did: `GETVAL`, `GETALL`, `GETNCNT` and `GETZCNT` report values and waiter counts other processes change, `GETPID` reports a host pid, `IPC_STAT` and `SEM_STAT` report host ids and wall-clock times, and `IPC_INFO`/`SEM_INFO` report host-wide limits and usage. The changes are visible to every other process using the set. Under Hermit none of this reaches the guest, because the call is refused.

### Nondeterminism sources

- Semaphore values and waiter counts changed by other processes
- `sempid` (`GETPID`), a host pid, and the owner and creator ids, which are host ids
- `sem_otime` and `sem_ctime`, which are wall-clock times
- System-wide limits and usage (`IPC_INFO`, `SEM_INFO`) and the slot indexes used by `SEM_STAT`
- Wakeups caused by `SETVAL`, `SETALL` and `IPC_RMID`, whose effect on waiters depends on host scheduling

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `semctl` as Determinized (`detcore/src/syscall_classification.rs:432`), in the block for System V semaphores and shared memory, added under `TODO-HUMAN-REVIEW(PR-859)` (`detcore/src/syscall_classification.rs:428`).
- The predicate `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) matches it (`detcore/src/syscall_classification.rs:1071`), together with every other System V IPC call, the POSIX message queue calls and Linux native AIO. Its doc comment (`detcore/src/syscall_classification.rs:1034`) gives the reason: the System V families operate on global, key-addressed kernel objects that persist across runs and are shared with the whole host, so forwarding them is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_SYSVIPC` returns.
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2237`-`2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It is an arm of the `match classify_syscall(..)` at `detcore/src/lib.rs:2028`, ahead of the typed match, because the pinned Reverie has no typed form for these calls (comment at `detcore/src/lib.rs:2232`). It reads no argument and forwards nothing to the host. It does not depend on `panic_on_unsupported_syscalls`, which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1325`), the boundary that backends running guest syscalls outside this dispatcher consult to enforce the same refusal (`detcore/src/lib.rs:226`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the Determinized classification and the predicate membership for `Sysno::semctl` (`detcore/src/syscall_classification.rs:2160`, assertion at line 2170). It pins membership only; what a guest sees is exercised by the programs under Recommended tests.

Today's consequence for every flag: F1 to F10 all return `ENOSYS` under Hermit. `arg` is neither read nor written, no set is changed or removed, no waiter is woken, and the error paths that would give `EINVAL`, `EIDRM` or `EFAULT` also give `ENOSYS`.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `semctl`:

- `IPC_STAT`, `SEM_STAT`, `SEM_STAT_ANY`, `GETVAL`, `GETALL` and `GETPID` (F1, F5, F6, F7) read Detcore's set; `GETPID` returns a virtual pid. `GETNCNT` and `GETZCNT` (F6) count the guest threads parked on the semaphore by the scheduler wait of owner decision 14.
- `IPC_INFO` and `SEM_INFO` (F4) report the defaults above; for `SEM_INFO`, `semusz` is the number of guest sets and `semaem` the number of guest semaphores, as Linux reports them. Both return the highest slot index in use.
- `SETVAL` and `SETALL` (F8, F9) clear the matching undo entries in every process, set `sem_ctime` to virtual time, and make parked waiters whose condition now holds runnable.
- `IPC_SET` (F2) changes only Detcore's record. `IPC_RMID` (F3) removes the set at once, frees its undo entries, and makes every parked waiter return `EIDRM`. `semctl` itself never waits, but it should land with `semop` and `semtimedop`.

Errno order (owner decision 6), following `ksys_semctl`, `semctl_setval`, `semctl_main` and `semctl_down` in Linux `ipc/sem.c`: `semid` below 0 gives `EINVAL` before anything else. `IPC_INFO` and `SEM_INFO` can then fail only with `EFAULT`. The stat commands look the set up (`EINVAL`), check read permission (`EACCES`, skipped for `SEM_STAT_ANY`) and copy out last (`EFAULT`). `SETVAL` checks the value first (`ERANGE`), then looks the set up (`EINVAL`), then `semnum` (`EINVAL`), then write permission (`EACCES`). `GETVAL`, `GETPID`, `GETNCNT`, `GETZCNT`, `GETALL` and `SETALL` look the set up (`EINVAL`), check permission (`EACCES`; write for `SETALL`), then `GETALL` copies out (`EFAULT`) and `SETALL` copies in (`EFAULT`) and range-checks every value (`ERANGE`); the single-semaphore commands check `semnum` last (`EINVAL`). `IPC_SET` copies in before the lookup (`EFAULT`); `IPC_SET` and `IPC_RMID` then look the set up (`EINVAL`) and require the owner, the creator or `CAP_SYS_ADMIN` (`EPERM`). Any other `cmd` gives `EINVAL`.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

The arm and the classification entries carry `TODO-HUMAN-REVIEW(#731)` and `TODO-HUMAN-REVIEW(PR-859)` markers (`detcore/src/lib.rs:2221`, `detcore/src/lib.rs:2235`, `detcore/src/syscall_classification.rs:1033`, `detcore/src/syscall_classification.rs:1049`). They ask for a human review of the refusal boundary; they do not report a known defect. Owner decision 9 (2026-10-04) settles that review: the family is to be modelled, so this refusal is temporary.

Every recommended test runs under `hermit run --verify` on ptrace and asserts the Linux result its text gives. Where the test says so, it also runs the program natively in a fresh IPC namespace (`unshare --user --map-root-user --ipc`), which starts empty with the default limits, as the model does, and compares the results other than pids and times. At hermit f4055eb9cae4 each of these tests fails with `ENOSYS`; an existing program named below that asserts `ENOSYS` checks today's refusal and must change when the model lands.

## Recommended tests

- T1 (covers F1, F4, F5, F6, F7): on a guest set, `IPC_STAT` gives the expected mode, `sem_nsems` and uid 0; `IPC_INFO` gives `semmsl` 32000, `semmns` 1024000000, `semopm` 500, `semmni` 32000 and `semvmx` 32767; `SEM_INFO` gives `semusz` 1; `SEM_STAT` and `SEM_STAT_ANY` on index 0 return the set's identifier; `GETVAL` and `GETALL` return the values set earlier; `GETPID` returns the last caller's virtual pid; `GETNCNT` is 1 while one thread waits for an increase and `GETZCNT` is 1 while one waits for zero. Compare with the native run in a fresh IPC namespace, except pids and times.
- T2 (covers F2, F3, F8, F9): `IPC_SET` changes the mode and `sem_ctime`. `SETVAL` wakes a waiting thread, and a process that had adjusted the semaphore with `SEM_UNDO` before the `SETVAL` exits without changing the value. `SETALL` sets every value, and a value of 32768 gives `ERANGE`. `IPC_RMID` wakes a waiting thread with `EIDRM`. After the guest drops to uid 1000 with no capabilities (owner decision 4), `IPC_SET` and `IPC_RMID` on a set owned by uid 0 give `EPERM`. A host set whose identifier the harness passes in is left unchanged.
- T3 (covers F10): `cmd` 99 gives `EINVAL`; `semid` -1 gives `EINVAL`; `GETVAL` with `semnum` 1000 gives `EINVAL`; `SETVAL` of 40000 on an unknown identifier gives `ERANGE`, because the value is checked before the lookup; `IPC_STAT` with a NULL `arg.buf` gives `EFAULT`.
- T4 (covers F6): record a guest that calls `semctl(GETVAL)` and `semctl(GETNCNT)` while another thread waits, then replay it; both phases see the same values.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/427: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every semctl call without reading arguments or reaching the host, detcore/src/lib.rs:2237-2241); linux_determinism stays conditional; 10 flag behaviours and 4 tests listed; no duplicate; owner question recorded under Open questions (keep refuse, or model System V IPC); no coverage token, so ptrace_verification stays untested and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 14, 4 and 13 applied; target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the semctl target (GETNCNT/GETZCNT count parked threads, SETVAL/SETALL clear undo entries and wake waiters, IPC_RMID gives EIDRM) and the kernel errno order (decision 6); T1-T4 rewritten as behaviour tests, ids and covers kept; no new F/T ids; removed both open questions (answered by decision 9 and by the host IPC namespace finding, hermit-cli/src/bin/hermit/container.rs:442-453)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `semctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+semctl&body=Discussion+for+the+%60semctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsemctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60semctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: semctl" (https://github.com/rrnewton/hermit/issues/427), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=semctl` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `semctl` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
semctl(-1, 0, IPC_STAT, NULL)   = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Notes

- The "temporary-passthrough" statement in the 2026-07-24 evidence describes the legacy code; at hermit f4055eb9cae4 the call is refused (Hermit design). No test file at that SHA names `semctl`.
