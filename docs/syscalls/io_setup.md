---
schema: syscall-pages/v1
syscall: io_setup
nr_x86_64: 206
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/615
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_setup&body=Discussion+for+the+%60io_setup%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_setup.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_setup%60%2C+or+in+what+the+page+says%3F%0A
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

`io_setup(nr_events, ctx_idp)` creates a Linux native AIO context able to hold `nr_events` in-flight requests and stores its handle in `*ctx_idp`. Hermit refuses it: Detcore returns a fixed `ENOSYS` without forwarding the call (`detcore/src/lib.rs:2237-2241`), as on a kernel built without `CONFIG_AIO`.

Manual reference: Linux man-pages io_setup(2).

## Linux semantics

On success the kernel maps a completion ring into the caller's address space and returns the ring's user-space address as the context handle (`aio_context_t`). The address is chosen by the kernel's mmap placement. The kernel raises the requested capacity to at least four events per possible CPU and then doubles it; the ring header, readable in guest memory, records that capacity. Every context counts against a host-wide budget: `/proc/sys/fs/aio-nr` must stay within `/proc/sys/fs/aio-max-nr`.

A context is a kernel object owned by the caller's address space (its `mm`), not by a file descriptor. A forked child does not inherit it: the child's lookup fails with `EINVAL`. All contexts are destroyed when the address space goes away, at exit or `execve`. A kernel built without `CONFIG_AIO` returns `ENOSYS` for every `io_*` call.

### Flags and options

- F1: success: `nr_events > 0` and `*ctx_idp == 0` on entry. The kernel creates the context and its ring mapping and stores the ring address in `*ctx_idp`. The capacity is raised to at least 4 per possible CPU, then doubled.
- F2: `*ctx_idp` not zero on entry: `EINVAL`
- F3: `nr_events == 0`, or so large that the doubled capacity overflows the kernel's limit: `EINVAL`
- F4: `nr_events` (the requested value, not the raised capacity) exceeds `/proc/sys/fs/aio-max-nr`, or would push the host-wide `/proc/sys/fs/aio-nr` past it: `EAGAIN`
- F5: `ctx_idp` not readable or not writable: `EFAULT`. If the handle cannot be stored, the new context is torn down.
- F6: kernel without `CONFIG_AIO`: `ENOSYS`. Kernel memory exhausted: `ENOMEM`.
- F7: lifetime: the context belongs to the address space; a forked child cannot use it (`EINVAL`), and exit or `execve` destroys it

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The error paths (F2, F3, F5) depend only on the arguments. A successful call does not repeat across hosts and often not across runs:

- the handle is a kernel-chosen mmap address, so it changes with address-space randomization and with every other mapping in the process;
- `EAGAIN` depends on the host-wide `aio-nr`, which other processes on the host raise and lower;
- the ring capacity depends on the host's possible-CPU count.

The call repeats when address layout is fixed, the host budget is not contended, and the CPU count is the same.

### Nondeterminism sources

- The mmap placement of the ring, which is the handle value
- The host-wide `aio-nr` budget, shared with every process on the host
- The host's possible-CPU count, visible through the ring capacity
- A new mapping (`[aio]`) in `/proc/self/maps`

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_AIO`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. The supportable fallback is synchronous or thread-based I/O: glibc's POSIX AIO (`aio_read`, `aio_write`) runs requests on user-space threads and never calls the native AIO syscalls, and callers of native AIO such as MySQL InnoDB switch to their own simulated AIO when `io_setup` fails (recalled, not re-checked in the InnoDB source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `io_setup` as Determinized (`detcore/src/syscall_classification.rs:403`, in the arm that ends at `detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) names it at `detcore/src/syscall_classification.rs:1054`.
- The guard arm in `handle_syscall_event` (`detcore/src/lib.rs:2237-2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every syscall that predicate names. The arm never injects the call, so the host kernel never sees it, whatever the arguments are, and no argument is validated first.
- The arm does not consult `panic_on_unsupported_syscalls`, so fail-closed mode and the compatibility opt-out return the same `ENOSYS` (`refused_only_when_fail_closed: false`).
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`), so backends that consult that boundary see the same refusal.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`, batch-7 loop at `detcore/src/syscall_classification.rs:2139-2171`) asserts the classification and the predicate membership. It does not launch Hermit, so it is not coverage for the flags below.

The doc comment (`detcore/src/syscall_classification.rs:1034-1047`) gives the reason: Linux native AIO completion is driven by the kernel and the device, outside the guest's logical time. Forwarding these calls, as the legacy pass-through did, is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_AIO` returns, so a guest sees a consistent, if smaller, kernel.

Why `refuse` and not the alternatives:

- `io_setup` creates every context. Refusing it means no guest ever holds a valid context, and the refusals of `io_submit`, `io_getevents`, `io_cancel` and `io_destroy` keep a guessed handle from reaching the host. Programs that probe `io_setup` and fall back to synchronous or thread-based I/O on failure keep working; programs that require native AIO fail with a clear `ENOSYS`.
- `model` would need Detcore-owned contexts at a deterministic address, every request executed at a deterministic point (for example synchronously inside `io_submit`), completions queued in a deterministic order, and `io_getevents` waits and timeouts owned by the scheduler and virtual time. None of that exists at this SHA. It stays a possible later upgrade if a workload needs this interface; it is not today's target.
- `emulate` (for example, returning success without doing anything) would silently lose data or hand back invented state, which is worse than a clear `ENOSYS`.
- `passthrough` reintroduces the host-dependent behaviour described under Determinism analysis, and the isolation hole.

Under `refuse`, every behaviour in `### Flags and options` is replaced by the same `-1`/`ENOSYS` with no side effect. The flags inventory still lists the Linux behaviours, because a test of the refusal must show that no argument shape gets past it, and because a later `model` would have to implement them.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F6): under `hermit run --verify` on ptrace, call raw `syscall(SYS_io_setup, ...)` with one argument shape per flag: 128 events with `ctx == 0`, a nonzero `ctx`, 0 events, and `0xffffffff` events. Every call must return -1 with `errno == ENOSYS`, and both runs must print the same output.
  Existing candidate: `tests/c/aio_refusal.c` (cell `c-programs/aio-refusal`, `tests/e2e/manifests/c-programs.yaml:10929`, verify mode with ci on ptrace). It calls only the first shape (`tests/c/aio_refusal.c:34`).
- T2 (covers F1, F5, F7): the refusal has no side effect. `*ctx_idp` stays 0 (`tests/c/aio_refusal.c:48` checks this), no `[aio]` mapping appears in `/proc/self/maps`, a `NULL` `ctx_idp` gives `ENOSYS` and not `EFAULT` (the refusal comes before argument validation), and a forked child gets the same `ENOSYS`.
- T3 (covers none): the same `ENOSYS` in fail-closed mode and under the compatibility opt-out, and a record followed by a replay reproduces it.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/615: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (detcore/src/lib.rs:2237-2241 returns a fixed ENOSYS for the native AIO family without forwarding, so no guest ever holds a context); linux_determinism stays conditional; 7 flag behaviours and 3 tests listed; tests/c/aio_refusal.c (cell c-programs/aio-refusal) is a candidate for T1/T2 but exercises one argument shape, so no coverage token yet and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_AIO, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no open questions to remove

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_setup`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_setup&body=Discussion+for+the+%60io_setup%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_setup.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_setup%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: io_setup" (https://github.com/rrnewton/hermit/issues/615), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### P3 review notes (2026-10-03, hermit f4055eb9cae4)

- The legacy recommended action **NEEDS-HANDLER** predates the deterministic `ENOSYS` refusal whose code is tagged `TODO-HUMAN-REVIEW(#731)` (https://github.com/rrnewton/hermit/issues/731). At this SHA the native AIO family is refused by `is_unsupported_async_ipc_syscall`, so the target is `refuse`, not a new handler.
- A later upgrade to `model` remains possible if a workload needs this interface; Hermit design lists what it would need.
