---
schema: syscall-pages/v1
syscall: io_cancel
nr_x86_64: 210
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/611
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_cancel&body=Discussion+for+the+%60io_cancel%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_cancel.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_cancel%60%2C+or+in+what+the+page+says%3F%0A
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

`io_cancel(ctx_id, iocb, result)` asks the kernel to cancel one request previously queued with `io_submit` on a Linux native AIO context. Hermit refuses it: Detcore returns a fixed `ENOSYS` without forwarding the call (`detcore/src/lib.rs:2237-2241`), as on a kernel built without `CONFIG_AIO`.

Manual reference: Linux man-pages io_cancel(2).

## Linux semantics

The kernel first reads the `aio_key` field that `io_submit` wrote into the `iocb`, then looks up the context, then searches the context's list of cancellable requests. Only requests that have a cancel method are on that list, for example `IOCB_CMD_POLL` and some driver-specific requests. Ordinary reads and writes on regular files are not.

In current kernels the `result` argument is ignored. A successful cancellation returns `EINPROGRESS`, and the cancelled request's completion event arrives later through the ring like any other. The man page still describes older behaviour: success returning 0 with the event copied to `result`, and `EAGAIN` for a request that was not cancelled.

A context is a kernel object owned by the caller's address space (its `mm`), not by a file descriptor. A forked child does not inherit it: the child's lookup fails with `EINVAL`. All contexts are destroyed when the address space goes away, at exit or `execve`. A kernel built without `CONFIG_AIO` returns `ENOSYS` for every `io_*` call.

### Flags and options

- F1: `iocb` not readable: `EFAULT`. `aio_key` not the value `io_submit` wrote: `EINVAL`. Both are checked before the context is looked up.
- F2: `ctx_id` naming no live context: `EINVAL`
- F3: the request is not on the cancellable list, because it already completed or has no cancel method: `EINVAL` (the man page says `EAGAIN`)
- F4: a cancellable request is found: cancellation starts, the call returns `EINPROGRESS`, and the completion event is delivered through the ring. `result` is not written.
- F5: kernel without `CONFIG_AIO`: `ENOSYS`

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. `io_cancel` races the request's completion: whether the request is still on the cancellable list when the call looks (F4) or has already completed (F3) depends on device and kernel timing outside the guest. The call repeats only when the outcome of that race is fixed, for example a request that can never be cancelled (always `EINVAL`) or a poll request on a descriptor that can never become ready.

### Nondeterminism sources

- The race between cancellation and completion of the request
- Which request types the kernel and the driver can cancel
- Kernel version: the result convention changed (0 with `result` filled in older kernels, `EINPROGRESS` now)

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_AIO`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. The supportable fallback is synchronous or thread-based I/O: glibc's POSIX AIO (`aio_read`, `aio_write`) runs requests on user-space threads and never calls the native AIO syscalls, and callers of native AIO such as MySQL InnoDB switch to their own simulated AIO when `io_setup` fails (recalled, not re-checked in the InnoDB source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `io_cancel` as Determinized (`detcore/src/syscall_classification.rs:406`, in the arm that ends at `detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) names it at `detcore/src/syscall_classification.rs:1057`.
- The guard arm in `handle_syscall_event` (`detcore/src/lib.rs:2237-2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every syscall that predicate names. The arm never injects the call, so the host kernel never sees it, whatever the arguments are, and no argument is validated first.
- The arm does not consult `panic_on_unsupported_syscalls`, so fail-closed mode and the compatibility opt-out return the same `ENOSYS` (`refused_only_when_fail_closed: false`).
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`), so backends that consult that boundary see the same refusal.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`, batch-7 loop at `detcore/src/syscall_classification.rs:2139-2171`) asserts the classification and the predicate membership. It does not launch Hermit, so it is not coverage for the flags below.

The doc comment (`detcore/src/syscall_classification.rs:1034-1047`) gives the reason: Linux native AIO completion is driven by the kernel and the device, outside the guest's logical time. Forwarding these calls, as the legacy pass-through did, is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_AIO` returns, so a guest sees a consistent, if smaller, kernel.

Why `refuse` and not the alternatives:

- `io_setup` is refused by the same predicate (`detcore/src/syscall_classification.rs:1054`), so a guest can never obtain a valid handle from inside the container. Any handle `io_cancel` receives is either garbage or a guess at a host object. Refusing `io_cancel` as well keeps the "kernel without `CONFIG_AIO`" picture consistent and keeps a guessed handle from reaching the host.
- `model` would need Detcore-owned contexts at a deterministic address, every request executed at a deterministic point (for example synchronously inside `io_submit`), completions queued in a deterministic order, and `io_getevents` waits and timeouts owned by the scheduler and virtual time. None of that exists at this SHA. It stays a possible later upgrade if a workload needs this interface; it is not today's target.
- `emulate` (for example, returning success without doing anything) would silently lose data or hand back invented state, which is worse than a clear `ENOSYS`.
- `passthrough` reintroduces the host-dependent behaviour described under Determinism analysis, and the isolation hole.

Under `refuse`, every behaviour in `### Flags and options` is replaced by the same `-1`/`ENOSYS` with no side effect. The flags inventory still lists the Linux behaviours, because a test of the refusal must show that no argument shape gets past it, and because a later `model` would have to implement them.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): under `hermit run --verify` on ptrace, call raw `syscall(SYS_io_cancel, ...)` on a guessed handle with a zeroed `iocb`, an `iocb` whose `aio_key` is nonzero, a `NULL` `iocb`, and a `NULL` and a valid `result`. Every call must return -1 with `errno == ENOSYS`, the `result` buffer must be unchanged, and both runs must print the same output.
- T2 (covers F4): with a refused `io_setup` and `io_submit`, an `io_cancel` on the same `iocb` returns `ENOSYS`, not `EINVAL` or `EINPROGRESS`, which shows it was not forwarded.
- T3 (covers none): the same `ENOSYS` in fail-closed mode and under the compatibility opt-out, and a record followed by a replay reproduces it.
  No test calls `io_cancel` at this SHA; `tests/c/aio_refusal.c` probes the other four AIO calls but not this one.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/611: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (detcore/src/lib.rs:2237-2241 returns a fixed ENOSYS for the native AIO family without forwarding); linux_determinism stays nondeterministic; 5 flag behaviours and 3 tests listed; no test calls io_cancel at this SHA, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_AIO, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no open questions to remove

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_cancel`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_cancel&body=Discussion+for+the+%60io_cancel%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_cancel.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_cancel%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: io_cancel" (https://github.com/rrnewton/hermit/issues/611), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### P3 review notes (2026-10-03, hermit f4055eb9cae4)

- The legacy recommended action **NEEDS-HANDLER** predates the deterministic `ENOSYS` refusal whose code is tagged `TODO-HUMAN-REVIEW(#731)` (https://github.com/rrnewton/hermit/issues/731). At this SHA the native AIO family is refused by `is_unsupported_async_ipc_syscall`, so the target is `refuse`, not a new handler.
- A later upgrade to `model` remains possible if a workload needs this interface; Hermit design lists what it would need.
