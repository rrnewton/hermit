---
schema: syscall-pages/v1
syscall: futex_waitv
nr_x86_64: 449
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/367
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex_waitv&body=Discussion+for+the+%60futex_waitv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex_waitv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex_waitv%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_futex2_enosys_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`futex_waitv(waiters, nr_futexes, flags, timeout, clockid)` (Linux 5.16) sleeps on up to 128 futex words at once and returns the index of the one that was woken. Detcore answers every call with a fixed `ENOSYS` (`detcore/src/lib.rs:2100`), the result a kernel older than 5.16 gives. Callers that probe for it take their legacy `futex(2)` path, which Detcore models.

Manual reference: Linux man-pages futex_waitv(2).

## Linux semantics

The caller passes an array of `struct futex_waitv { u64 val; u64 uaddr; u32 flags; u32 __reserved; }` (`FUTEX_WAITV_MAX` is 128). The kernel checks the arguments, then queues the task on every word's hash bucket. If any word no longer holds its expected `val`, the call returns `EAGAIN` without sleeping. Otherwise the task sleeps until a wake on any of the words, the timeout or a signal. A successful return is the array index of a woken entry. futex2 waits share hash buckets with legacy `futex(2)`, so a `FUTEX_WAKE` from a thread using the old interface wakes a `futex_waitv` waiter on the same word.

Argument errors are deterministic. `EINVAL` covers a nonzero syscall `flags`, `nr_futexes` of 0 or above 128, a NULL `waiters`, a bad clock, a bad entry `flags` or a nonzero `__reserved`. `EFAULT` covers an unreadable array or timeout. The host strace under Additional information shows the all-ones probe returning `EINVAL`.

### Flags and options

- F1: the waiter vector: `nr_futexes` from 1 to 128 entries. `nr_futexes` of 0 or above 128, or a NULL `waiters`, gives `EINVAL`. An unreadable array gives `EFAULT`.
- F2: the syscall-level `flags` argument: no flag is defined, so any nonzero value gives `EINVAL`.
- F3: per-entry size `FUTEX2_SIZE_U8`, `FUTEX2_SIZE_U16`, `FUTEX2_SIZE_U32` or `FUTEX2_SIZE_U64` (values 0 to 3, mask `FUTEX2_SIZE_MASK`). Only `FUTEX2_SIZE_U32` is accepted; the others give `EINVAL`. `val` must fit in the chosen size and `uaddr` must be aligned to it, otherwise `EINVAL`.
- F4: per-entry `FUTEX2_PRIVATE` (128, equal to `FUTEX_PRIVATE_FLAG`): key the word by the process's address space instead of by the underlying mapping, so only threads of the same process match it.
- F5: per-entry `FUTEX2_NUMA` (0x04) and `FUTEX2_MPOL` (0x08), on kernels that implement NUMA-aware futexes: the futex becomes two words, the second holding a node id (`FUTEX_NO_NODE` is -1). Any undefined entry flag bit, or a nonzero `__reserved`, gives `EINVAL`.
- F6: value check at queue time: if any word differs from its `val`, the call returns `EAGAIN` and does not sleep.
- F7: wake: a `futex_wake`, `futex_requeue` or legacy `FUTEX_WAKE` on any listed word ends the wait and the call returns that entry's index. If several are woken, the returned index is one of them.
- F8: `timeout` NULL: wait indefinitely, and `clockid` is not examined. `timeout` non-NULL: an absolute deadline on `clockid`, which must be `CLOCK_MONOTONIC` or `CLOCK_REALTIME`, otherwise `EINVAL`. A malformed timespec gives `EINVAL`, and expiry gives `ETIMEDOUT`.
- F9: a signal during the sleep gives `EINTR`, or the call restarts when the handler uses `SA_RESTART`. The deadline is absolute, so a restart does not extend it.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Argument validation (F1 to F5) and the `EAGAIN` check against a value the caller just read are repeatable. The rest is not: which word is woken first, and therefore the returned index, depends on how the host schedules the waking threads. Whether the value check sees a stale value depends on races with other writers. Whether the deadline or a signal ends the wait depends on host time and signal timing. The call repeats only when no other thread can touch the words and no timeout or signal can fire.

### Nondeterminism sources

- Competing futex wakeups, and which of several woken entries the kernel reports
- Concurrent writes to the futex words between the caller's read and the kernel's value check
- Scheduler and signal ordering
- Timeout progression on the host's `CLOCK_MONOTONIC` or `CLOCK_REALTIME`

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built before 5.16 (no futex2 interface), returned before any argument is examined, so the valid one-entry wait and the all-ones garbage probe get the same `ENOSYS`. Linux has no unprivileged success case to preserve here: every outcome of the real call is either an argument error or a wait, so decision 6's exceptions do not apply. The supportable fallback is the caller's legacy `futex(2)` path, which Detcore models (`FUTEX_WAIT`/`FUTEX_WAKE` and their `_BITSET` forms, `detcore/src/syscalls/threads.rs:1277` and `detcore/src/syscalls/threads.rs:1315`); Wine/Proton's fsync mode probes for `futex_waitv` and, on `ENOSYS`, uses its other synchronization paths (owner decision 9; the Proton source was not re-read for this body). Today's code already realizes the target, so no code change is needed:

- `is_futex2_enosys_syscall` (`detcore/src/syscall_classification.rs:882`) names `futex_requeue`, `futex_wait`, `futex_waitv` and `futex_wake`. All four are classified `Determinized` (`detcore/src/syscall_classification.rs:101` to `detcore/src/syscall_classification.rs:107`), and the predicate feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1322`).
- The dispatch arm at `detcore/src/lib.rs:2100` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It inspects no argument, reads no guest memory and never forwards the call to the kernel. The arm has no strict-mode condition, so run, `--strict` and record/replay runs all get the same answer.
- The doc comment at `detcore/src/syscall_classification.rs:877` gives the reasoning: Detcore models the established `futex(2)` ABI but not futex2's vector waits, sized values or requeue rules. A fixed `ENOSYS` matches a kernel without the interface, sends feature-probing runtimes to their legacy-futex fallback, does not reveal the host kernel version, and keeps every wait inside Detcore's scheduler.

Why `refuse` and not the alternatives:

- `passthrough` would break the scheduler. Detcore emulates legacy futexes and never runs them in the kernel (`detcore/src/syscalls/threads.rs:1255`): a thread blocked in `FUTEX_WAIT` is parked in Detcore's scheduler, not in a kernel hash bucket. A forwarded `futex_waitv` would sleep in the kernel where the sequentializing scheduler cannot see it, and no Detcore-emulated `FUTEX_WAKE` would ever reach it. Under sequentialization the guest would hang (reasoned from the code, not measured).
- `model` is possible later but is not small. The precise futex model (`detcore/src/syscalls/threads.rs:1256`) handles one word per waiter, `FUTEX_WAIT`/`FUTEX_WAIT_BITSET` (`detcore/src/syscalls/threads.rs:1315`) and `FUTEX_WAKE`/`FUTEX_WAKE_BITSET` (`detcore/src/syscalls/threads.rs:1277`). It would need a wait on up to 128 keys at once, with a deterministic choice of which index to return when several are woken.
- `emulate` with anything other than `ENOSYS`, for example an immediate `EAGAIN`, would put a caller that expects futex2 into a busy loop. `ENOSYS` is what a real kernel before 5.16 returns, and callers already handle it.

What the refusal costs: a program that requires `futex_waitv`, with no fallback, cannot run under Hermit. No such workload is recorded in this tree, and owner decision 9 makes `refuse` the long-term target.

`linux_determinism` stays `nondeterministic`: that describes Linux. Under Hermit the result is a constant.

## Recommended tests

- T1 (covers F1, F2): with `syscall(SYS_futex_waitv, NULL, 0, 0, NULL, 0)` under `hermit run --strict --verify` on ptrace, the call returns -1 with `errno == ENOSYS`.
  Existing candidates: `tests/c/futex_waitv_enosys.c`, used by cell `c-programs/futex-waitv-enosys` (`tests/e2e/manifests/c-programs.yaml:1595`, verify ci on ptrace), and the first case of `futex2_feature_probes_receive_deterministic_enosys` (`hermit-cli/tests/futex2_refusal.rs:31`), which runs on ptrace only. Both cover only this zero-argument probe.
- T2 (covers F3, F4, F5): a well-formed one-entry vector (a readable `u32` word, `FUTEX2_SIZE_U32`, with and without `FUTEX2_PRIVATE`), and entries using `FUTEX2_SIZE_U64`, `FUTEX2_NUMA`, `FUTEX2_MPOL` and a nonzero `__reserved`, all return `ENOSYS`. That shows Hermit does not reveal which of these the host kernel accepts.
- T3 (covers F6, F7): with a second thread, call `futex_waitv` on a word whose value matches and on one whose value differs. Both return `ENOSYS` at once: no sleep and no `EAGAIN`. A legacy `FUTEX_WAKE` on the word afterwards returns 0.
- T4 (covers F8, F9): with `timeout` set to an absolute `CLOCK_MONOTONIC` deadline 10 ms ahead, with an invalid `clockid`, and with a `SIGALRM` handler armed, the call returns `ENOSYS` immediately. It never returns `ETIMEDOUT` or `EINTR`, and logical time advances by no more than one syscall's cost.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/367: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_futex2_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for the whole futex2 family at detcore/src/lib.rs:2100, which matches a pre-5.16 kernel and keeps every wait in Detcore's scheduler); linux_determinism stays nondeterministic; 9 flag behaviours and 4 tests listed; existing tests cover only the zero-argument probe and no coverage token is recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS before any argument check, already realized at detcore/src/lib.rs:2100); target_policy stays refuse; no new F/T ids; removed the open question on whether a permanent ENOSYS is acceptable for Wine/Proton
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/futex_waitv_enosys.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `futex_waitv`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex_waitv&body=Discussion+for+the+%60futex_waitv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex_waitv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex_waitv%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: futex_waitv" (https://github.com/rrnewton/hermit/issues/367), legacy variant A2. It was ranked 88 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

The futex2 refusal set came from the change tagged `TODO-HUMAN-REVIEW(PR-852)` in the code (`detcore/src/lib.rs:2096`, `detcore/src/syscall_classification.rs:876`); that human review is still open in the source. Sibling bodies: futex_wake, futex_wait and futex_requeue share the same predicate and dispatch arm.

### Open questions

- `tests/e2e/manifests/inventory/test-files.json:629` describes this guest as running in blocking CI on "ptrace and DBT". The manifest cell sets `dbt: false` under verify ci (`tests/e2e/manifests/c-programs.yaml:1615`), so the inventory text is stale for DBT.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=futex_waitv -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(449, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
futex_waitv(0xffffffffffffffff, 4294967295, 0xffffffff, 0xffffffffffffffff, 0xffffffff /* CLOCK_??? */) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
