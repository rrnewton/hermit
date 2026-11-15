---
schema: syscall-pages/v1
syscall: io_getevents
nr_x86_64: 208
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/613
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_getevents&body=Discussion+for+the+%60io_getevents%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_getevents.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_getevents%60%2C+or+in+what+the+page+says%3F%0A
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

`io_getevents(ctx_id, min_nr, nr, events, timeout)` reaps between `min_nr` and `nr` completion events from a Linux native AIO context, waiting up to `timeout`. Hermit refuses it: Detcore returns a fixed `ENOSYS` without forwarding the call (`detcore/src/lib.rs:2237-2241`), as on a kernel built without `CONFIG_AIO`.

Manual reference: Linux man-pages io_getevents(2).

## Linux semantics

Each `struct io_event` holds the request's `data` cookie, a pointer to its `iocb` (`obj`), and the results `res` and `res2`. Events are returned in the order completions reached the ring, which is the order the device and the kernel finished the requests, not the order they were submitted.

`timeout` is relative and measured on the monotonic clock. When it expires the call returns the events it has, possibly none. The ring is also mapped into the caller's memory, and the kernel writes completions into it asynchronously.

A wait interrupted by a signal handler fails with `EINTR` if no event was reaped. It is never restarted, even with `SA_RESTART` (signal(7)). The sibling `io_pgetevents` adds a signal mask and is refused by the same predicate.

A context is a kernel object owned by the caller's address space (its `mm`), not by a file descriptor. A forked child does not inherit it: the child's lookup fails with `EINVAL`. All contexts are destroyed when the address space goes away, at exit or `execve`. A kernel built without `CONFIG_AIO` returns `ENOSYS` for every `io_*` call.

### Flags and options

- F1: `min_nr` and `nr`: wait for at least `min_nr` events and return at most `nr`. `min_nr` negative or greater than `nr` gives `EINVAL`.
- F2: `timeout == NULL`: wait without limit until `min_nr` events are available
- F3: `timeout` set: return when `min_nr` events are available or the relative timeout expires, possibly with fewer events or none. A zero timeout polls without waiting.
- F4: a signal handler interrupts the wait: `EINTR` when no event was reaped; never restarted
- F5: errors: `ctx_id` naming no live context: `EINVAL`. `events` not writable or `timeout` not readable: `EFAULT`. Kernel without `CONFIG_AIO`: `ENOSYS`.
- F6: event contents and order: `data`, `obj`, `res`, `res2` per event, in completion order

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Which requests have completed when the call looks, and in what order, depends on device and kernel timing outside the guest. A timeout turns that timing into a different return count. The call repeats only when every request has already completed before it is called and `min_nr` asks for no more than that.

### Nondeterminism sources

- Completion order and timing of the underlying I/O
- Real elapsed time against `timeout`
- Signal timing (`EINTR`)
- Asynchronous kernel writes into the ring mapped in guest memory

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_AIO`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. The supportable fallback is synchronous or thread-based I/O: glibc's POSIX AIO (`aio_read`, `aio_write`) runs requests on user-space threads and never calls the native AIO syscalls, and callers of native AIO such as MySQL InnoDB switch to their own simulated AIO when `io_setup` fails (recalled, not re-checked in the InnoDB source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `io_getevents` as Determinized (`detcore/src/syscall_classification.rs:407`, in the arm that ends at `detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) names it at `detcore/src/syscall_classification.rs:1058`.
- The guard arm in `handle_syscall_event` (`detcore/src/lib.rs:2237-2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every syscall that predicate names. The arm never injects the call, so the host kernel never sees it, whatever the arguments are, and no argument is validated first.
- The arm does not consult `panic_on_unsupported_syscalls`, so fail-closed mode and the compatibility opt-out return the same `ENOSYS` (`refused_only_when_fail_closed: false`).
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`), so backends that consult that boundary see the same refusal.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`, batch-7 loop at `detcore/src/syscall_classification.rs:2139-2171`) asserts the classification and the predicate membership. It does not launch Hermit, so it is not coverage for the flags below.

The doc comment (`detcore/src/syscall_classification.rs:1034-1047`) gives the reason: Linux native AIO completion is driven by the kernel and the device, outside the guest's logical time. Forwarding these calls, as the legacy pass-through did, is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_AIO` returns, so a guest sees a consistent, if smaller, kernel.

Why `refuse` and not the alternatives:

- `io_setup` is refused by the same predicate (`detcore/src/syscall_classification.rs:1054`), so a guest can never obtain a valid handle from inside the container. Any handle `io_getevents` receives is either garbage or a guess at a host object. Refusing `io_getevents` as well keeps the "kernel without `CONFIG_AIO`" picture consistent and keeps a guessed handle from reaching the host.
- `model` would need Detcore-owned contexts at a deterministic address, every request executed at a deterministic point (for example synchronously inside `io_submit`), completions queued in a deterministic order, and `io_getevents` waits and timeouts owned by the scheduler and virtual time. None of that exists at this SHA. It stays a possible later upgrade if a workload needs this interface; it is not today's target.
- `emulate` (for example, returning success without doing anything) would silently lose data or hand back invented state, which is worse than a clear `ENOSYS`.
- `passthrough` reintroduces the host-dependent behaviour described under Determinism analysis, and the isolation hole.

Under `refuse`, every behaviour in `### Flags and options` is replaced by the same `-1`/`ENOSYS` with no side effect. The flags inventory still lists the Linux behaviours, because a test of the refusal must show that no argument shape gets past it, and because a later `model` would have to implement them.

## Recommended tests

- T1 (covers F1, F2, F3, F5, F6): under `hermit run --verify` on ptrace, call raw `syscall(SYS_io_getevents, ...)` on a guessed handle with: `min_nr` 0 and `nr` 0, `min_nr` greater than `nr`, `min_nr` -1, a `NULL` timeout, a zero timeout, a 1-second timeout, and `NULL` `events`. Every call must return -1 with `errno == ENOSYS` at once (the `NULL`-timeout case must not hang), the `events` buffer must be unchanged, and both runs must print the same output.
  Existing candidate: `tests/c/aio_refusal.c` (cell `c-programs/aio-refusal`, `tests/e2e/manifests/c-programs.yaml:10929`, verify mode with ci on ptrace). It calls one shape, `min_nr` 0, `nr` 0, `NULL` events and timeout (`tests/c/aio_refusal.c:70`).
- T2 (covers F2, F4): with a signal handler installed and a timer armed, a `NULL`-timeout `io_getevents` returns `ENOSYS` before the timer fires and never `EINTR`, because no thread waits in the kernel.
- T3 (covers none): the same `ENOSYS` in fail-closed mode and under the compatibility opt-out, and a record followed by a replay reproduces it.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/613: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (detcore/src/lib.rs:2237-2241 returns a fixed ENOSYS for the native AIO family without forwarding); linux_determinism stays nondeterministic; 6 flag behaviours and 3 tests listed; tests/c/aio_refusal.c calls io_getevents with one argument shape, a candidate for T1 only, so no coverage token yet and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_AIO, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no open questions to remove

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_getevents`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_getevents&body=Discussion+for+the+%60io_getevents%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_getevents.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_getevents%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: io_getevents" (https://github.com/rrnewton/hermit/issues/613), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### P3 review notes (2026-10-03, hermit f4055eb9cae4)

- The legacy recommended action **NEEDS-HANDLER** predates the deterministic `ENOSYS` refusal whose code is tagged `TODO-HUMAN-REVIEW(#731)` (https://github.com/rrnewton/hermit/issues/731). At this SHA the native AIO family is refused by `is_unsupported_async_ipc_syscall`, so the target is `refuse`, not a new handler.
- A later upgrade to `model` remains possible if a workload needs this interface; Hermit design lists what it would need.
