---
schema: syscall-pages/v1
syscall: io_submit
nr_x86_64: 209
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/616
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_submit&body=Discussion+for+the+%60io_submit%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_submit.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_submit%60%2C+or+in+what+the+page+says%3F%0A
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

`io_submit(ctx_id, nr, iocbpp)` queues `nr` asynchronous I/O requests, described by `struct iocb`s, on a Linux native AIO context. Hermit refuses it: Detcore returns a fixed `ENOSYS` without forwarding the call (`detcore/src/lib.rs:2237-2241`), as on a kernel built without `CONFIG_AIO`.

Manual reference: Linux man-pages io_submit(2).

## Linux semantics

Each `iocb` names an operation (`aio_lio_opcode`), a file descriptor, a buffer, a length, an offset, a user cookie (`aio_data`), and optional flags. The kernel writes a key into each `iocb` (used later by `io_cancel`) and starts the request.

When the request finishes, the kernel writes an `io_event` into the context's ring. On most filesystems, buffered reads and writes run to completion inside `io_submit`, while `O_DIRECT` I/O and poll requests complete later, in an order the device decides. The data a write puts in the file, and the data a read puts in the buffer, become visible at that completion.

The call returns the number of requests it accepted, which can be fewer than `nr`. It fails with the first request's error only if no request was accepted.

A context is a kernel object owned by the caller's address space (its `mm`), not by a file descriptor. A forked child does not inherit it: the child's lookup fails with `EINVAL`. All contexts are destroyed when the address space goes away, at exit or `execve`. A kernel built without `CONFIG_AIO` returns `ENOSYS` for every `io_*` call.

### Flags and options

- F1: `IOCB_CMD_PREAD` and `IOCB_CMD_PWRITE`: read or write `aio_nbytes` bytes at `aio_offset` with the buffer at `aio_buf`
- F2: `IOCB_CMD_PREADV` and `IOCB_CMD_PWRITEV`: the same with an `iovec` array
- F3: `IOCB_CMD_FSYNC` and `IOCB_CMD_FDSYNC`: flush the file asynchronously. A file without fsync support gives `EINVAL`.
- F4: `IOCB_CMD_POLL` (Linux 4.18): a one-shot poll for the events in `aio_buf`, completing when the descriptor is ready; it can be cancelled
- F5: unknown opcode, including the defined but unimplemented `IOCB_CMD_NOOP`: `EINVAL`
- F6: `IOCB_FLAG_RESFD`: signal the eventfd `aio_resfd` when the request completes. A bad descriptor gives `EBADF`; one that is not an eventfd gives `EINVAL`.
- F7: `IOCB_FLAG_IOPRIO`: use `aio_reqprio` as the request's I/O priority. An invalid value gives `EINVAL`; a privileged class without the capability gives `EPERM`.
- F8: `aio_rw_flags` (Linux 4.13): per-request `RWF_*` flags as for `preadv2`/`pwritev2` (`RWF_HIPRI`, `RWF_DSYNC`, `RWF_SYNC`, `RWF_NOWAIT`, `RWF_APPEND`). An unsupported flag gives `EOPNOTSUPP`.
- F9: batch semantics: `nr == 0` returns 0; `nr < 0` gives `EINVAL`; `nr` beyond the context's capacity is cut down to it; the return value counts accepted requests, and the first request's error is returned only if none was accepted
- F10: errors: `ctx_id` naming no live context: `EINVAL`. Nonzero reserved fields, or a buffer range that overflows: `EINVAL`. A closed descriptor, or one not opened for the requested direction: `EBADF`. `iocbpp` or an `iocb` not readable: `EFAULT`. No free request slots: `EAGAIN`.
- F11: completion: each accepted request later produces one `io_event` (`data`, `obj`, `res`, `res2`) in the ring, in completion order; buffered I/O on most filesystems completes inside the call, `O_DIRECT` and poll requests complete later

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Requests that complete after `io_submit` returns finish at times and in an order set by the device and the kernel, outside the guest's schedule. The file and buffer contents a request changes become visible at that moment, so a guest thread reading them races the device. How many requests a batch accepts (`EAGAIN`) depends on free kernel resources. The call repeats only for requests that run to completion inside the call, such as buffered I/O on most filesystems, and even that depends on the filesystem.

### Nondeterminism sources

- Asynchronous completion order and timing (`O_DIRECT`, poll, device latency)
- Guest-visible memory and file contents written at completion time
- Filesystem-dependent choice between synchronous and asynchronous execution
- Free request slots and host memory, which decide partial acceptance

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_AIO`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. The supportable fallback is synchronous or thread-based I/O: glibc's POSIX AIO (`aio_read`, `aio_write`) runs requests on user-space threads and never calls the native AIO syscalls, and callers of native AIO such as MySQL InnoDB switch to their own simulated AIO when `io_setup` fails (recalled, not re-checked in the InnoDB source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `io_submit` as Determinized (`detcore/src/syscall_classification.rs:405`, in the arm that ends at `detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) names it at `detcore/src/syscall_classification.rs:1056`.
- The guard arm in `handle_syscall_event` (`detcore/src/lib.rs:2237-2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every syscall that predicate names. The arm never injects the call, so the host kernel never sees it, whatever the arguments are, and no argument is validated first.
- The arm does not consult `panic_on_unsupported_syscalls`, so fail-closed mode and the compatibility opt-out return the same `ENOSYS` (`refused_only_when_fail_closed: false`).
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`), so backends that consult that boundary see the same refusal.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`, batch-7 loop at `detcore/src/syscall_classification.rs:2139-2171`) asserts the classification and the predicate membership. It does not launch Hermit, so it is not coverage for the flags below.

The doc comment (`detcore/src/syscall_classification.rs:1034-1047`) gives the reason: Linux native AIO completion is driven by the kernel and the device, outside the guest's logical time. Forwarding these calls, as the legacy pass-through did, is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_AIO` returns, so a guest sees a consistent, if smaller, kernel.

Why `refuse` and not the alternatives:

- `io_setup` is refused by the same predicate (`detcore/src/syscall_classification.rs:1054`), so a guest can never obtain a valid handle from inside the container. Any handle `io_submit` receives is either garbage or a guess at a host object. Refusing `io_submit` as well keeps the "kernel without `CONFIG_AIO`" picture consistent and keeps a guessed handle from reaching the host.
- `model` would need Detcore-owned contexts at a deterministic address, every request executed at a deterministic point (for example synchronously inside `io_submit`), completions queued in a deterministic order, and `io_getevents` waits and timeouts owned by the scheduler and virtual time. None of that exists at this SHA. It stays a possible later upgrade if a workload needs this interface; it is not today's target.
- `emulate` (for example, returning success without doing anything) would silently lose data or hand back invented state, which is worse than a clear `ENOSYS`.
- `passthrough` reintroduces the host-dependent behaviour described under Determinism analysis, and the isolation hole.

Under `refuse`, every behaviour in `### Flags and options` is replaced by the same `-1`/`ENOSYS` with no side effect. The flags inventory still lists the Linux behaviours, because a test of the refusal must show that no argument shape gets past it, and because a later `model` would have to implement them.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8, F9, F10): under `hermit run --verify` on ptrace, call raw `syscall(SYS_io_submit, ...)` on a guessed handle with `nr` 0, `nr` -1, a `NULL` `iocbpp`, and with one `iocb` per opcode (`PREAD`, `PWRITE`, `PREADV`, `PWRITEV`, `FSYNC`, `FDSYNC`, `POLL`, `NOOP`, and an unknown value), one with `IOCB_FLAG_RESFD` on an eventfd, one with `IOCB_FLAG_IOPRIO`, and one with `aio_rw_flags = RWF_NOWAIT`. Every call must return -1 with `errno == ENOSYS`, and both runs must print the same output.
  Existing candidate: `tests/c/aio_refusal.c` (cell `c-programs/aio-refusal`, `tests/e2e/manifests/c-programs.yaml:10929`, verify mode with ci on ptrace). It calls one shape, `nr` 0 with a `NULL` `iocbpp` (`tests/c/aio_refusal.c:56`).
- T2 (covers F1, F6, F11): the refusal has no side effect. An `iocb` that writes a known pattern to a file and names an eventfd leaves the file unchanged, the `aio_key` field untouched, and the eventfd counter at 0.
- T3 (covers none): the same `ENOSYS` in fail-closed mode and under the compatibility opt-out, and a record followed by a replay reproduces it.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/616: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (detcore/src/lib.rs:2237-2241 returns a fixed ENOSYS for the native AIO family without forwarding); linux_determinism stays nondeterministic; 11 flag behaviours and 3 tests listed; tests/c/aio_refusal.c calls io_submit with one argument shape, a candidate for T1 only, so no coverage token yet and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_AIO, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no open questions to remove

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_submit`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_submit&body=Discussion+for+the+%60io_submit%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_submit.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_submit%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: io_submit" (https://github.com/rrnewton/hermit/issues/616), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### P3 review notes (2026-10-03, hermit f4055eb9cae4)

- The legacy recommended action **NEEDS-HANDLER** predates the deterministic `ENOSYS` refusal whose code is tagged `TODO-HUMAN-REVIEW(#731)` (https://github.com/rrnewton/hermit/issues/731). At this SHA the native AIO family is refused by `is_unsupported_async_ipc_syscall`, so the target is `refuse`, not a new handler.
- A later upgrade to `model` remains possible if a workload needs this interface; Hermit design lists what it would need.
