---
schema: syscall-pages/v1
syscall: flock
nr_x86_64: 73
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/357
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+flock&body=Discussion+for+the+%60flock%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fflock.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60flock%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`flock(fd, operation)` takes or releases an advisory lock on a whole file. The lock belongs to the open file description that `fd` refers to, and a request can wait. Detcore handles it (`detcore/src/syscalls/files.rs:1270`): the kernel holds the lock table, but Detcore turns every blocking request into a `LOCK_NB` probe. If that probe finds the lock held, Detcore refuses the request with `ENOLCK`, or stops the run when it fails closed, because its scheduler cannot yet park a thread on a file lock.

Manual reference: Linux man-pages flock(2).

## Linux semantics

A lock belongs to the open file description, not to the descriptor or the process. Copies made by `dup`, `fork` or `SCM_RIGHTS` share one lock. A second `open()` of the same file creates a separate description, which conflicts with the first.

The lock is released on `LOCK_UN`, or when the last descriptor referring to that open file description closes. A blocking request sleeps in the kernel until it can be granted. If several waiters compete, which one wins depends on host scheduling.

Changing the lock already held through the same description (shared to exclusive, or the reverse) is not atomic. `flock_lock_inode` deletes the existing lock before it checks for a conflict, so a contended non-blocking conversion leaves the caller with no lock.

Argument checking happens in a fixed order:

1. A `LOCK_MAND` bit makes the call return 0 without taking a lock. Recent kernels log a one-time warning.
2. With `LOCK_NB` removed, the operation must be exactly `LOCK_SH`, `LOCK_EX` or `LOCK_UN`; anything else is `EINVAL`.
3. Then `fd` is checked.

The host strace under Additional information, `flock(-1, LOCK_SH|LOCK_EX|LOCK_NB|LOCK_UN|LOCK_MAND|LOCK_RW|0xffffff10) = 0`, shows the `LOCK_MAND` rule winning over `EBADF`.

On local filesystems, `flock` locks and `fcntl` record locks are independent. NFS emulates `flock` with whole-file `fcntl` locks, so on NFS the two kinds interact.

### Flags and options

- F1: `LOCK_SH`: take a shared lock. It is granted unless another open file description holds `LOCK_EX`; otherwise the call waits.
- F2: `LOCK_EX`: take an exclusive lock. It is granted only if no other open file description holds a lock; otherwise the call waits.
- F3: `LOCK_UN`: release the lock held through this open file description. It succeeds even when no lock is held.
- F4: `LOCK_NB` with `LOCK_SH` or `LOCK_EX`: fail with `EWOULDBLOCK` instead of waiting
- F5: `LOCK_UN|LOCK_NB`: accepted, and acts like `LOCK_UN`
- F6: conversion of a held lock on the same open file description, either an upgrade (`LOCK_SH` to `LOCK_EX`) or a downgrade. The conversion is not atomic, so a contended `LOCK_NB` conversion drops the old lock.
- F7: invalid operation: `0`, `LOCK_SH|LOCK_EX`, or any undefined bit without `LOCK_MAND` gives `EINVAL`. This check runs before `fd` is examined.
- F8: `LOCK_MAND` (32), alone or with `LOCK_READ`/`LOCK_WRITE` (`LOCK_RW`): returns 0 and takes no lock. This check runs before every other one.
- F9: `fd` is not open: `EBADF`. `LOCK_SH` or `LOCK_EX` on an `O_PATH` descriptor, which is opened for neither reading nor writing: `EBADF`.
- F10: lock identity: `dup`, `fork` and `SCM_RIGHTS` copies share one lock. A separate `open()` of the same file conflicts. The lock is released when the last descriptor referring to the open file description closes.
- F11: a signal interrupts a blocking wait: `EINTR`, or the call restarts if the handler uses `SA_RESTART`

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Which process gets a contended lock, and when, depends on the order in which the host schedules the competing processes and on when signals arrive. A process outside the guest can also hold a lock on a file the guest can see. The call repeats only when every contender is inside one deterministic schedule and no outside process touches the file.

### Nondeterminism sources

- Competing lock holders, and the order in which the kernel wakes waiters
- Scheduler and signal timing, which decide when a blocking wait ends and whether it ends with `EINTR`
- Processes outside the container that lock a file the guest can see. The handler's doc comment at `detcore/src/syscalls/files.rs:1189` records a measurement: a guest `LOCK_EX|LOCK_NB` returned `EWOULDBLOCK` while a host `flock -x` held the file, and succeeded when no host process did.
- Filesystem lock semantics, for example the `fcntl` emulation on NFS

## Hermit design

Target policy: `model`. The kernel stays the lock table, and Detcore owns every decision about waiting and about lock state it can know. The handler at `detcore/src/syscalls/files.rs:1270` already does most of this:

- Malformed operations (F7, F8) go to the kernel unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:1291`), so its order of checks and its `LOCK_MAND` behaviour are kept.
- Every `LOCK_SH`/`LOCK_EX` request is sent as `operation | LOCK_NB` (`detcore/src/syscalls/files.rs:1329`), so the kernel never blocks the sequentialized scheduler. `LOCK_UN` cannot block and is sent exactly as written.
- After a successful call, Detcore caches the granted mode on the open file description (`set_flock_mode`, `detcore/src/fd.rs:652`). The cache is read through `known_flock_mode` (`detcore/src/fd.rs:640`) and is shared by every `dup` copy, as F10 requires.
- `EWOULDBLOCK` for a caller that passed `LOCK_NB` is returned as is (`detcore/src/syscalls/files.rs:1350`). If it was a conversion, Detcore forgets the cached mode, matching Linux's non-atomic drop (F6).
- `EWOULDBLOCK` for a caller that asked to wait (`detcore/src/syscalls/files.rs:1364`) is handled in two steps. First, Detcore re-takes the lock the conversion dropped, so the refusal has no side effect. Then it refuses through `refuse_unserviceable_operation` (`detcore/src/syscalls/helpers.rs:77`). Outside fail-closed mode that returns `ENOLCK`; in fail-closed mode it stops the run.
- If a blocking request comes in on a description whose lock state Detcore does not know, Detcore refuses before it sends any probe (`detcore/src/syscalls/files.rs:1310`). It first runs an `F_GETFD` check, so a bad `fd` still gives `EBADF`.

Why `model` and not the alternatives:

- Before the change the code tags `TODO-HUMAN-REVIEW(#2373)` (https://github.com/rrnewton/hermit/issues/2373), the handler was an unconditional no-op success. That was `emulate`, and it removed mutual exclusion; the doc comment at `detcore/src/syscalls/files.rs:1189` records two processes holding the same `LOCK_EX`.
- `passthrough` lets a thread sleep inside the kernel where the scheduler cannot see it, and the doc comment records a four-way contention guest hanging under plain forwarding.
- `refuse` would break every lockfile user.

What remains before the code fully realizes `model`:

- **Contended blocking requests.** The comment at `detcore/src/syscalls/files.rs:1395` says a faithful wait needs a wait queue owned by the deterministic scheduler, the way futexes are handled. Until that exists, a contended blocking `LOCK_SH`/`LOCK_EX` gives `ENOLCK` under Hermit, while Linux would wait and then succeed. F1 and F2 are realized only for the uncontended case, and F11 cannot happen, because no thread ever waits in the kernel.
- **Locks on descriptors inherited across `fork`.** A successful process clone makes every inherited lock state unknown, in both parent and child (`detcore/src/syscalls/threads.rs:967`; unit test `fork_makes_inherited_flock_state_unknown_in_parent_and_child` at `detcore/src/tool_local.rs:1031`). From then on, any blocking `LOCK_SH`/`LOCK_EX` on such a descriptor is refused, even with no contention. By this reading of the code, the documented flock(1) pattern `exec 9>lockfile; flock 9` gets `ENOLCK`: the shell opens fd 9 and the `flock` child inherits it across fork. Natively it succeeds. This is a hypothesis from the code, not a measurement; T7 confirms or refutes it.

Target (owner decision 14): a blocking `LOCK_SH`/`LOCK_EX` is a non-blocking probe plus a wait in the deterministic scheduler. Detcore sends `operation | LOCK_NB` as it does today (`detcore/src/syscalls/files.rs:1329-1336`); on `EWOULDBLOCK` for a caller that did not pass `LOCK_NB`, it parks the thread on a scheduler-owned wait queue keyed by the file, wakes it when a guest releases or closes a lock on that file, and re-probes, so F1 and F2 wait and then succeed as on Linux and F11 (`EINTR`, or a restart under `SA_RESTART`) becomes reachable through the scheduler's signal delivery. Linux's blocking conversion also drops the old lock before it waits (`flock_lock_inode()` in `fs/locks.c` removes the existing lock first), so under the target the probe's drop is the Linux behaviour rather than collateral damage: the restore step (`detcore/src/syscalls/files.rs:1364-1393`) and the refusal of blocking requests on descriptions whose lock state Detcore does not know (`detcore/src/syscalls/files.rs:1310-1325`) are no longer needed, and the flock(1) pattern in T7 succeeds. `target_policy` stays `model`; this is a common call and is not refused on blocking grounds. Today's code uses the probe without the wait: a contended blocking request gets `ENOLCK` through `refuse_unserviceable_operation` (`detcore/src/syscalls/helpers.rs:77`), or stops the run in fail-closed mode (`detcore/src/syscalls/files.rs:1394-1406`). A holder outside the container never wakes the queue; that host-holder source stays outside Hermit's control, as the next paragraph says.

`linux_determinism` stays `nondeterministic`. Hermit removes the scheduling source by probing instead of waiting. The host-holder source remains, and Hermit accepts it as part of the outside filesystem state it does not control.

## Recommended tests

- T1 (covers F1, F2, F3, F5): on one open file description, under `hermit run --verify` on ptrace, run this sequence and check every return value: `LOCK_EX`, `LOCK_SH`, `LOCK_UN`, `LOCK_EX|LOCK_NB`, `LOCK_UN|LOCK_NB`.
  Existing candidate: `tests/c/flock_lifecycle.c` (cell `c-programs/flock-lifecycle`, `tests/e2e/manifests/c-programs.yaml:11885`, verify ci on ptrace). It does not call `LOCK_UN|LOCK_NB`. Its header comment, which says Hermit does not return `EWOULDBLOCK` on a contended request, is stale at this SHA.
- T2 (covers F4, F10): a held `LOCK_EX` must make `LOCK_EX|LOCK_NB` return `EWOULDBLOCK`, both on a second `open()` in the same process and in a second process. The lock must be free again after `LOCK_UN`. Also check that a `dup` copy shares the lock and that closing the last copy releases it.
  Existing candidate: `flock_excludes_a_second_open_and_a_second_process` at `hermit-cli/tests/flock_exclusion.rs:157`, which runs on ptrace (`hermit-cli/tests/flock_exclusion.rs:117`).
- T3 (covers F6): a contended `LOCK_NB` upgrade loses the shared lock, as on Linux. A contended blocking upgrade drops the shared lock, waits until the other holder releases, and then returns 0 holding `LOCK_EX` (target, owner decision 14). Hermit at f4055eb9cae4 refuses it with `ENOLCK` and restores the shared lock, which the existing candidates below assert; they change with the wait queue.
  Existing candidates: `contended_blocking_upgrade_is_refused_without_losing_the_shared_lock` at `hermit-cli/tests/flock_exclusion.rs:208` and `contended_blocking_upgrade_is_fail_closed_under_strict` at `hermit-cli/tests/flock_exclusion.rs:241`. The `LOCK_NB` drop case still needs a test.
- T4 (covers F7, F8): with a raw `syscall(SYS_flock, ...)`, check that operations `0`, `LOCK_SH|LOCK_EX` and `0x100` return `EINVAL`, both on a valid fd and on fd `-1`. `LOCK_MAND`, and `LOCK_MAND|LOCK_RW`, must return 0 on fd `-1` and take no lock: a second open's `LOCK_EX|LOCK_NB` still succeeds.
  Existing partial coverage: `flock(fd, 0)` returning `EINVAL` in `tests/c/flock_exclusion.c:337` and `tests/c/flock_exclusion.c:465`.
- T5 (covers F9): `EBADF` for a closed fd, for both blocking and `LOCK_NB` requests, including the path through the `F_GETFD` check when the lock state is unknown. `EBADF` for `LOCK_EX` on an `O_PATH` descriptor.
- T6 (covers F10, F11): locks on descriptors received through `SCM_RIGHTS` or `pidfd_getfd` keep Linux sharing semantics. A contended blocking request on them waits in the scheduler (target, owner decision 14); a signal with a handler interrupts the wait with `EINTR`, or restarts it under `SA_RESTART`, as on Linux. At f4055eb9cae4 such a request is refused with `ENOLCK` instead, so no `EINTR` window exists today.
  Existing candidates: `blocking_upgrade_on_received_fd_preserves_unknown_lock_state` at `hermit-cli/tests/flock_exclusion.rs:294`, `pidfd_getfd_alias_mutation_invalidates_source_flock_authority` at `hermit-cli/tests/flock_exclusion.rs:530`, and `transferred_lock_state_is_unknown_to_the_sender` at `hermit-cli/tests/flock_exclusion.rs:587`.
- T7 (covers F1, F2, F10): an uncontended blocking `LOCK_EX` on a descriptor the shell opened and the child inherited across `fork`, as in `bash -c 'exec 9>"$f"; flock 9; echo locked'`. The target is that `flock 9` succeeds and the shell prints `locked` (owner decision 14 removes the unknown-state refusal). Record what Hermit returns at f4055eb9cae4; if it is `ENOLCK`, as the code suggests, file it as a compatibility defect.
  Related existing test: `failed_process_clone_preserves_known_flock_state` at `hermit-cli/tests/flock_exclusion.rs:480`, which covers only the failed-clone case.
- T8 (covers F1, F2, F4): record and replay a guest that takes and releases `LOCK_SH`/`LOCK_EX`, including a `LOCK_NB` request that fails with `EWOULDBLOCK`. Replay must re-issue every lock operation. A contended blocking request must wait at record time and replay in the same order (target, owner decision 14); at f4055eb9cae4 it is refused at record time.
  Existing candidates: `replay_reissues_every_flock_for_a_materialized_file` at `hermit-cli/tests/flock_exclusion.rs:771`, `replay_refuses_flock_for_a_non_materialized_file` at `hermit-cli/tests/flock_exclusion.rs:914`, and `recording_refuses_a_contended_blocking_flock` at `hermit-cli/tests/flock_exclusion.rs:966`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/357: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · p3-samples · p3: target_policy model (the kernel holds the lock table; Detcore probes with LOCK_NB, caches lock modes per open file description, and refuses contended blocking requests until the scheduler has a lock wait queue); linux_determinism stays nondeterministic; 11 flag behaviours and 8 tests listed; no coverage token yet because no test has been run against this list, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 applied: target is the LOCK_NB probe plus a scheduler-owned wait queue, so contended blocking LOCK_SH/LOCK_EX waits and the restore step and unknown-state refusal (detcore/src/syscalls/files.rs:1310-1325, 1364-1393) go away; today the probe has no wait and gives ENOLCK; target_policy stays model; T3, T6, T7, T8 expectations moved to the target with today's result recorded; open questions on the wait queue and the fork invalidation removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `flock`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+flock&body=Discussion+for+the+%60flock%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fflock.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60flock%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: flock" (https://github.com/rrnewton/hermit/issues/357), legacy variant A2. It was ranked 78 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

### Open questions

- `tests/c/flock_lifecycle.c` has a stale header comment, which says Hermit does not return `EWOULDBLOCK`. The test code is still correct.
- The compat cell `flock` in `tests/e2e/manifests/compat.yaml` runs `flock -x "$f" -c ...`. In that cell the `flock` utility opens the file itself, so it does not exercise the inherited-descriptor case in T7.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=flock -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(73, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
flock(-1, LOCK_SH|LOCK_EX|LOCK_NB|LOCK_UN|LOCK_MAND|LOCK_RW|0xffffff10) = 0
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
