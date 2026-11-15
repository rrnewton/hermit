---
schema: syscall-pages/v1
syscall: mq_unlink
nr_x86_64: 241
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/303
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_unlink&body=Discussion+for+the+%60mq_unlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_unlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_unlink%60%2C+or+in+what+the+page+says%3F%0A
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

`mq_unlink(name)` removes the name of a POSIX message queue; the queue itself is destroyed once its last descriptor is closed. Detcore refuses it with a fixed `ENOSYS` before the kernel sees it (`detcore/src/lib.rs:2237-2241`), as it refuses the whole `mq_*` family; that is what a kernel built without `CONFIG_POSIX_MQUEUE` returns.

## Linux semantics

The name lives in the IPC namespace, on the internal `mqueue` filesystem. The glibc wrapper requires a leading slash and strips it; the raw syscall takes the name without it. Removing the name does not affect processes that already hold descriptors: they keep using the queue until they close it. A new `mq_open` of the same name afterwards creates or opens a different queue. Queue names persist across runs, so the outcome depends on what earlier runs and other processes left behind.

The host strace under Additional information shows a nonexistent name failing with `EACCES` on that host, where `/dev/mqueue` access was restricted; on a host with an accessible `mqueue` filesystem the same call gives `ENOENT`.

### Flags and options

- F1: `name` names an existing queue the caller may remove: the name is removed and the call returns 0; the queue persists until its last descriptor closes.
- F2: `name` does not exist: `ENOENT`.
- F3: the caller lacks permission to remove the name: `EACCES`.
- F4: name checks: `ENAMETOOLONG` for a name longer than `NAME_MAX`; `EFAULT` for a bad pointer.

## Determinism analysis

`linux_determinism: conditional`, changed from the legacy `deterministic`. The call reads no clock and returns no value beyond success or an errno, so it repeats given the same IPC namespace state. That state is not under the guest's control: queue names persist across runs and can be created or removed by any process in the namespace, including processes outside the guest, and host permissions on the `mqueue` filesystem decide between `ENOENT` and `EACCES`, as the strace evidence shows. The legacy text itself conditions determinism on "fixed IPC namespace state", which is what `conditional` means in this schema.

### Nondeterminism sources

- Queue names shared with every process in the IPC namespace, and persisting across runs
- Host permissions on the `mqueue` filesystem

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_POSIX_MQUEUE`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. A program that needs a message queue has to fall back to another IPC channel, such as a pipe, a Unix-domain socket or a mutex-guarded queue in shared memory; this body names no specific program that does so. Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `mq_unlink` (`detcore/src/syscall_classification.rs:410`) in the arm that returns `SyscallClassification::Determinized` (`detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` includes it (`detcore/src/syscall_classification.rs:1061`), with the rest of the `mq_*` family (`detcore/src/syscall_classification.rs:1060-1065`).
- In `handle_syscall_event`, the match on the classification (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2237-2241`, which returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall. The host kernel never runs it, so a guest cannot remove a host queue name. The arm does not consult `panic_on_unsupported_syscalls`, so the result is the same with or without fail-closed mode.
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`).

Why `refuse` and not the alternatives:

- `mq_open` is refused too (`detcore/src/syscall_classification.rs:1060`), so no guest can create a queue under Hermit. The only names `mq_unlink` could reach are host queues, and removing them would let a guest change state outside its container. The predicate's doc comment (`detcore/src/syscall_classification.rs:1034-1047`) calls the legacy pass-through a container-isolation hole.
- `model` is meaningful only together with a modelled `mq_open`, a private queue namespace and the rest of the family. Until that exists, refusing the whole family keeps it consistent.
- `ENOSYS` matches a real kernel without `CONFIG_POSIX_MQUEUE`, so a program cleaning up queues sees a coherent configuration in which no queue call works.

Guest-visible difference from Linux: F1-F4 all return `ENOSYS`, including the `ENOENT` and `EACCES` cases. This is the intended refusal.

Existing test evidence at f4055eb9cae4: the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts, in its loop at `detcore/src/syscall_classification.rs:2143`, that `mq_unlink` classifies as Determinized and is in the predicate. It does not run a guest and does not check the errno. No e2e program or integration test calls `mq_unlink`.

## Recommended tests

- T1 (covers F1, F2, F3, F4): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mq_unlink, ...)` with a short valid name, a name that does not exist, a name longer than `NAME_MAX`, and a NULL pointer. Each call must return -1 with `errno == ENOSYS`, and the output must be identical across both runs.
- T2 (covers F1): create a queue on the host before the run (where the host allows it), run a guest that calls `mq_unlink` on its name under Hermit, and check after the run that the host queue still exists. Remove it on the host afterwards.
- T3 (covers none): record and replay a guest that calls `mq_unlink`; replay must produce the same `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/303#issuecomment-5872868701
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/303: linux_determinism deterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mq_unlink records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/303 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns a fixed ENOSYS for every mq_* call at detcore/src/lib.rs:2237-2241, the result of a kernel without CONFIG_POSIX_MQUEUE); linux_determinism deterministic -> conditional (the result depends on host-shared, run-persistent queue names and mqueue permissions); 4 behaviours and 3 tests listed; no guest-level test calls mq_unlink yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_POSIX_MQUEUE, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no questions removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mq_unlink`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_unlink&body=Discussion+for+the+%60mq_unlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_unlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_unlink%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mq_unlink" (https://github.com/rrnewton/hermit/issues/303), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/303.

The 2026-07-24 evidence below says Hermit routed `mq_unlink` through a temporary pass-through block. That was true then; at f4055eb9cae4 the call is refused with `ENOSYS` (see Hermit design).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mq_unlink`, and by confirming hermit currently routes `mq_unlink` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mq_unlink("/nonexist-q-xyz")    = -1 EACCES (Permission denied)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/303#issuecomment-5872868701
