---
schema: syscall-pages/v1
syscall: semop
nr_x86_64: 65
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/434
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+semop&body=Discussion+for+the+%60semop%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsemop.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60semop%60%2C+or+in+what+the+page+says%3F%0A
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

`semop(semid, sops, nsops)` applies an array of operations to a System V semaphore set atomically, and can wait until they can all be applied. Detcore refuses it: every call returns a fixed `ENOSYS` without reading the arguments or reaching the kernel (`detcore/src/lib.rs:2237`), so a guest thread never waits on a host semaphore. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: [semop(2)](https://man7.org/linux/man-pages/man2/semop.2.html).

## Linux semantics

Each `struct sembuf` names a semaphore (`sem_num`), an operation (`sem_op`) and flags (`sem_flg`). The array is applied all or nothing: if any operation would wait, none is applied and the caller waits (or fails with `IPC_NOWAIT`) until the whole array can proceed. On success the kernel sets each touched semaphore's `sempid` to the caller's pid and the set's `sem_otime` to the wall-clock time. When several callers wait, which one proceeds after a change depends on the kernel's queue order and on host scheduling. `semtimedop` is the same call with a timeout and is tracked separately.

Host evidence (details under Additional information): an invalid identifier failed with `EINVAL` (`semop(-1, [{sem_num=0, sem_op=0, sem_flg=0}], 1)`).

### Flags and options

- F1: `sem_op > 0`: add to the semaphore's value. It never waits. A result above `SEMVMX` (32767) fails with `ERANGE`.
- F2: `sem_op < 0`: if the value is at least `|sem_op|`, subtract; otherwise wait until it is, until the set is removed (`EIDRM`), or until a signal arrives (`EINTR`).
- F3: `sem_op == 0`: wait until the value is 0.
- F4: `IPC_NOWAIT` in `sem_flg`: fail with `EAGAIN` instead of waiting.
- F5: `SEM_UNDO` in `sem_flg`: record the adjustment and reverse it when the process exits. The undo list is shared by threads created with `CLONE_SYSVSEM`.
- F6: several operations in one call are applied atomically and in array order, so a later entry sees the effect of an earlier one on the same semaphore.
- F7: argument errors: `nsops` 0 gives `EINVAL`, `nsops` above `SEMOPM` gives `E2BIG`, a `sem_num` beyond the set gives `EFBIG`, an identifier that names no set gives `EINVAL`, a removed set gives `EIDRM`, missing permission gives `EACCES`, and an unreadable `sops` gives `EFAULT`.
- F8: a signal handler that interrupts a wait makes the call fail with `EINTR`. The call is never restarted, even with `SA_RESTART`.
- F9: side effects of success: `sempid` of each touched semaphore and the set's `sem_otime` change, and waiters whose condition now holds are woken.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. A call that does not wait is decided by the set's current values, but those values are written by other processes, and a call that waits ends when another process changes the set. Which of several waiters proceeds, and whether a signal ends the wait first, depends on host scheduling. The call repeats only when every process using the set runs inside one deterministic schedule. Under Hermit none of this reaches the guest, because the call is refused.

### Nondeterminism sources

- Semaphore values written by other processes sharing the set, inside or outside the guest
- The order in which the kernel wakes waiters, and host scheduling
- Signal timing, which decides whether a wait ends with `EINTR` (never restarted)
- `SEM_UNDO` adjustments applied when another process exits
- `sempid` and `sem_otime` written by the call, later visible through `semctl`

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `semop` as Determinized (`detcore/src/syscall_classification.rs:436`), in the block for System V semaphores and shared memory, added under `TODO-HUMAN-REVIEW(PR-859)` (`detcore/src/syscall_classification.rs:428`).
- The predicate `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) matches it (`detcore/src/syscall_classification.rs:1075`), together with every other System V IPC call, the POSIX message queue calls and Linux native AIO. Its doc comment (`detcore/src/syscall_classification.rs:1034`) gives the reason: the System V families operate on global, key-addressed kernel objects that persist across runs and are shared with the whole host, so forwarding them is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_SYSVIPC` returns.
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2237`-`2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It is an arm of the `match classify_syscall(..)` at `detcore/src/lib.rs:2028`, ahead of the typed match, because the pinned Reverie has no typed form for these calls (comment at `detcore/src/lib.rs:2232`). It reads no argument and forwards nothing to the host. It does not depend on `panic_on_unsupported_syscalls`, which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1325`), the boundary that backends running guest syscalls outside this dispatcher consult to enforce the same refusal (`detcore/src/lib.rs:226`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the Determinized classification and the predicate membership for `Sysno::semop` (`detcore/src/syscall_classification.rs:2162`, assertion at line 2170). It pins membership only; what a guest sees is exercised by the programs under Recommended tests.

Today's consequence for every flag: F1 to F9 all return `ENOSYS` under Hermit, at once. No thread waits in the kernel where the deterministic scheduler cannot see it, so the wait paths (F2, F3), the `EINTR` path (F8) and the wakeups (F9) cannot happen. `sops` is not read, so `EFAULT` and the other F7 errors also become `ENOSYS`.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `semop`:

- The operations apply atomically and in array order against Detcore's values, as `perform_atomic_semop` does: if any entry would wait or fail, none takes effect (F1, F6).
- `SEM_UNDO` (F5) records adjustments in a per-process undo list. Threads created with `CLONE_SYSVSEM` share it, so Detcore must read that flag from `clone` and `clone3`. The adjustments are applied when the last task sharing the list exits, clamped to 0..SEMVMX as `exit_sem` does.
- Success sets `sempid` of each touched semaphore to the caller's virtual pid and `sem_otime` to virtual time, and makes parked waiters whose condition now holds runnable, in the order the kernel's `update_queue` scans its queues (F9).

Errno order (owner decision 6), following `do_semtimedop` and `__do_semtimedop` in Linux `ipc/sem.c`: `nsops` above SEMOPM gives `E2BIG`, then `nsops` 0 gives `EINVAL`, then an unreadable `sops` gives `EFAULT`, then `semid` below 0 gives `EINVAL`. With `SEM_UNDO` on any entry, the undo-list lookup comes next (`EINVAL` for an unknown set, `ENOMEM`, `EIDRM`). Then the set lookup gives `EINVAL`, a `sem_num` at or beyond `sem_nsems` gives `EFBIG`, and a missing permission gives `EACCES` (write if any entry alters a value, read otherwise). Only then are the operations tried, in array order: a value that would pass SEMVMX gives `ERANGE`, and an entry that cannot proceed gives `EAGAIN` with `IPC_NOWAIT` or waits. A wait ends in success, `EIDRM` or `EINTR`.

Blocking (owner decision 14): today no form of `semop` blocks, because every call returns `ENOSYS` at once (`detcore/src/lib.rs:2237`-`2241`), so nothing sleeps in the kernel while holding the turn. Under the model the call is never forwarded. Detcore runs the operation against its own state as a non-blocking probe. If Linux would wait, the thread parks in the deterministic scheduler, and it becomes runnable to probe again when a guest operation on the set changes a value it waits for, when an `IPC_RMID` removes the object (`EIDRM`), or when a signal arrives (`EINTR`, never restarted). Until that scheduler wait exists, the blocking form keeps today's `ENOSYS`. A family that works only with `IPC_NOWAIT` is not a Linux configuration a guest expects, so the semaphore calls should keep `ENOSYS` as a unit until the wait lands; the shared-memory calls never wait and can land first.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

The arm and the classification entries carry `TODO-HUMAN-REVIEW(#731)` and `TODO-HUMAN-REVIEW(PR-859)` markers (`detcore/src/lib.rs:2221`, `detcore/src/lib.rs:2235`, `detcore/src/syscall_classification.rs:1033`, `detcore/src/syscall_classification.rs:1049`). They ask for a human review of the refusal boundary; they do not report a known defect. Owner decision 9 (2026-10-04) settles that review: the family is to be modelled, so this refusal is temporary.

Every recommended test runs under `hermit run --verify` on ptrace and asserts the Linux result its text gives. Where the test says so, it also runs the program natively in a fresh IPC namespace (`unshare --user --map-root-user --ipc`), which starts empty with the default limits, as the model does, and compares the results other than pids and times. At hermit f4055eb9cae4 each of these tests fails with `ENOSYS`; an existing program named below that asserts `ENOSYS` checks today's refusal and must change when the model lands.

## Recommended tests

- T1 (covers F1, F2, F3, F6): on a guest set, `+1` then `-1` succeed and `GETVAL` follows them; a three-entry array applies in order, so a later entry sees an earlier one; an array whose second entry cannot proceed under `IPC_NOWAIT` leaves the first unapplied; a `+1` that would take the value above 32767 gives `ERANGE`. One thread waits in `sem_op = -1` until another posts, and one waits in `sem_op = 0` until the value reaches 0; the output is the same in both `--verify` runs. Until the scheduler wait exists, the waiting cases see `ENOSYS`.
- T2 (covers F4, F5): `IPC_NOWAIT` on a semaphore with value 0 gives `EAGAIN`. A child does `-1` with `SEM_UNDO` on a value of 1 and exits, and the value is 1 again. A thread created by `pthread_create`, which passes `CLONE_SYSVSEM`, adjusts with `SEM_UNDO` and exits: the value is not undone until the whole process exits.
- T3 (covers F7): `nsops` 0 gives `EINVAL`; `nsops` 501 gives `E2BIG` even with a NULL `sops`, because the count is checked first; a `sem_num` of 1000 gives `EFBIG`; `sops == NULL` with `nsops` 1 gives `EFAULT`; `semid` -1 gives `EINVAL`; after the guest drops to uid 1000 with no capabilities (owner decision 4), an altering operation on a 0600 set owned by uid 0 gives `EACCES`. Compare with the native run in a fresh IPC namespace, except pids and times.
- T4 (covers F2, F8, F9): a thread waits on a semaphore with value 0. A `SIGALRM` handler installed with `SA_RESTART` interrupts the wait and the call returns `EINTR`. A `+1` from another thread wakes a second waiter, and `GETPID` then returns the poster's virtual pid. `IPC_RMID` wakes a third waiter with `EIDRM`. A host set whose identifier the harness passes in is never touched.
- T5 (covers F1): record a guest in which two threads synchronize through a semaphore, then replay it; both phases see the same order of operations and output.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/434: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every semop call without reading arguments or reaching the host, detcore/src/lib.rs:2237-2241); linux_determinism stays nondeterministic; 9 flag behaviours and 5 tests listed; no duplicate; owner question recorded under Open questions (keep refuse, or model System V IPC); no coverage token, so ptrace_verification stays untested and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 14, 4 and 13 applied; target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the semop target (atomic array order, SEM_UNDO lists shared under CLONE_SYSVSEM, update_queue wake order), the kernel errno order (decision 6) and the decision-14 rule (non-blocking probe plus scheduler wait; the semaphore calls keep ENOSYS as a unit until that wait exists); T1-T5 rewritten as behaviour tests, ids and covers kept; no new F/T ids; removed both open questions (answered by decision 9 and by the host IPC namespace finding, hermit-cli/src/bin/hermit/container.rs:442-453)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `semop`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+semop&body=Discussion+for+the+%60semop%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsemop.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60semop%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: semop" (https://github.com/rrnewton/hermit/issues/434), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=semop` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `semop` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
semop(-1, [{sem_num=0, sem_op=0, sem_flg=0}], 1) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Notes

- The "temporary-passthrough" statement in the 2026-07-24 evidence describes the legacy code; at hermit f4055eb9cae4 the call is refused (Hermit design). No test file at that SHA names `semop`.
