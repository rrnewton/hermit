---
schema: syscall-pages/v1
syscall: futex_wait
nr_x86_64: 455
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/366
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex_wait&body=Discussion+for+the+%60futex_wait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex_wait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex_wait%60%2C+or+in+what+the+page+says%3F%0A
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

`futex_wait(uaddr, val, mask, flags, timeout, clockid)` (Linux 6.7) sleeps on a futex2 word while it still holds `val`, until a matching wake, an absolute timeout or a signal. Detcore answers every call with a fixed `ENOSYS` (`detcore/src/lib.rs:2100`), the result a kernel without futex2 gives. Callers that probe for it take their legacy `futex(FUTEX_WAIT_BITSET)` path, which Detcore models. No test in the tree names this syscall.

Manual reference: Linux man-pages futex_waitv(2) / futex2.

## Linux semantics

The kernel validates `flags`, `val`, `mask` and the timeout, then locks the word's hash bucket and reads the word. If it differs from `val`, the call returns `EAGAIN` without sleeping. Otherwise the task is queued with bitset `mask` and sleeps until a wake whose mask intersects it (from `futex_wake`, `futex_requeue`, or a legacy `FUTEX_WAKE`/`FUTEX_WAKE_BITSET`, since the interfaces share hash buckets), until the deadline, or until a signal. A woken call returns 0.

Argument errors are deterministic: undefined `flags` bits, an unsupported size, a `val` or `mask` that does not fit the size, a zero `mask`, a misaligned `uaddr` or a bad clock give `EINVAL`. An unreadable word or timeout gives `EFAULT`. The host strace under Additional information shows the all-ones probe returning `EINVAL`.

### Flags and options

- F1: size `FUTEX2_SIZE_U8`, `FUTEX2_SIZE_U16`, `FUTEX2_SIZE_U32` or `FUTEX2_SIZE_U64` (values 0 to 3, mask `FUTEX2_SIZE_MASK`). Only `FUTEX2_SIZE_U32` is accepted; the others give `EINVAL`. `val` must fit in the size and `uaddr` must be aligned to it, otherwise `EINVAL`.
- F2: `FUTEX2_PRIVATE` (128, equal to `FUTEX_PRIVATE_FLAG`): key the word by the process's address space. Without it the key follows the underlying mapping, so a waker in another process sharing the mapping can wake the caller.
- F3: `FUTEX2_NUMA` (0x04) and `FUTEX2_MPOL` (0x08), on kernels that implement NUMA-aware futexes: the futex is two words, the second a node id that selects the hash bucket. Any undefined `flags` bit gives `EINVAL`.
- F4: `mask`: the waiter's bitset. Only wakes whose mask intersects it end the wait. A zero `mask`, or one that does not fit the size, gives `EINVAL`.
- F5: value check: if the word does not hold `val` when the bucket is locked, the call returns `EAGAIN` and does not sleep. An unreadable word gives `EFAULT`.
- F6: wake: a matching `futex_wake`, `futex_requeue` or legacy `FUTEX_WAKE`/`FUTEX_WAKE_BITSET` ends the wait and the call returns 0.
- F7: `timeout` NULL: wait indefinitely, and `clockid` is not examined. `timeout` non-NULL: an absolute deadline on `clockid`, which must be `CLOCK_MONOTONIC` or `CLOCK_REALTIME`, otherwise `EINVAL`. A malformed timespec gives `EINVAL`, and expiry gives `ETIMEDOUT`.
- F8: a signal during the sleep gives `EINTR`, or the call restarts when the handler uses `SA_RESTART`. The deadline is absolute, so a restart does not extend it.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Argument validation is repeatable. Whether the value check returns `EAGAIN` depends on races with other writers to the word. How long the call sleeps, and whether it ends with a wake, `ETIMEDOUT` or `EINTR`, depends on host scheduling of the waking thread, on host time and on signal timing. The call repeats only when no other thread can write or wake the word and no timeout or signal can fire.

### Nondeterminism sources

- Concurrent writes to the futex word between the caller's read and the kernel's value check
- Wake and signal ordering
- Timeout progression on the host clock, and scheduler state

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built before 6.7 (no `futex_wait` in the futex2 interface), returned before any argument is examined, so a valid wait and a garbage-argument call get the same `ENOSYS`. The real call has no outcome that a refusal must preserve as an unprivileged success, so decision 6's exceptions do not apply. The supportable fallback is legacy `futex(2)` `FUTEX_WAIT`/`FUTEX_WAIT_BITSET`, which Detcore models (`detcore/src/syscalls/threads.rs:1315`). Today's code already realizes the target, so no code change is needed:

- `is_futex2_enosys_syscall` (`detcore/src/syscall_classification.rs:882`) names `futex_requeue`, `futex_wait`, `futex_waitv` and `futex_wake`. All four are classified `Determinized` (`detcore/src/syscall_classification.rs:101` to `detcore/src/syscall_classification.rs:107`), and the predicate feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1322`).
- The dispatch arm at `detcore/src/lib.rs:2100` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It inspects no argument, reads no guest memory and never forwards the call to the kernel. The arm has no strict-mode condition, so run, `--strict` and record/replay runs all get the same answer.
- The doc comment at `detcore/src/syscall_classification.rs:877` gives the reasoning: Detcore models the established `futex(2)` ABI but not futex2. A fixed `ENOSYS` matches a kernel without the interface, sends feature-probing runtimes to their legacy-futex fallback, does not reveal the host kernel version, and keeps every wait inside Detcore's scheduler.

Why `refuse` and not the alternatives:

- `passthrough` would break the scheduler. Detcore emulates legacy futexes and never runs them in the kernel (`detcore/src/syscalls/threads.rs:1255`). A forwarded `futex_wait` would sleep in the kernel where the sequentializing scheduler cannot see it, and a Detcore-emulated legacy `FUTEX_WAKE` would never reach it. Under sequentialization the guest would hang (reasoned from the code, not measured).
- `model` is feasible later. `futex_wait` is legacy `FUTEX_WAIT_BITSET` with a different argument layout and a selectable clock, and the precise futex model already implements `FUTEX_WAIT_BITSET` (`detcore/src/syscalls/threads.rs:1315`). It is not worth modeling alone: a program that uses futex2 waits usually also needs `futex_wake`, and probes the family as a whole.
- `ENOSYS` is what a kernel before 6.7 returns, and callers already handle it. An `emulate` answer such as an immediate `EAGAIN` would turn a caller's wait loop into a busy loop.

`linux_determinism` stays `nondeterministic`: that describes Linux. Under Hermit the result is a constant.

## Recommended tests

- T1 (covers F1, F2, F3, F4): under `hermit run --strict --verify` on ptrace, `futex_wait` on a readable aligned `u32` word holding `val`, with `FUTEX2_SIZE_U32` and `mask` all ones, with and without `FUTEX2_PRIVATE`, plus `FUTEX2_SIZE_U64`, `FUTEX2_NUMA`, `FUTEX2_MPOL`, `mask` 0 and a NULL `uaddr`, all return -1 with `errno == ENOSYS`. No such test exists at this SHA: `hermit-cli/tests/futex2_refusal.rs:16` lists `futex_waitv`, `futex_wake` and `futex_requeue` but not `futex_wait`, and there is no `tests/c/futex_wait_enosys.c`.
- T2 (covers F5, F6): with a second thread ready to issue a legacy `FUTEX_WAKE`, `futex_wait` on a word whose value matches returns `ENOSYS` at once instead of sleeping; on a word whose value differs it returns `ENOSYS`, not `EAGAIN`.
- T3 (covers F7, F8): with `timeout` set to an absolute `CLOCK_MONOTONIC` deadline 10 ms ahead, with an invalid `clockid`, and with a `SIGALRM` handler armed, the call returns `ENOSYS` immediately and never `ETIMEDOUT` or `EINTR`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/366: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_futex2_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for the whole futex2 family at detcore/src/lib.rs:2100, which matches a pre-6.7 kernel and keeps every wait in Detcore's scheduler); linux_determinism stays nondeterministic; 8 flag behaviours and 3 tests listed; no test in the tree exercises futex_wait at all, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS before any argument check, already realized at detcore/src/lib.rs:2100); target_policy stays refuse; no new F/T ids; no open questions to remove

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `futex_wait`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex_wait&body=Discussion+for+the+%60futex_wait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex_wait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex_wait%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: futex_wait" (https://github.com/rrnewton/hermit/issues/366), legacy variant A2. It was ranked 87 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

The futex2 refusal set came from the change tagged `TODO-HUMAN-REVIEW(PR-852)` in the code (`detcore/src/lib.rs:2096`, `detcore/src/syscall_classification.rs:876`); that human review is still open in the source. Sibling bodies: futex_waitv, futex_wake and futex_requeue share the same predicate and dispatch arm. Of the four, only `futex_wait` has no guest test; the unit test `futex2_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2251`) checks its classification and predicate membership but does not run a guest.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=futex_wait -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(455, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
futex_wait(0xffffffffffffffff, 18446744073709551615, 0xffffffffffffffff, FUTEX2_SIZE_U64|FUTEX2_NUMA|FUTEX2_PRIVATE|0xffffff78, 0xffffffffffffffff, 0xffffffff /* CLOCK_??? */) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
