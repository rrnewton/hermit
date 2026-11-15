---
schema: syscall-pages/v1
syscall: set_tid_address
nr_x86_64: 218
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/1954
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_tid_address&body=Discussion+for+the+%60set_tid_address%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_tid_address.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_tid_address%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`set_tid_address(tidptr)` sets the calling thread's `clear_child_tid` address and returns the caller's thread ID. Hermit forwards the call to the kernel as a reviewed pass-through. It also copies the address into the scheduler, so that the scheduler's modeled CHILD_CLEARTID futex wake at thread exit targets the same word that the kernel clears.

## Linux semantics

The call takes a single `int *tidptr` and cannot fail. It stores the pointer in the calling task's `clear_child_tid` field and returns the caller's TID, as seen in the caller's PID namespace. The pointer is not dereferenced at call time.

The registration takes effect when the thread's memory is released, which happens at exit (normal exit, `exit_group`, or a fatal signal) and during a successful `execve`. At that point, if the address is non-NULL and the address space is still shared with another task, the kernel writes 0 to the address and does a `FUTEX_WAKE` of one waiter. It then resets `clear_child_tid` to NULL. A failed store is ignored silently. `clone` with `CLONE_CHILD_CLEARTID` sets the same field for the new task, and glibc's `pthread_join` relies on that clear and wake. `set_tid_address` replaces whatever value is there.

### Flags and options

- F1: Non-NULL `tidptr`: registers the address as `clear_child_tid` and returns the caller's TID.
- F2: NULL `tidptr`: removes any registration (no exit-time store or wake), and still returns the caller's TID.
- F3: Replacement: the last registration wins. A call replaces an earlier call's address, and the `ctid` that `clone(CLONE_CHILD_CLEARTID)` installed.
- F4: Exit-time clear and wake: when the thread exits and the address space is shared with another task, the kernel stores 0 at the address and wakes at most one futex waiter on it. A thread that is the last user of its address space gets no store.
- F5: Invalid or unmapped `tidptr`: accepted without error at call time. The exit-time store faults silently, and nothing else changes.
- F6: Per-task lifetime: a new task starts registered only if `clone` passed `CLONE_CHILD_CLEARTID`, and unregistered otherwise. A successful `execve` performs the F4 clear and wake for a shared address space, then resets the registration to NULL.

## Determinism analysis

`linux_determinism: conditional`. The call has no inputs other than the pointer and always succeeds. The guest sees two things: the returned TID, and the exit-time store and wake.

### Nondeterminism sources

- The returned TID is whatever the kernel allocated. Under Hermit's container PID namespace, with thread creation sequentialized, TIDs are allocated in a fixed order. Outside a fresh PID namespace they depend on host activity.
- Which waiter the exit-time `FUTEX_WAKE` releases, and when the woken thread runs relative to its siblings, depends on host scheduling.
- The F4 store happens during kernel teardown of the exiting task. The order of that store relative to other threads' memory accesses is fixed only if the guest's threads are serialized.

The call repeats when TIDs come from a deterministic PID namespace and thread exit and waking are ordered by a deterministic scheduler.

## Hermit design

Target policy: `passthrough`. The kernel owns the registration and the return value, and Detcore keeps a scheduler-side copy of the address so its modeled wake matches the kernel's.

What the code does at hermit f4055eb9cae4:

- Classification: a reviewed pass-through (`detcore/src/syscall_classification.rs:712`). The block comment at `detcore/src/syscall_classification.rs:691-692` scopes these pass-throughs as "conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions".
- Dispatch: a dedicated PassThrough arm calls `handle_set_tid_address` (`detcore/src/lib.rs:2819-2824`). The call is subscribed even under record/replay's pass-through optimization, so the scheduler always sees address changes (`detcore/src/lib.rs:1150-1153`).
- Handler: "Linux owns the guest-visible registration and return value" (`detcore/src/syscalls/threads.rs:1009-1011`). The call is forwarded through `record_or_replay` (`detcore/src/syscalls/threads.rs:1018-1020`). When `sequentialize_threads` is on, the address (0 for NULL) is copied into the scheduler (`detcore/src/syscalls/threads.rs:1017`, `detcore/src/syscalls/threads.rs:1021-1022`; the scheduler stores it at `detcore/src/scheduler.rs:2173-2179`).
- Exit: when the scheduler logically kills a thread with a non-zero copied address, it wakes the modeled futex waiter at that address (`detcore/src/scheduler.rs:2330-2335`). It wakes one waiter, matching the man page (`detcore/src/scheduler.rs:2896-2905`).
- Clone: a new thread's address is copied only when `CLONE_CHILD_CLEARTID` is set (`detcore/src/tool_global.rs:3412-3418`).
- Exec: a successful exec resets the copy to 0 (`detcore/src/lib.rs:1766-1770`).

The returned TID is the raw kernel TID, because Detcore does not virtualize TIDs (`detcore/src/lib.rs:1492`). `gettid` is also a reviewed pass-through (`detcore/src/syscall_classification.rs:705`), so the two calls agree. The value is deterministic because of the container PID namespace and sequentialized creation, not because of any translation.

Why not model or emulate: the 2026-08-07 strategy draft (kept below under Additional information) asked for a logical TID and a Detcore-performed clear and wake at a deterministic exit point. The current code covers the part of that draft that affects determinism. The kernel's return value already equals the TID that every other guest-visible interface reports. The scheduler supplies the wake that releases the modeled waiter at the deterministic exit point. A separate logical TID would only be meaningful as part of TID virtualization, which is tracked as `T78538674` at `detcore/src/lib.rs:1492` and is outside this syscall.

Known gaps:

- The header comment of the existing fixture says "Hermit virtualizes thread IDs" (`tests/c/set_tid_address.c:8`). That is inaccurate at this SHA, although the fixture's assertions do not depend on it.
- No test today exercises F3 through F6 under Hermit.

## Recommended tests

- T1 (covers F1, F2): Raw `SYS_set_tid_address` with a valid pointer, a second valid pointer, then NULL. Each call returns the same positive TID, equal to `gettid()`. Existing candidate: `tests/c/set_tid_address.c` (cell `c-programs/set-tid-address`, `tests/e2e/manifests/c-programs.yaml:10996`, verify CI on ptrace at `tests/e2e/manifests/c-programs.yaml:11013-11015`, with `bitwise_parity` at `tests/e2e/manifests/c-programs.yaml:11023`).
- T2 (covers F3, F4): A thread created with `CLONE_CHILD_CLEARTID` calls `set_tid_address` on a second word and then exits. A joiner waiting on the second word with `FUTEX_WAIT` is woken and observes 0, and the original `ctid` word is left unchanged. Run under `--verify`.
- T3 (covers F4): Two waiters on one cleared word: exactly one is woken by the exit, and the other times out deterministically on a virtual-time timeout. A single-threaded process that registers an address and exits produces no store, checked through a shared mapping by the parent.
- T4 (covers F5): Register an unmapped address (for example a page that was just unmapped), then exit the thread. The process continues and the sibling threads observe no fault.
- T5 (covers F6): A `CLONE_VM` child (not a thread) registers the parent's word and execs. The parent's futex wait on that word returns with the word cleared. A plain `fork` child reports no inherited registration: its exit does not clear the parent's word.
- T6 (covers F1, F4): Record and replay T2. The replay reports the same TID and the same wake order.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. set_tid_address is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1954: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch sysno-guard.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> passthrough (the kernel owns the registration and the raw namespaced TID; Detcore copies the address into the scheduler for its modeled CHILD_CLEARTID wake and resets it on exec, which supersedes the 2026-08-07 logical-TID draft); linux_determinism stays conditional; 6 flag behaviours and 6 tests listed; only T1 has an existing candidate (tests/c/set_tid_address.c), no coverage token recorded, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/set_tid_address.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `set_tid_address`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_tid_address&body=Discussion+for+the+%60set_tid_address%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_tid_address.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_tid_address%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: set_tid_address" (https://github.com/rrnewton/hermit/issues/1954), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy strategy draft (2026-08-07)

The legacy body's Hermit design read: "Return the logical TID, store the guest clear-child address in logical-thread state, and perform clear-plus-wake at the deterministic exit point with explicit memory/futex ordering." Its determinism note read: "Host forwarding can expose a host TID and couples exit-time memory clearing and futex wakeup to backend teardown order." Hermit design above explains how the code at f4055eb9cae4 handles each point.

Legacy required tests (2026-08-07), now folded into T1-T6: null and valid addresses, replacement, invalid or unmapped memory at exit, main and child threads; returned TID, zeroing, futex wake count, and ordering relative to wait and exit; clone flags that also set child-tid addresses, and exec; identical strict-repeat and record/replay behaviour.

### Legacy acceptance text (2026-08-07)

Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

set_tid_address is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
