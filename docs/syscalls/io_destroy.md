---
schema: syscall-pages/v1
syscall: io_destroy
nr_x86_64: 207
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/612
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_destroy&body=Discussion+for+the+%60io_destroy%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_destroy.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_destroy%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_unsupported_async_ipc_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`io_destroy(ctx_id)` tears down a Linux native AIO context: it cancels what it can, waits for the remaining in-flight requests, and unmaps the completion ring. Hermit refuses it: Detcore returns a fixed `ENOSYS` without forwarding the call (`detcore/src/lib.rs:2237-2241`), as on a kernel built without `CONFIG_AIO`.

Manual reference: Linux man-pages io_destroy(2).

## Linux semantics

The kernel looks `ctx_id` up in the caller's context table and marks the context dead. Requests that have a cancel method (for example `IOCB_CMD_POLL`) are cancelled. The call then waits until every request has completed, so that the kernel no longer touches the caller's buffers once it returns. The ring is unmapped and completion events still in it are discarded. Completions are still signalled to any `IOCB_FLAG_RESFD` eventfd.

A context is a kernel object owned by the caller's address space (its `mm`), not by a file descriptor. A forked child does not inherit it: the child's lookup fails with `EINVAL`. All contexts are destroyed when the address space goes away, at exit or `execve`. A kernel built without `CONFIG_AIO` returns `ENOSYS` for every `io_*` call.

### Flags and options

- F1: valid `ctx_id`: cancel cancellable requests, wait for all others to complete, unmap the ring, free the context, return 0
- F2: `ctx_id` naming no live context of this address space (never created, already destroyed, or inherited across `fork`): `EINVAL`
- F3: concurrent `io_destroy` calls on one context: one returns 0, the others `EINVAL`
- F4: kernel without `CONFIG_AIO`: `ENOSYS`

## Determinism analysis

`linux_determinism: conditional` (was `nondeterministic`). The return value depends only on whether `ctx_id` names a live context, which the guest controls. Because the call waits for every in-flight request, the caller's buffers hold the same contents afterwards whatever the device timing was. The legacy reason, "pending-completion state is asynchronous", describes how long the call takes, not what it returns.

Two things still vary:

- How long the call blocks depends on host device latency.
- When two threads race `io_destroy` with each other, or with `io_submit` on the same context, which one wins depends on host scheduling (F3).

The call repeats when one thread owns the context's teardown and nothing races it.

### Nondeterminism sources

- Host device latency of in-flight requests, which sets how long the call waits
- Scheduling races with concurrent `io_destroy` or `io_submit` on the same context
- Whether a cancellable request completes or is cancelled first. The result of that race is discarded with the ring, but it changes the timing.

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_AIO`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. The supportable fallback is synchronous or thread-based I/O: glibc's POSIX AIO (`aio_read`, `aio_write`) runs requests on user-space threads and never calls the native AIO syscalls, and callers of native AIO such as MySQL InnoDB switch to their own simulated AIO when `io_setup` fails (recalled, not re-checked in the InnoDB source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `io_destroy` as Determinized (`detcore/src/syscall_classification.rs:404`, in the arm that ends at `detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) names it at `detcore/src/syscall_classification.rs:1055`.
- The guard arm in `handle_syscall_event` (`detcore/src/lib.rs:2237-2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every syscall that predicate names. The arm never injects the call, so the host kernel never sees it, whatever the arguments are, and no argument is validated first.
- The arm does not consult `panic_on_unsupported_syscalls`, so fail-closed mode and the compatibility opt-out return the same `ENOSYS` (`refused_only_when_fail_closed: false`).
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`), so backends that consult that boundary see the same refusal.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`, batch-7 loop at `detcore/src/syscall_classification.rs:2139-2171`) asserts the classification and the predicate membership. It does not launch Hermit, so it is not coverage for the flags below.

The doc comment (`detcore/src/syscall_classification.rs:1034-1047`) gives the reason: Linux native AIO completion is driven by the kernel and the device, outside the guest's logical time. Forwarding these calls, as the legacy pass-through did, is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_AIO` returns, so a guest sees a consistent, if smaller, kernel.

Why `refuse` and not the alternatives:

- `io_setup` is refused by the same predicate (`detcore/src/syscall_classification.rs:1054`), so a guest can never obtain a valid handle from inside the container. Any handle `io_destroy` receives is either garbage or a guess at a host object. Refusing `io_destroy` as well keeps the "kernel without `CONFIG_AIO`" picture consistent and keeps a guessed handle from reaching the host.
- `model` would need Detcore-owned contexts at a deterministic address, every request executed at a deterministic point (for example synchronously inside `io_submit`), completions queued in a deterministic order, and `io_getevents` waits and timeouts owned by the scheduler and virtual time. None of that exists at this SHA. It stays a possible later upgrade if a workload needs this interface; it is not today's target.
- `emulate` (for example, returning success without doing anything) would silently lose data or hand back invented state, which is worse than a clear `ENOSYS`.
- `passthrough` reintroduces the host-dependent behaviour described under Determinism analysis, and the isolation hole.

Under `refuse`, every behaviour in `### Flags and options` is replaced by the same `-1`/`ENOSYS` with no side effect. The flags inventory still lists the Linux behaviours, because a test of the refusal must show that no argument shape gets past it, and because a later `model` would have to implement them.

## Recommended tests

- T1 (covers F1, F2, F3, F4): under `hermit run --verify` on ptrace, call raw `syscall(SYS_io_destroy, ...)` with a guessed handle (`0x1234`), with 0, and twice in a row with the same value, and from two threads at once. Every call must return -1 with `errno == ENOSYS`, and both runs must print the same output.
  Existing candidate: `tests/c/aio_refusal.c` (cell `c-programs/aio-refusal`, `tests/e2e/manifests/c-programs.yaml:10929`, verify mode with ci on ptrace). It calls `io_destroy(0x1234)` once (`tests/c/aio_refusal.c:85`).
- T2 (covers F1): the refusal never reaches a host context. A process that obtained a real context outside Hermit cannot pass it into the guest's address space, so instead check that `io_destroy` after a refused `io_setup` returns `ENOSYS` and not `EINVAL`, which shows the call was not forwarded.
- T3 (covers none): the same `ENOSYS` in fail-closed mode and under the compatibility opt-out, and a record followed by a replay reproduces it.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/612: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (detcore/src/lib.rs:2237-2241 returns a fixed ENOSYS for the native AIO family without forwarding); linux_determinism nondeterministic -> conditional (the result depends only on whether the context is live and the call waits for all in-flight I/O, so only its duration and races on one context vary); 4 flag behaviours and 3 tests listed; tests/c/aio_refusal.c calls io_destroy once, a candidate for T1 only, so no coverage token yet and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_AIO, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no open questions to remove
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/aio_refusal.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_destroy`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_destroy&body=Discussion+for+the+%60io_destroy%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_destroy.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_destroy%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: io_destroy" (https://github.com/rrnewton/hermit/issues/612), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### P3 review notes (2026-10-03, hermit f4055eb9cae4)

- The legacy recommended action **NEEDS-HANDLER** predates the deterministic `ENOSYS` refusal whose code is tagged `TODO-HUMAN-REVIEW(#731)` (https://github.com/rrnewton/hermit/issues/731). At this SHA the native AIO family is refused by `is_unsupported_async_ipc_syscall`, so the target is `refuse`, not a new handler.
- A later upgrade to `model` remains possible if a workload needs this interface; Hermit design lists what it would need.
