---
schema: syscall-pages/v1
syscall: mq_open
nr_x86_64: 240
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/300
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_open&body=Discussion+for+the+%60mq_open%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_open.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_open%60%2C+or+in+what+the+page+says%3F%0A
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

`mq_open(name, oflag, mode, attr)` creates or opens a POSIX message queue and returns a message-queue descriptor, which on Linux is a file descriptor. Detcore refuses it with a fixed `ENOSYS` before the kernel sees it (`detcore/src/lib.rs:2237-2241`), as it refuses the whole `mq_*` family; that is what a kernel built without `CONFIG_POSIX_MQUEUE` returns.

## Linux semantics

A queue is named by a string in the IPC namespace and lives on the internal `mqueue` filesystem, which is often mounted at `/dev/mqueue`. The glibc wrapper requires a leading slash and strips it; the raw syscall takes the name without it. A queue persists until it is unlinked and its last descriptor is closed, so it outlives the process and the run. Every process in the IPC namespace with permission can open it. The kernel sets close-on-exec on the returned descriptor. The returned descriptor number is the lowest free slot in the caller's descriptor table.

The default and maximum queue sizes come from the host's `/proc/sys/fs/mqueue/` limits, and the number of queues is bounded by `queues_max` and by `RLIMIT_MSGQUEUE`.

The host strace under Additional information shows a nonexistent queue opened read-only failing with `EACCES` on that host, where `/dev/mqueue` access was restricted.

### Flags and options

- F1: access mode `O_RDONLY`, `O_WRONLY` or `O_RDWR`: decides whether the descriptor can receive, send, or both.
- F2: `O_CREAT`: create the queue if it does not exist, with permission bits `mode` (masked by the umask) and attributes from `attr`.
- F3: `attr` with `O_CREAT`: NULL takes the host defaults (`msg_default`, `msgsize_default`); otherwise `mq_maxmsg` and `mq_msgsize` must be positive and within the host limits (`msg_max`, `msgsize_max`) unless the caller has `CAP_SYS_RESOURCE`, else `EINVAL`.
- F4: `O_EXCL` with `O_CREAT`: `EEXIST` if the queue already exists.
- F5: `O_NONBLOCK`: later sends and receives on this descriptor fail with `EAGAIN` instead of waiting.
- F6: opening an existing queue without `O_CREAT`: `ENOENT` if it does not exist; `EACCES` if the caller lacks permission for the requested access mode.
- F7: name checks: `ENAMETOOLONG` for a name longer than `NAME_MAX`; `EACCES` for a name containing a slash after the one glibc strips; `EINVAL` from glibc for the name `/` alone.
- F8: resource limits: `EMFILE`, `ENFILE`, `ENOSPC` (`queues_max` reached) and `ENOMEM` (`RLIMIT_MSGQUEUE` exceeded).
- F9: the returned descriptor: an fd number allocated in the caller's table, with close-on-exec set.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Given a fixed IPC namespace, fixed host limits and a fixed descriptor table, the result and the returned number repeat. Those conditions often fail: queues persist across runs, so a name left by an earlier run changes `O_EXCL` and `ENOENT` outcomes; processes outside the guest can create or remove the same name; and the defaults and limits come from host sysctls.

### Nondeterminism sources

- Queue names shared with every process in the IPC namespace, and persisting across runs
- Host `mqueue` sysctls and `RLIMIT_MSGQUEUE`, which set defaults and limits
- Host permissions on the `mqueue` filesystem, which decided the `EACCES` in the strace evidence
- The returned descriptor number, which Hermit must keep deterministic like any other fd

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_POSIX_MQUEUE`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. A program that needs a message queue has to fall back to another IPC channel, such as a pipe, a Unix-domain socket or a mutex-guarded queue in shared memory; this body names no specific program that does so. Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `mq_open` (`detcore/src/syscall_classification.rs:409`) in the arm that returns `SyscallClassification::Determinized` (`detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` includes it (`detcore/src/syscall_classification.rs:1060`), with the other five `mq_*` calls (`detcore/src/syscall_classification.rs:1061-1065`).
- In `handle_syscall_event`, the match on the classification (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2237-2241`, which returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall. The host kernel never runs it, so no queue is created and no descriptor is allocated. The arm does not consult `panic_on_unsupported_syscalls`, so the result is the same with or without fail-closed mode.
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`).

Why `refuse` and not the alternatives:

- `passthrough` would let a guest create and open queues shared with the host that persist after the run. The predicate's doc comment (`detcore/src/syscall_classification.rs:1034-1047`) calls that nondeterministic and a container-isolation hole.
- `model` would need Detcore to keep a private queue namespace, the queue contents, priority ordering, blocking sends and receives on a scheduler-owned wait queue, virtual-time timeouts, and `mq_notify` delivery. That is a feature, not a fix, and it would have to cover all six `mq_*` calls at once. The legacy NEEDS-HANDLER recommendation predates this family-wide refusal.
- `ENOSYS` is what a real kernel without `CONFIG_POSIX_MQUEUE` returns, so programs that probe for message queues and fall back see a real Linux configuration.

Guest-visible difference from Linux: every F1-F9 case returns `ENOSYS`, including the `EACCES` and `ENOENT` cases, because the refusal precedes all argument checks. This is the intended refusal.

Existing test evidence at f4055eb9cae4: the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts, in its loop at `detcore/src/syscall_classification.rs:2143`, that `mq_open` classifies as Determinized and is in the predicate. It does not run a guest and does not check the errno. No e2e program or integration test calls `mq_open`. A model for a guest-level contract is `tests/c/aio_refusal.c` (cell `c-programs/aio-refusal`, `tests/e2e/manifests/c-programs.yaml:10929`, verify with ptrace in CI), which does the same for native AIO.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8, F9): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mq_open, ...)` with each access mode; with `O_CREAT` and a NULL `attr`; with `O_CREAT` and an explicit `attr`; with `O_CREAT|O_EXCL`; with `O_NONBLOCK`; without `O_CREAT` on a missing name; with a name containing a slash; and with a name longer than `NAME_MAX`. Each call must return -1 with `errno == ENOSYS`, and the output must be identical across both runs. After the calls, `fcntl` on the next free descriptor number must show it was not allocated.
- T2 (covers F2, F9): after a refused `O_CREAT` under Hermit, no queue with that name exists on the host: a host-side `mq_open(name, O_RDONLY)` after the run gets `ENOENT` (or `EACCES` where `/dev/mqueue` is restricted), never a descriptor.
- T3 (covers none): record and replay a guest that calls `mq_open`; replay must produce the same `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/300#issuecomment-5872862075
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/300: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mq_open records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/300 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns a fixed ENOSYS for every mq_* call at detcore/src/lib.rs:2237-2241, the result of a kernel without CONFIG_POSIX_MQUEUE); linux_determinism stays conditional; 9 flag behaviours and 3 tests listed; no guest-level test calls mq_open yet, so tracking_status stays todo; open question on host /dev/mqueue visibility under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_POSIX_MQUEUE, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no questions removed; mq_open's open question on host /dev/mqueue visibility stays

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mq_open`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mq_open&body=Discussion+for+the+%60mq_open%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmq_open.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mq_open%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mq_open" (https://github.com/rrnewton/hermit/issues/300), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/300.

The 2026-07-24 evidence below says Hermit routed `mq_open` through a temporary pass-through block. That was true then; at f4055eb9cae4 the call is refused with `ENOSYS` (see Hermit design).

### Open questions

- Refusing `mq_open` does not hide the host's `mqueue` filesystem. If `/dev/mqueue` is visible inside the guest, `open` and `read` on a queue file there would reach host queue state through ordinary file syscalls. Whether the guest sees `/dev/mqueue`, and whether Hermit gives the guest its own IPC namespace, could not be checked here: the read-only export at f4055eb9cae4 holds no `hermit-cli/src`. This is a question to check, not a policy decision.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mq_open`, and by confirming hermit currently routes `mq_open` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mq_open("/nonexist-q-xyz", O_RDONLY) = -1 EACCES (Permission denied)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/300#issuecomment-5872862075
