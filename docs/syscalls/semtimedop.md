---
schema: syscall-pages/v1
syscall: semtimedop
nr_x86_64: 220
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/436
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+semtimedop&body=Discussion+for+the+%60semtimedop%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsemtimedop.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60semtimedop%60%2C+or+in+what+the+page+says%3F%0A
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

`semtimedop(semid, sops, nsops, timeout)` applies an array of operations atomically to a System V semaphore set, like `semop`, but bounds any wait with a relative timeout. Detcore refuses it with a fixed `ENOSYS` before the kernel sees it (`detcore/src/lib.rs:2237-2241`), as part of refusing all System V IPC, which is what a kernel built without `CONFIG_SYSVIPC` returns. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

## Linux semantics

A semaphore set is a kernel object in the IPC namespace, named by the `semid` that `semget` returned. It outlives the process that created it and is visible to every process in that namespace that has permission. The `sops` array is applied all or nothing: if any operation cannot proceed, none is applied, and the caller either waits or fails. A waiter sleeps in the kernel until another process changes the semaphore values, the timeout expires, the set is removed, or a signal arrives.

The timeout is a relative `struct timespec`, measured against the host clock. A NULL timeout makes the call identical to `semop`. A signal interrupts the wait with `EINTR`; System V IPC calls are never restarted, even with `SA_RESTART`.

The host strace under Additional information shows a bad `semid` (-1) failing with `EINVAL` on Linux 6.17.

### Flags and options

- F1: `sem_op > 0`: add `sem_op` to the semaphore value (a release). Never waits. `ERANGE` if the result would exceed `SEMVMX`.
- F2: `sem_op < 0`: subtract `|sem_op|` if the value is at least that large; otherwise wait until it is.
- F3: `sem_op == 0`: wait until the semaphore value is zero.
- F4: `IPC_NOWAIT` in `sem_flg`: fail with `EAGAIN` instead of waiting.
- F5: `SEM_UNDO` in `sem_flg`: record an adjustment that the kernel undoes when the process exits.
- F6: `timeout` non-NULL: a relative `timespec`; on expiry the call fails with `EAGAIN`. An invalid `timespec` gives `EINVAL`. `timeout == NULL` waits without a bound, as `semop` does.
- F7: several operations in one `sops` array (`nsops > 1`), possibly on different semaphores of the set: applied atomically, all or none. `nsops == 0` gives `EINVAL`; `nsops > SEMOPM` gives `E2BIG`.
- F8: errors from the arguments and the object: `EINVAL` for a bad `semid`, `EFBIG` for `sem_num` outside the set, `EACCES` without permission, `EFAULT` for a bad `sops` or `timeout` pointer, `EIDRM` if the set is removed while the caller waits.
- F9: a signal during a wait: `EINTR`, never restarted.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Whether a call waits, how long it waits, and which of several waiters proceeds first depend on how the host schedules the other processes that use the set. The timeout is compared against the host clock, so a call that waits can end either with success or with `EAGAIN` depending on real elapsed time. The set itself lives in the IPC namespace and can be changed by processes outside the guest; it also survives the run, so a second run can find state the first one left. The call repeats only when every user of the set runs inside one deterministic schedule, no outside process touches it, and no wait reaches its timeout.

### Nondeterminism sources

- Cross-process waiting and wakeup order on the semaphore set
- The timeout, compared against the host clock
- Semaphore sets that persist across runs and are shared with processes outside the guest
- Signal timing, which decides whether a wait ends with `EINTR`

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `semtimedop` (`detcore/src/syscall_classification.rs:438`) in the arm that returns `SyscallClassification::Determinized` (`detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` includes it (`detcore/src/syscall_classification.rs:1077`), next to `semget`, `semop`, `semctl`, the `shm*` and `msg*` calls, the POSIX message queues and native AIO.
- In `handle_syscall_event`, the match on the classification (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2237-2241`, which returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall. The host kernel never runs it. The arm does not consult `panic_on_unsupported_syscalls`, so the result is the same with or without fail-closed mode (`refused_only_when_fail_closed: false`).
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`), so backends that must enforce Detcore's refusals refuse it too.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `semtimedop`: as for `semop`. The operations apply atomically and in array order against Detcore's values, `SEM_UNDO` uses the per-process undo lists shared under `CLONE_SYSVSEM`, and success sets `sempid` and `sem_otime` to a virtual pid and virtual time and wakes waiters whose condition now holds (F1 to F5, F7, F9). The timeout (F6) runs on virtual time (owner decision 13): a parked caller becomes runnable at the deadline and fails with `EAGAIN` if its operations still cannot proceed. A NULL timeout behaves as `semop`.

Errno order (owner decision 6), following `ksys_semtimedop`, `do_semtimedop` and `__do_semtimedop` in Linux `ipc/sem.c`: an unreadable `timeout` gives `EFAULT` before anything else. Then `nsops` above SEMOPM gives `E2BIG`, `nsops` 0 gives `EINVAL`, an unreadable `sops` gives `EFAULT`, `semid` below 0 gives `EINVAL`, and an invalid `timespec` gives `EINVAL`. With `SEM_UNDO` on any entry, the undo-list lookup comes next (`EINVAL`, `ENOMEM`, `EIDRM`). Then the set lookup gives `EINVAL`, a `sem_num` at or beyond `sem_nsems` gives `EFBIG`, and a missing permission gives `EACCES`. Only then are the operations tried in array order: `ERANGE`, `EAGAIN` under `IPC_NOWAIT`, or a wait that ends in success, `EIDRM`, `EINTR`, or `EAGAIN` at the deadline.

Blocking (owner decision 14): today no form of `semtimedop` blocks, because every call returns `ENOSYS` at once (`detcore/src/lib.rs:2237`-`2241`), so nothing sleeps in the kernel while holding the turn. Under the model the call is never forwarded. Detcore runs the operation against its own state as a non-blocking probe. If Linux would wait, the thread parks in the deterministic scheduler, and it becomes runnable to probe again when a guest operation on the set changes a value it waits for, when an `IPC_RMID` removes the object (`EIDRM`), or when a signal arrives (`EINTR`, never restarted), or when the virtual-time deadline passes (`EAGAIN`, owner decision 13). Until that scheduler wait exists, the blocking form keeps today's `ENOSYS`. A family that works only with `IPC_NOWAIT` is not a Linux configuration a guest expects, so the semaphore calls should keep `ENOSYS` as a unit until the wait lands; the shared-memory calls never wait and can land first.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

Existing test evidence at f4055eb9cae4: the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts, in its loop at `detcore/src/syscall_classification.rs:2143`, that `semtimedop` classifies as Determinized and is in the predicate. It does not run a guest and does not check the errno. No e2e program or integration test calls `semtimedop`. The closest guest-level contracts are `tests/c/sysv_sem_enosys.c` (cell `c-programs/sysv-sem-enosys`, `tests/e2e/manifests/c-programs.yaml:9663`) and `tests/c/sysv_ipc_refusal.c` (cell `c-programs/sysv-ipc-refusal`, `tests/e2e/manifests/c-programs.yaml:14033`), which check only `semget`, `shmget` and `msgget`.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8, F9): under `hermit run --verify` on ptrace, call raw `syscall(SYS_semtimedop, ...)` on a guest set once per flag shape and check the Linux result: a positive `sem_op` succeeds and a value above 32767 gives `ERANGE`; a negative and a zero `sem_op` that cannot proceed give `EAGAIN` with `IPC_NOWAIT`; `SEM_UNDO` is undone at process exit; a NULL timeout behaves as `semop`; a `timespec` with `tv_nsec` 1000000000 gives `EINVAL`; a two-operation array applies all or nothing; `nsops == 0` gives `EINVAL`; `semid == -1` gives `EINVAL`; a bad `sops` pointer gives `EFAULT`; a bad `timeout` pointer gives `EFAULT` even with `nsops == 0`, because the timeout is read first. Compare with the native run in a fresh IPC namespace, except pids and times. `tests/c/sysv_ipc_refusal.c` and `tests/c/sysv_sem_enosys.c` assert today's `ENOSYS`, so this needs a new program.
- T2 (covers F6, F9): a call with a 1-second timeout on a semaphore with value 0 returns `EAGAIN` after 1 second of virtual time, with the same output in both `--verify` runs; a `+1` from another thread before the deadline lets it succeed instead; a `SIGALRM` handler that interrupts the wait makes it return `EINTR`. Until the scheduler wait exists, these calls see `ENOSYS`.
- T3 (covers none): record and replay a guest in which a thread waits with a timeout and another posts; replay must produce the same results without consulting a host semaphore set.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/436: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns a fixed ENOSYS for all System V IPC at detcore/src/lib.rs:2237-2241, the result of a kernel without CONFIG_SYSVIPC; semget is refused too, so no guest can hold a semid); linux_determinism stays nondeterministic; 9 flag behaviours and 3 tests listed; no guest-level test calls semtimedop yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 14, 13 and 4 applied; target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the semtimedop target (as semop, timeout on virtual time), the kernel errno order (decision 6) and the decision-14 rule (non-blocking probe plus scheduler wait with a virtual deadline; ENOSYS as a unit with the other semaphore calls until that wait exists); T1-T3 rewritten as behaviour tests, ids and covers kept; no new F/T ids; replaced the "not a defect" paragraph, which described the refusal as the target; no open questions existed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `semtimedop`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+semtimedop&body=Discussion+for+the+%60semtimedop%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsemtimedop.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60semtimedop%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: semtimedop" (https://github.com/rrnewton/hermit/issues/436), legacy variant A1.

The 2026-07-24 evidence below says Hermit routed `semtimedop` through a temporary pass-through block. That was true then; at f4055eb9cae4 the call is refused with `ENOSYS` (see Hermit design).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=semtimedop` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `semtimedop` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
semtimedop(-1, [{sem_num=0, sem_op=0, sem_flg=0}], 1, {tv_sec=0, tv_nsec=0}) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
