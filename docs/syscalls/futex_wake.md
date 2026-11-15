---
schema: syscall-pages/v1
syscall: futex_wake
nr_x86_64: 454
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/368
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex_wake&body=Discussion+for+the+%60futex_wake%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex_wake.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex_wake%60%2C+or+in+what+the+page+says%3F%0A
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

`futex_wake(uaddr, mask, nr, flags)` (Linux 6.7) wakes up to `nr` tasks waiting on a futex2 word whose wait bitset intersects `mask`, and returns how many it woke. Detcore answers every call with a fixed `ENOSYS` (`detcore/src/lib.rs:2100`), the result a kernel without futex2 gives. Callers that probe for it take their legacy `futex(FUTEX_WAKE)` path, which Detcore models.

Manual reference: Linux man-pages futex_waitv(2) / futex2.

## Linux semantics

The kernel validates `flags` and `mask`, computes the futex key for `uaddr`, locks its hash bucket and wakes matching waiters in queue order until `nr` have been woken. Waiters match when they are on the same key and their bitset intersects `mask`. futex2 shares hash buckets with legacy `futex(2)`, so `futex_wake` also wakes `FUTEX_WAIT`/`FUTEX_WAIT_BITSET` waiters on the same word, and `futex_waitv`/`futex_wait` waiters. The return value is the number of tasks woken, which can be 0.

Argument errors are deterministic: undefined `flags` bits, an unsupported size, a `mask` that does not fit the size, or a zero `mask` give `EINVAL`, and a misaligned `uaddr` gives `EINVAL`. The host strace under Additional information shows the all-ones probe returning `EINVAL`.

### Flags and options

- F1: size `FUTEX2_SIZE_U8`, `FUTEX2_SIZE_U16`, `FUTEX2_SIZE_U32` or `FUTEX2_SIZE_U64` (values 0 to 3, mask `FUTEX2_SIZE_MASK`). Only `FUTEX2_SIZE_U32` is accepted; the others give `EINVAL`. `uaddr` must be aligned to the size.
- F2: `FUTEX2_PRIVATE` (128, equal to `FUTEX_PRIVATE_FLAG`): key the word by the process's address space, so only waiters of the same process match. Without it the key follows the underlying mapping and can wake waiters in other processes sharing it.
- F3: `FUTEX2_NUMA` (0x04) and `FUTEX2_MPOL` (0x08), on kernels that implement NUMA-aware futexes: the futex is two words, the second a node id that selects the hash bucket. Any undefined `flags` bit gives `EINVAL`.
- F4: `mask`: only waiters whose bitset intersects it are woken. A zero `mask`, or one that does not fit the chosen size, gives `EINVAL`. `FUTEX_BITSET_MATCH_ANY` (all ones) wakes every matching waiter.
- F5: `nr`: the maximum number of tasks to wake. The return value is the number actually woken, from 0 up to `nr`, and depends on how many tasks were waiting. `nr` of 0 wakes nothing.
- F6: wakes cross the old and new interfaces: a `futex_wake` wakes legacy `futex(FUTEX_WAIT)` waiters on the same word, and `futex_waitv` waiters, which then return the index of that word.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Argument validation is repeatable. The return value is not: it counts the tasks that happened to be queued on the word when the bucket was locked, which depends on how far the waiting threads had progressed under host scheduling. Which waiters are woken, and in what order they then run, also depends on queue order and the host scheduler. The call repeats only when the set of waiters is fixed before it runs, for example when no other thread can be entering or leaving a wait on the word.

### Nondeterminism sources

- Wait-queue membership and order at the moment the bucket is locked
- Concurrent wait, requeue and wake operations on the same word, from either futex interface
- Scheduler interleaving of the woken tasks

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built before 6.7 (no `futex_wake` in the futex2 interface), returned before any argument is examined, so a valid wake and a garbage-argument call get the same `ENOSYS`. The real call has no outcome that a refusal must preserve as an unprivileged success, so decision 6's exceptions do not apply. The supportable fallback is legacy `futex(2)` `FUTEX_WAKE`/`FUTEX_WAKE_BITSET`, which Detcore models (`detcore/src/syscalls/threads.rs:1277`). Today's code already realizes the target, so no code change is needed:

- `is_futex2_enosys_syscall` (`detcore/src/syscall_classification.rs:882`) names `futex_requeue`, `futex_wait`, `futex_waitv` and `futex_wake`. All four are classified `Determinized` (`detcore/src/syscall_classification.rs:101` to `detcore/src/syscall_classification.rs:107`), and the predicate feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1322`).
- The dispatch arm at `detcore/src/lib.rs:2100` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It inspects no argument, reads no guest memory and never forwards the call to the kernel. The arm has no strict-mode condition, so run, `--strict` and record/replay runs all get the same answer.
- The doc comment at `detcore/src/syscall_classification.rs:877` gives the reasoning: Detcore models the established `futex(2)` ABI but not futex2. A fixed `ENOSYS` matches a kernel without the interface, sends feature-probing runtimes to their legacy-futex fallback and does not reveal the host kernel version.

Why `refuse` and not the alternatives:

- `passthrough` would be wrong. Detcore emulates legacy futexes and never runs them in the kernel (`detcore/src/syscalls/threads.rs:1255`): a thread blocked in `FUTEX_WAIT` is parked in Detcore's scheduler, not in a kernel hash bucket. A forwarded `futex_wake` would find no kernel waiters, return 0 and wake nobody, so the cross-interface wake in F6 would silently fail and the waiter would stay parked. Its return value would also come from the host.
- `model` is the closest of the four futex2 calls to existing code. The precise futex model already implements `FUTEX_WAKE_BITSET` (`detcore/src/syscalls/threads.rs:1277`), including the `EINVAL` for a zero bitset (`detcore/src/syscalls/threads.rs:1272`). Modeling `futex_wake` would mean mapping its flags onto that path. It is not worth doing alone, because a futex2 waker is useful only alongside futex2 waiters (`futex_wait`, `futex_waitv`), and a program that probes futex2 usually checks the whole family. If the family is ever modeled, `futex_wake` should be the first.
- Returning success with 0 woken would look valid but would drop wakes. `ENOSYS` is what a kernel before 6.7 returns, and callers already handle it.

`linux_determinism` stays `nondeterministic`: that describes Linux. Under Hermit the result is a constant.

## Recommended tests

- T1 (covers F1, F5): with `syscall(SYS_futex_wake, NULL, 0, 0, 0)` under `hermit run --strict --verify` on ptrace, the call returns -1 with `errno == ENOSYS`.
  Existing candidates: `tests/c/futex_wake_enosys.c`, used by cell `c-programs/futex-wake-enosys` (`tests/e2e/manifests/c-programs.yaml:1660`, verify ci on ptrace), and the second case of `futex2_feature_probes_receive_deterministic_enosys` (`hermit-cli/tests/futex2_refusal.rs:31`), which runs on ptrace only. Both cover only this zero-argument probe.
- T2 (covers F1, F2, F3, F4): on a readable aligned `u32` word, `FUTEX2_SIZE_U32` with `mask` all ones, with and without `FUTEX2_PRIVATE`, plus `FUTEX2_SIZE_U64`, `FUTEX2_NUMA`, `FUTEX2_MPOL` and `mask` 0, all return `ENOSYS`. That shows Hermit does not reveal which of these the host kernel accepts.
- T3 (covers F5, F6): a second thread blocks in legacy `futex(FUTEX_WAIT)` on a word. `futex_wake(word, FUTEX_BITSET_MATCH_ANY, 1, FUTEX2_SIZE_U32|FUTEX2_PRIVATE)` returns `ENOSYS` and leaves the waiter parked, and a following legacy `FUTEX_WAKE` returns 1 and releases it. The run must pass `--verify`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/368: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_futex2_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for the whole futex2 family at detcore/src/lib.rs:2100, which matches a pre-6.7 kernel; a forwarded wake could not reach Detcore-parked legacy waiters); linux_determinism stays nondeterministic; 6 flag behaviours and 3 tests listed; existing tests cover only the zero-argument probe and no coverage token is recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS before any argument check, already realized at detcore/src/lib.rs:2100); target_policy stays refuse; no new F/T ids; no questions removed (the remaining one is about stale inventory text)
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/futex_wake_enosys.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `futex_wake`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex_wake&body=Discussion+for+the+%60futex_wake%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex_wake.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex_wake%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: futex_wake" (https://github.com/rrnewton/hermit/issues/368), legacy variant A2. It was ranked 89 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

The futex2 refusal set came from the change tagged `TODO-HUMAN-REVIEW(PR-852)` in the code (`detcore/src/lib.rs:2096`, `detcore/src/syscall_classification.rs:876`); that human review is still open in the source. Sibling bodies: futex_waitv, futex_wait and futex_requeue share the same predicate and dispatch arm.

### Open questions

- `tests/e2e/manifests/inventory/test-files.json:635` says this guest runs only on explicit selection with `ci=false`. The manifest cell sets `ptrace: true` under verify ci (`tests/e2e/manifests/c-programs.yaml:1679`), so the inventory text is stale.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=futex_wake -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(454, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
futex_wake(0xffffffffffffffff, 0xffffffffffffffff, -1, FUTEX2_SIZE_U64|FUTEX2_NUMA|FUTEX2_PRIVATE|0xffffff78) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
