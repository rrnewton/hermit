---
schema: syscall-pages/v1
syscall: mq_getsetattr
nr_x86_64: 245
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/298
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_getsetattr&body=Discussion+for+the+%60mq_getsetattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_getsetattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_getsetattr%60%2C+or+in+what+the+page+says%3F%0A
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

`mq_getsetattr(mqdes, newattr, oldattr)` reads and optionally changes the attributes of a POSIX message-queue descriptor; glibc's `mq_getattr` and `mq_setattr` are built on it. Detcore refuses it with a fixed `ENOSYS` before the kernel sees it (`detcore/src/lib.rs:2237-2241`), as it refuses the whole `mq_*` family; that is what a kernel built without `CONFIG_POSIX_MQUEUE` returns.

## Linux semantics

`struct mq_attr` holds `mq_flags`, `mq_maxmsg`, `mq_msgsize` and `mq_curmsgs`. Only `O_NONBLOCK` in `mq_flags` can be changed, and the change applies to the open file description, so every `dup` copy sees it. `mq_maxmsg` and `mq_msgsize` are fixed when the queue is created. `mq_curmsgs` is the number of messages in the queue at the moment of the call, which reflects every sender and receiver using the queue.

No strace line was captured for this call (see Additional information).

### Flags and options

- F1: `oldattr` non-NULL: store the current attributes (`mq_flags`, `mq_maxmsg`, `mq_msgsize`, `mq_curmsgs`); this is `mq_getattr`.
- F2: `newattr` non-NULL: set or clear `O_NONBLOCK` from `newattr->mq_flags`; `mq_maxmsg`, `mq_msgsize` and `mq_curmsgs` in `newattr` are ignored. This is `mq_setattr`.
- F3: both non-NULL: `oldattr` receives the attributes as they were before `newattr` was applied, in one call.
- F4: both NULL: only `mqdes` is checked; the call returns 0 for a valid descriptor.
- F5: `newattr->mq_flags` with any bit other than `O_NONBLOCK`: `EINVAL`.
- F6: errors: `EBADF` if `mqdes` is not a message-queue descriptor; `EFAULT` for a bad `newattr` or `oldattr` pointer.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. `mq_flags`, `mq_maxmsg` and `mq_msgsize` repeat given the same descriptor. `mq_curmsgs` reports how many messages the queue holds at that instant, which depends on how far concurrent senders and receivers, possibly outside the guest, have progressed under host scheduling, and on messages left from earlier runs. The result repeats only when no other process uses the queue concurrently and the queue starts in the same state.

### Nondeterminism sources

- `mq_curmsgs`, which reflects concurrent producers and consumers
- Queue contents shared with processes outside the guest, and persisting across runs
- `O_NONBLOCK` changes made through a shared open file description by another process

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_POSIX_MQUEUE`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. A program that needs a message queue has to fall back to another IPC channel, such as a pipe, a Unix-domain socket or a mutex-guarded queue in shared memory; this body names no specific program that does so. Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `mq_getsetattr` (`detcore/src/syscall_classification.rs:414`) in the arm that returns `SyscallClassification::Determinized` (`detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` includes it (`detcore/src/syscall_classification.rs:1065`), with the rest of the `mq_*` family (`detcore/src/syscall_classification.rs:1060-1064`).
- In `handle_syscall_event`, the match on the classification (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2237-2241`, which returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall. The host kernel never runs it, so no attributes are read or changed. The arm does not consult `panic_on_unsupported_syscalls`, so the result is the same with or without fail-closed mode.
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`).

Why `refuse` and not the alternatives:

- `mq_open` is refused (`detcore/src/syscall_classification.rs:1060`), so no guest can obtain a message-queue descriptor through Hermit, and there is nothing for `mq_getsetattr` to act on. Refusing it keeps the family consistent.
- `passthrough` would report `mq_curmsgs` from a queue shared with the host. The predicate's doc comment (`detcore/src/syscall_classification.rs:1034-1047`) calls the legacy pass-through nondeterministic and a container-isolation hole.
- `model` would need a Detcore-owned queue model, and only makes sense as part of modelling all six `mq_*` calls. The legacy NEEDS-HANDLER recommendation predates this family-wide refusal.
- `ENOSYS` matches a real kernel without `CONFIG_POSIX_MQUEUE`.

Guest-visible difference from Linux: F1-F6 all return `ENOSYS`, including the `EBADF` and `EINVAL` cases, because the refusal precedes every check. Nothing is written through `oldattr`. This is the intended refusal.

Existing test evidence at f4055eb9cae4: the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts, in its loop at `detcore/src/syscall_classification.rs:2143`, that `mq_getsetattr` classifies as Determinized and is in the predicate. It does not run a guest and does not check the errno. No e2e program or integration test calls `mq_getsetattr`.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mq_getsetattr, ...)` on an invalid descriptor (-1) and on an ordinary pipe descriptor, with only `oldattr`, only `newattr` (`mq_flags` 0 and `O_NONBLOCK`), both, neither, a `newattr` with an extra flag bit, and a bad pointer. Each call must return -1 with `errno == ENOSYS`, the `oldattr` buffer must keep its sentinel bytes, and the output must be identical across both runs.
- T2 (covers none): record and replay a guest that calls `mq_getsetattr`; replay must produce the same `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/298#issuecomment-5872857558
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/298: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mq_getsetattr records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/298 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns a fixed ENOSYS for every mq_* call at detcore/src/lib.rs:2237-2241, the result of a kernel without CONFIG_POSIX_MQUEUE; mq_open is refused too, so no guest obtains a queue descriptor); linux_determinism stays conditional; 6 flag behaviours and 2 tests listed; no guest-level test calls mq_getsetattr yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_POSIX_MQUEUE, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no questions removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mq_getsetattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_getsetattr&body=Discussion+for+the+%60mq_getsetattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_getsetattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_getsetattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mq_getsetattr" (https://github.com/rrnewton/hermit/issues/298), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/298.

The 2026-07-24 evidence below says Hermit routed `mq_getsetattr` through a temporary pass-through block. That was true then; at f4055eb9cae4 the call is refused with `ENOSYS` (see Hermit design).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mq_getsetattr`, and by confirming hermit currently routes `mq_getsetattr` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
(not triggered by tested programs; classification from man page + kernel behavior)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/298#issuecomment-5872857558
