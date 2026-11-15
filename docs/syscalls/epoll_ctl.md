---
schema: syscall-pages/v1
syscall: epoll_ctl
nr_x86_64: 233
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_ctl&body=Discussion+for+the+%60epoll_ctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_ctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_ctl%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`epoll_ctl(epfd, op, fd, event)` adds, changes or removes the registration of descriptor `fd` on the epoll instance `epfd`. Detcore handles it (`detcore/src/lib.rs:2623`, `handle_epoll_ctl` at `detcore/src/syscalls/io.rs:1176`): it takes an empty scheduler turn, so the mutation happens at a deterministic point of the schedule, and then forwards the call through `record_or_replay`. The kernel keeps the interest list.

Manual reference: Linux man-pages epoll_ctl(2).

## Linux semantics

The interest list is keyed by the pair of open file description and descriptor number. A registration stays in place until it is deleted or until every descriptor referring to that open file description is closed, so a registration made through one descriptor survives closing it while a `dup` copy remains. Registering a descriptor that is already ready makes it reportable at the next `epoll_wait`. A `MOD` replaces the event mask and `data`, and re-arms a disabled `EPOLLONESHOT` registration.

Checks measured on the host (Linux 7.1.3, see Additional information): a second `ADD` gives `EEXIST`; `MOD` or `DEL` of an unregistered descriptor gives `ENOENT`; a regular file gives `EPERM`, because it does not support polling; `fd == epfd`, an `epfd` that is not an epoll descriptor, and an unknown `op` give `EINVAL`; a closed `fd` gives `EBADF`; `ADD` with a NULL `event` gives `EFAULT`. `EPOLLEXCLUSIVE` is accepted only on `ADD`: with `MOD`, combined with `EPOLLONESHOT`, or as a later `MOD` of an exclusive registration, the call gives `EINVAL`. `EPOLLWAKEUP` without `CAP_BLOCK_SUSPEND` is silently dropped and the call returns 0. Per the manual page, adding an epoll descriptor that would create a cycle or nest more than five levels deep gives `ELOOP`, and exceeding `/proc/sys/fs/epoll/max_user_watches`, a per-user limit shared by all of the user's processes, gives `ENOSPC`.

### Flags and options

- F1: `EPOLL_CTL_ADD`: register `fd` with `event`; `EEXIST` if already registered. A descriptor that is already ready becomes reportable at once.
- F2: `EPOLL_CTL_MOD`: replace the mask and `data` of an existing registration and re-arm `EPOLLONESHOT`; `ENOENT` if not registered.
- F3: `EPOLL_CTL_DEL`: remove the registration; `ENOENT` if not registered; `event` is ignored and may be NULL.
- F4: any other `op`: `EINVAL`.
- F5: event bits `EPOLLIN`, `EPOLLOUT`, `EPOLLRDHUP`, `EPOLLPRI`, `EPOLLERR`, `EPOLLHUP` and the `data` value; `EPOLLERR` and `EPOLLHUP` are always reported.
- F6: `EPOLLET`: edge-triggered reporting.
- F7: `EPOLLONESHOT`: disable after one report until `MOD`.
- F8: `EPOLLWAKEUP`: needs `CAP_BLOCK_SUSPEND`; without it the bit is silently dropped.
- F9: `EPOLLEXCLUSIVE`: `ADD` only; `EINVAL` with `MOD`, with `EPOLLONESHOT`, or for a `MOD` of an exclusive registration.
- F10: descriptor errors: `EBADF` for a closed `epfd` or `fd`; `EINVAL` when `epfd` is not an epoll descriptor or `fd == epfd`; `EPERM` when `fd` does not support polling (regular files, directories).
- F11: nested epoll descriptors: allowed, but `ELOOP` for a cycle or for nesting deeper than the kernel limit.
- F12: `event` NULL or unreadable for `ADD` or `MOD`: `EFAULT`.
- F13: resource limits: `ENOSPC` when `max_user_watches` is reached, `ENOMEM` when the kernel cannot allocate.
- F14: registration identity: keyed by open file description and descriptor number; it survives closing `fd` while a `dup` copy of the same open file description remains.

## Determinism analysis

`linux_determinism: conditional`, kept from the migration placeholder. The result depends only on the operation, the event payload, the state of the interest list and the descriptor table, which repeat when the program's own actions repeat. Three things can still vary: another thread closing or re-registering the same descriptor concurrently (which call wins), the per-user `max_user_watches` limit, which counts the registrations of every process of the same user on the host (F13), and the caller's `CAP_BLOCK_SUSPEND` (F8).

### Nondeterminism sources

- Concurrent `epoll_ctl` and `close` on the same descriptor from different threads
- The host-wide per-user watch count behind `ENOSPC`
- Capabilities, which decide whether `EPOLLWAKEUP` is kept
- Readiness of the descriptor at the moment of `ADD` or `MOD`, which depends on producer timing (observed through `epoll_wait`)

## Hermit design

Target policy: `model`. The kernel stays the store for the interest list, and Detcore decides when each mutation happens. `handle_epoll_ctl` makes an empty resource request (`detcore/src/syscalls/io.rs:1182`), which gives the scheduler a turn so a concurrent `close` or second `epoll_ctl` lands in schedule order, and then forwards the unchanged call with `record_or_replay` (`detcore/src/syscalls/io.rs:1183`). Its doc comment (`detcore/src/syscalls/io.rs:1171`) states the intended argument: strict execution serializes the mutation, so the result depends only on the operation, the payload and deterministically reconstructed descriptor state, and record/replay rebuilds the same state by reissuing the same control operations. Guest descriptor numbers are deterministic, so the key of F14 repeats, and every kernel check (F1 to F12) is kept exactly as Linux makes it.

Why `model` and not the alternatives: plain forwarding without the turn would leave a race between threads to the host; `emulate` would duplicate the kernel's readiness machinery, which `epoll_wait` relies on; `refuse` would break event loops.

What remains: `ENOSPC` (F13) depends on the host user's total watch count and `EPOLLWAKEUP` (F8) on the capabilities Hermit runs with; neither is virtualized, and no test at this SHA exercises them. Both are rare in practice, and Hermit accepts them as host state it does not control.

## Recommended tests

- T1 (covers F1, F2, F3, F5, F6, F7, F11): `ADD`, `MOD` and `DEL` on pipes, a socketpair, a timerfd, a signalfd and an eventfd, with level-triggered, `EPOLLET` and `EPOLLONESHOT` registrations, a `MOD` re-arm and a nested epoll instance, under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/epoll_determinism.c` (cell `c-programs/epoll-determinism`, `tests/e2e/manifests/c-programs.yaml:1318`, verify ci on ptrace), which uses `EPOLLET` (line 188), `EPOLLONESHOT` with a `MOD` re-arm (lines 211 and 218), nesting (lines 308 and 309) and `DEL` with a NULL event (line 387).
- T2 (covers F1, F3): register, observe, delete with a NULL event, observe empty.
  Existing candidate: `tests/c/epoll_readiness.c` (cell `c-programs/epoll-readiness`, `tests/e2e/manifests/c-programs.yaml:11508`, verify ci true with ptrace enabled).
- T3 (covers F4, F9, F10, F12): raw calls reproducing the host probe under Additional information: second `ADD`, `MOD`/`DEL` of an unregistered descriptor, a regular file, `fd == epfd`, a pipe as `epfd`, fd 999, op 7, `ADD` with NULL, and the three `EPOLLEXCLUSIVE` refusals; results must match the native values.
- T4 (covers F11): a cycle of two epoll instances and a chain six levels deep must give `ELOOP`.
- T5 (covers F8, F13): `EPOLLWAKEUP` without `CAP_BLOCK_SUSPEND` returns 0 and the bit is absent from reported events; with `max_user_watches` lowered in a test user namespace or VM, the next `ADD` gives `ENOSPC`. Record the result; this documents host-dependent behaviour rather than gating it.
- T6 (covers F14): register through `fd`, `dup` it, close the original, and check that events are still reported; close the copy and check that the registration is gone. Repeat with two threads racing `close` against `EPOLL_CTL_ADD`; the outcome must be identical across `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (empty scheduler turn, then the unchanged call through record_or_replay; the kernel keeps the interest list); linux_determinism stays conditional; 14 flag behaviours and 6 tests listed; no coverage token because no test was run against this list, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/epoll_readiness.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `epoll_ctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_ctl&body=Discussion+for+the+%60epoll_ctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_ctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_ctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `epoll_ctl`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### 2026-10-03 host probe

Host: Linux 7.1.3 x86_64, unprivileged. A C program called the glibc wrapper on one epoll instance. Results: `ADD` pipe → 0; second `ADD` → `EEXIST`; `MOD` and `DEL` of an unregistered pipe → `ENOENT`; `ADD` of `/etc/hostname` → `EPERM`; `ADD` of the epoll fd to itself → `EINVAL`; pipe as `epfd` → `EINVAL`; fd 999 → `EBADF`; op 7 → `EINVAL`; `ADD` with NULL event → `EFAULT`; `MOD` with `EPOLLEXCLUSIVE` → `EINVAL`; `ADD` with `EPOLLEXCLUSIVE|EPOLLONESHOT` → `EINVAL`; `ADD` with `EPOLLEXCLUSIVE` → 0, then `MOD` of it → `EINVAL`; `ADD` with `EPOLLWAKEUP` → 0.
