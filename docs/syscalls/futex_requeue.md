---
schema: syscall-pages/v1
syscall: futex_requeue
nr_x86_64: 456
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/365
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex_requeue&body=Discussion+for+the+%60futex_requeue%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex_requeue.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex_requeue%60%2C+or+in+what+the+page+says%3F%0A
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

`futex_requeue(waiters, flags, nr_wake, nr_requeue)` (Linux 6.7) checks that the first of two futex2 words still holds an expected value, wakes up to `nr_wake` of its waiters and moves up to `nr_requeue` more onto the second word's queue. Detcore answers every call with a fixed `ENOSYS` (`detcore/src/lib.rs:2100`), the result a kernel without futex2 gives. Detcore does not model requeue for legacy futexes either.

Manual reference: Linux man-pages futex_waitv(2) / futex2.

## Linux semantics

`waiters` points to exactly two `struct futex_waitv { u64 val; u64 uaddr; u32 flags; u32 __reserved; }` entries: the source word and the destination word. Each entry's `flags` and `__reserved` are validated as for `futex_waitv`, and the two entries may differ, for example in `FUTEX2_PRIVATE`. The kernel locks both hash buckets and compares the source word with `waiters[0].val`. On a mismatch the call returns `EAGAIN` and does nothing. Otherwise it wakes up to `nr_wake` matching waiters on the source word and moves up to `nr_requeue` further waiters onto the destination word without waking them. It returns the number woken plus the number moved. This is the futex2 form of legacy `FUTEX_CMP_REQUEUE`; waiters queued through either interface are affected, because the interfaces share hash buckets.

Argument errors are deterministic: a nonzero syscall `flags`, a NULL `waiters`, a bad entry, or a negative `nr_wake` or `nr_requeue` give `EINVAL`. An unreadable array or word gives `EFAULT`. The host strace under Additional information shows the all-ones probe returning `EINVAL`.

### Flags and options

- F1: the syscall-level `flags` argument: no flag is defined, so any nonzero value gives `EINVAL`. A NULL `waiters` gives `EINVAL`, and an unreadable array gives `EFAULT`.
- F2: per-entry size `FUTEX2_SIZE_U8`, `FUTEX2_SIZE_U16`, `FUTEX2_SIZE_U32` or `FUTEX2_SIZE_U64` (values 0 to 3, mask `FUTEX2_SIZE_MASK`). Only `FUTEX2_SIZE_U32` is accepted; the others give `EINVAL`. `val` must fit in the size and `uaddr` must be aligned to it.
- F3: per-entry `FUTEX2_PRIVATE` (128, equal to `FUTEX_PRIVATE_FLAG`): key that word by the process's address space instead of by the underlying mapping. The source and destination entries choose this independently.
- F4: per-entry `FUTEX2_NUMA` (0x04) and `FUTEX2_MPOL` (0x08), on kernels that implement NUMA-aware futexes: that futex is two words, the second a node id. Any undefined entry flag bit, or a nonzero `__reserved`, gives `EINVAL`.
- F5: value check: if the source word does not hold `waiters[0].val`, the call returns `EAGAIN` and wakes or moves nothing.
- F6: `nr_wake`: wake up to this many waiters on the source word. A negative value gives `EINVAL`.
- F7: `nr_requeue`: move up to this many further waiters to the destination word, where a later wake on that word releases them. A negative value gives `EINVAL`. The return value is woken plus moved, and depends on how many waiters were queued.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Argument validation is repeatable. The rest is not: which waiters are on the source queue when the buckets are locked, and therefore which are woken, which are moved and the returned count, depend on how far the waiting threads had progressed under host scheduling. The value check races with other writers to the source word. The call repeats only when the waiter set is fixed before it runs and no other thread writes the source word.

### Nondeterminism sources

- Waiter set and queue order at the moment the buckets are locked
- Concurrent writes to the source word, and concurrent waits and wakes on either word
- Scheduler interleaving of the woken tasks

## Hermit design

Target (owner decision 9, 6, 10): `refuse` with a fixed `ENOSYS`, the answer of a kernel built before 6.7 (no `futex_requeue` in the futex2 interface), returned before any argument is examined, so a valid requeue and a garbage-argument call get the same `ENOSYS`. The real call has no outcome that a refusal must preserve as an unprivileged success, so decision 6's exceptions do not apply. The supportable fallback is legacy `futex(2)` waits and wakes, which Detcore models. Owner decision 10 settles the legacy requeue operations separately, in the `futex` body: Detcore should first answer `FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE` and `FUTEX_WAKE_OP` with `ENOSYS` instead of reaching the panic at `detcore/src/syscalls/threads.rs:1371`, and then model them. That later model of legacy requeue does not change this body's target: `futex_requeue` stays refused. Today's code already realizes this body's target, so no code change is needed here:

- `is_futex2_enosys_syscall` (`detcore/src/syscall_classification.rs:882`) names `futex_requeue`, `futex_wait`, `futex_waitv` and `futex_wake`. All four are classified `Determinized` (`detcore/src/syscall_classification.rs:101` to `detcore/src/syscall_classification.rs:107`), and the predicate feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1322`).
- The dispatch arm at `detcore/src/lib.rs:2100` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It inspects no argument, reads no guest memory and never forwards the call to the kernel. The arm has no strict-mode condition, so run, `--strict` and record/replay runs all get the same answer.
- The doc comment at `detcore/src/syscall_classification.rs:877` gives the reasoning: Detcore models the established `futex(2)` ABI but not futex2's vector waits, sized values or requeue rules. A fixed `ENOSYS` matches a kernel without the interface, sends feature-probing runtimes to their legacy-futex fallback and does not reveal the host kernel version.

Why `refuse` and not the alternatives:

- `passthrough` would act on kernel queues that Detcore does not use. Detcore emulates legacy futexes and never runs them in the kernel (`detcore/src/syscalls/threads.rs:1255`), so threads it has parked are in its scheduler, not in a kernel hash bucket. A forwarded requeue would find no waiters to wake or move, and its return value would come from the host.
- `model` has no foundation yet. The precise futex model handles only `FUTEX_WAKE`/`FUTEX_WAKE_BITSET` (`detcore/src/syscalls/threads.rs:1277`) and `FUTEX_WAIT`/`FUTEX_WAIT_BITSET` (`detcore/src/syscalls/threads.rs:1315`). Any other legacy operation, `FUTEX_REQUEUE` and `FUTEX_CMP_REQUEUE` included, reaches `panic!("[detcore] futex op not handled yet: {}", other)` (`detcore/src/syscalls/threads.rs:1371`). Modeling `futex_requeue` would first need a scheduler operation that moves parked waiters from one futex key to another, which would serve legacy `FUTEX_CMP_REQUEUE` too.
- `ENOSYS` is what a kernel before 6.7 returns, and callers already handle it.

`linux_determinism` stays `nondeterministic`: that describes Linux. Under Hermit the result is a constant.

## Recommended tests

- T1 (covers F1, F6, F7): with `syscall(SYS_futex_requeue, NULL, 0, 0, 0)` under `hermit run --strict --verify` on ptrace, the call returns -1 with `errno == ENOSYS`.
  Existing candidates: `tests/c/futex_requeue_enosys.c`, used by cell `c-programs/futex-requeue-enosys` (`tests/e2e/manifests/c-programs.yaml:1527`, verify ci on ptrace), and the third case of `futex2_feature_probes_receive_deterministic_enosys` (`hermit-cli/tests/futex2_refusal.rs:31`), which runs on ptrace only. Both cover only this zero-argument probe; neither passes a nonzero or negative `nr_wake`/`nr_requeue`.
- T2 (covers F2, F3, F4): a well-formed two-entry array of readable aligned `u32` words (`FUTEX2_SIZE_U32`, private source and shared destination), and arrays using `FUTEX2_SIZE_U64`, `FUTEX2_NUMA`, `FUTEX2_MPOL` and a nonzero `__reserved`, all return `ENOSYS`. That shows Hermit does not reveal which of these the host kernel accepts.
- T3 (covers F5, F6, F7): two threads block in legacy `futex(FUTEX_WAIT)` on the source word. `futex_requeue` with `nr_wake` 1, `nr_requeue` 1 and a matching `val` returns `ENOSYS` and leaves both waiters parked on the source word; the same call with a non-matching `val` also returns `ENOSYS`, not `EAGAIN`; and negative counts return `ENOSYS`, not `EINVAL`. A following legacy `FUTEX_WAKE` of 2 on the source word returns 2. The run must pass `--verify`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/365: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_futex2_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for the whole futex2 family at detcore/src/lib.rs:2100, matching a pre-6.7 kernel; Detcore has no requeue primitive even for legacy futex, threads.rs:1371); linux_determinism stays nondeterministic; 7 flag behaviours and 3 tests listed; existing tests cover only the zero-argument probe and no coverage token is recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6 and 10 applied (fixed ENOSYS before any argument check, already realized at detcore/src/lib.rs:2100; legacy FUTEX_REQUEUE/FUTEX_CMP_REQUEUE go to ENOSYS then a model in the futex body, futex_requeue stays refused); target_policy stays refuse; no new F/T ids; removed the open question about the legacy requeue panic at detcore/src/syscalls/threads.rs:1371
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/futex_requeue_enosys.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `futex_requeue`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futex_requeue&body=Discussion+for+the+%60futex_requeue%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutex_requeue.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futex_requeue%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: futex_requeue" (https://github.com/rrnewton/hermit/issues/365), legacy variant A2. It was ranked 86 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

The futex2 refusal set came from the change tagged `TODO-HUMAN-REVIEW(PR-852)` in the code (`detcore/src/lib.rs:2096`, `detcore/src/syscall_classification.rs:876`); that human review is still open in the source. Sibling bodies: futex_waitv, futex_wake and futex_wait share the same predicate and dispatch arm.

### Open questions

- `tests/e2e/manifests/inventory/test-files.json:623` says this guest runs only on explicit selection with `ci=false`. The manifest cell sets `ptrace: true` under verify ci (`tests/e2e/manifests/c-programs.yaml:1546`), so the inventory text is stale.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=futex_requeue -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(456, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
futex_requeue(0xffffffffffffffff, 0xffffffff, -1, -1) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
