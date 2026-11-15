---
schema: syscall-pages/v1
syscall: io_pgetevents
nr_x86_64: 333
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/614
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_pgetevents&body=Discussion+for+the+%60io_pgetevents%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_pgetevents.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_pgetevents%60%2C+or+in+what+the+page+says%3F%0A
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

`io_pgetevents(ctx_id, min_nr, nr, events, timeout, usig)` collects completion events from a Linux native AIO context, like `io_getevents`, and can install a temporary signal mask for the wait, as `ppoll` and `pselect6` do. Detcore refuses it with a fixed `ENOSYS` before the kernel sees it (`detcore/src/lib.rs:2237-2241`), as it refuses all of native AIO; that is what a kernel built without AIO support returns.

## Linux semantics

An AIO context is created by `io_setup`, which returns `ctx_id`, and requests are queued with `io_submit`. The kernel completes those requests asynchronously, in an order and at times set by the device, the filesystem and host scheduling, and places one `struct io_event` per completion in the context's ring. `io_pgetevents` waits until at least `min_nr` completions are available, the timeout expires or a signal arrives, then copies up to `nr` events into `events` and returns how many it copied.

`usig` points to a `struct __aio_sigset` holding a signal-mask pointer and its size. When the mask pointer is not NULL, the kernel installs that mask for the duration of the wait and restores the old one afterwards, atomically, so a signal blocked outside the call can be delivered only while it waits. A signal ends the wait with `EINTR`; the call is not restarted.

### Flags and options

- F1: `min_nr` and `nr`: wait for at least `min_nr` completions, return at most `nr`. `nr < 0` or `min_nr > nr` gives `EINVAL`; `min_nr == 0` returns whatever is available without waiting for more.
- F2: `timeout` non-NULL: a relative `timespec` bounding the wait; on expiry the call returns the number of events collected so far, possibly 0. A zero timeout polls. An invalid `timespec` gives `EINVAL`.
- F3: `timeout` NULL: wait without a bound until `min_nr` events are available.
- F4: `usig` non-NULL with a non-NULL mask pointer: install that signal mask for the wait. A `sigsetsize` other than `sizeof(sigset_t)` (8 on x86_64) gives `EINVAL`.
- F5: `usig` NULL, or a `__aio_sigset` whose mask pointer is NULL: no mask change; the call behaves like `io_getevents`.
- F6: completion contents and order: each returned `io_event` carries the request's `data`, its `iocb` pointer and its result, in the order the kernel completed them.
- F7: errors: `EINVAL` for an unknown or destroyed `ctx_id`; `EFAULT` for a bad `events`, `timeout` or `usig` pointer.
- F8: a signal during the wait: `EINTR`, never restarted.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. The number of events available, their order and the moment they become available are set by asynchronous kernel and device work that runs outside the caller's control, and the timeout is measured on the host clock. Two runs of the same program can collect different numbers of events, in a different order, from the same submissions. Signal timing decides whether the wait ends with `EINTR`, and the temporary mask (F4) changes which signals can end it.

### Nondeterminism sources

- Asynchronous completion of AIO requests by the kernel and the device
- The timeout, compared against the host clock
- Signal timing during the wait, under the temporary mask

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_AIO`, returned before any argument is examined, so a well-formed call and a garbage-argument call get the same `ENOSYS`. No unprivileged success case needs preserving, so decision 6's exceptions do not apply. The supportable fallback is synchronous or thread-based I/O: glibc's POSIX AIO (`aio_read`, `aio_write`) runs requests on user-space threads and never calls the native AIO syscalls, and callers of native AIO such as MySQL InnoDB switch to their own simulated AIO when `io_setup` fails (recalled, not re-checked in the InnoDB source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2237-2241`), so no code change is needed:

- `classify_syscall` lists `io_pgetevents` (`detcore/src/syscall_classification.rs:408`) in the arm that returns `SyscallClassification::Determinized` (`detcore/src/syscall_classification.rs:688`).
- `is_unsupported_async_ipc_syscall` includes it (`detcore/src/syscall_classification.rs:1059`), with `io_setup` (`detcore/src/syscall_classification.rs:1054`) and the other native AIO calls.
- In `handle_syscall_event`, the match on the classification (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2237-2241`, which returns `Err(Error::Errno(Errno::ENOSYS))` without injecting the syscall. The host kernel never runs it, so the guest never waits on host AIO completions and no signal mask is changed. The arm does not consult `panic_on_unsupported_syscalls`, so the result is the same with or without fail-closed mode.
- `is_deterministically_refused_syscall` includes the predicate (`detcore/src/syscall_classification.rs:1325`).

Why `refuse` and not the alternatives:

- `io_setup` is refused (`detcore/src/syscall_classification.rs:1054`), so no guest can hold a valid `ctx_id` under Hermit. Refusing `io_pgetevents` as well keeps native AIO consistently absent.
- `passthrough` would expose completion timing and order, which the predicate's doc comment (`detcore/src/syscall_classification.rs:1034-1047`) describes as kernel-driven and outside the guest's logical time.
- `model` would need Detcore to run AIO requests itself and deliver completions at deterministic points in virtual time. That is a feature for the whole AIO family, not a handler for this one call. The legacy NEEDS-HANDLER recommendation predates this family-wide refusal.
- `ENOSYS` is what a kernel built without AIO support returns. A program that uses native AIO meets the refusal first at `io_setup`, where it can treat AIO as unavailable.

Guest-visible difference from Linux: F1-F8 all return `ENOSYS`, including the `EINVAL` and `EFAULT` cases, because the refusal precedes every check. Nothing is written to `events`. This is the intended refusal.

Existing test evidence at f4055eb9cae4: the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts, in its loop at `detcore/src/syscall_classification.rs:2143`, that `io_pgetevents` classifies as Determinized and is in the predicate. It does not run a guest and does not check the errno. The guest-level contract `tests/c/aio_refusal.c` (cell `c-programs/aio-refusal`, `tests/e2e/manifests/c-programs.yaml:10929`, verify with ptrace in CI) checks that `io_setup`, `io_submit`, `io_getevents` and `io_destroy` return `ENOSYS`, but it does not call `io_pgetevents`, so it is not coverage for this syscall.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8): extend `tests/c/aio_refusal.c` (or add a sibling) with raw `syscall(SYS_io_pgetevents, ...)` calls under `hermit run --verify` on ptrace: `ctx_id` 0 and an arbitrary value; `min_nr` greater than `nr` and `nr` negative; a NULL, a zero and an invalid `timeout`; a NULL `usig`, a `usig` with a NULL mask pointer, and a `usig` with a mask and a wrong `sigsetsize`; and a bad `events` pointer. Each call must return -1 with `errno == ENOSYS`, the `events` buffer must keep its sentinel bytes, and the output must be identical across both runs.
- T2 (covers F4, F8): with `SIGUSR1` blocked and pending, a raw call whose `usig` mask unblocks `SIGUSR1` must return `ENOSYS` at once; the handler must not run during the call, and `SIGUSR1` must still be blocked and pending afterwards.
- T3 (covers none): record and replay a guest that calls `io_pgetevents`; replay must produce the same `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/614: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns a fixed ENOSYS for all native AIO at detcore/src/lib.rs:2237-2241, the result of a kernel without AIO; io_setup is refused too, so no guest holds a ctx_id); linux_determinism stays nondeterministic; 8 flag behaviours and 3 tests listed; tests/c/aio_refusal.c does not call io_pgetevents, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_AIO, before any argument check, already realized at detcore/src/lib.rs:2237-2241); target_policy stays refuse; no new F/T ids; no open questions to remove

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_pgetevents`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_pgetevents&body=Discussion+for+the+%60io_pgetevents%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_pgetevents.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_pgetevents%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: io_pgetevents" (https://github.com/rrnewton/hermit/issues/614), legacy variant B.

The legacy Summary asked to "replace its temporary catch-all policy with an explicit Detcore classification". That is done at f4055eb9cae4: the call has an explicit classification and dispatch arm (see Hermit design).

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
