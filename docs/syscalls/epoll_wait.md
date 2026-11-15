---
schema: syscall-pages/v1
syscall: epoll_wait
nr_x86_64: 232
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_wait&body=Discussion+for+the+%60epoll_wait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_wait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_wait%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`epoll_wait(epfd, events, maxevents, timeout)` waits until at least one descriptor registered on the epoll instance `epfd` is ready, or until `timeout` milliseconds pass, and copies up to `maxevents` ready events to `events`. Detcore handles it (`detcore/src/lib.rs:2625`, `handle_epoll_wait` at `detcore/src/syscalls/io.rs:1295`): under sequentialized threads it never lets the kernel block, but retries a zero-timeout probe at scheduler turns against a deadline in virtual time; in record/replay or unsequentialized runs it is recorded as a blocking external operation.

Manual reference: Linux man-pages epoll_wait(2).

## Linux semantics

The call returns the number of events copied, 0 when the timeout expires with nothing ready, or -1 with `errno`. Each returned `struct epoll_event` carries the readiness bits and the `data` value given at registration, unchanged. `EPOLLERR` and `EPOLLHUP` are reported whether or not they were requested.

Readiness follows the registration mode set by `epoll_ctl`. A level-triggered descriptor is reported on every call while it stays ready; an `EPOLLET` descriptor is reported once per transition to ready; an `EPOLLONESHOT` descriptor is disabled after one report until `EPOLL_CTL_MOD` re-arms it. Events come off the instance's ready list in the order the descriptors became ready; when more are ready than `maxevents`, the rest stay queued and the reported ones move to the back of the list, so successive calls rotate through them. An epoll descriptor registered in another epoll set is itself readable when its own set has events.

Argument checks, measured on the host (Linux 7.1.3, see Additional information): `maxevents` of 0, -1 or `INT_MAX` gives `EINVAL`; a closed `epfd` gives `EBADF`; an `epfd` that is not an epoll descriptor gives `EINVAL`; an unwritable `events` gives `EFAULT` once an event has to be copied. A signal handler interrupts the wait with `EINTR`, and the call is never restarted, even with `SA_RESTART` (signal(7)).

### Flags and options

- F1: `timeout < 0`: wait with no deadline until an event is ready or a signal arrives (the host probe got 1 for `-5` with a ready descriptor).
- F2: `timeout == 0`: never block; return the ready events, or 0.
- F3: `timeout > 0`: wait at most that many milliseconds, rounded up to clock granularity; return 0 on expiry.
- F4: `maxevents <= 0`, or larger than `INT_MAX / sizeof(struct epoll_event)`: `EINVAL`.
- F5: `epfd` not open: `EBADF`; `epfd` open but not an epoll descriptor: `EINVAL`.
- F6: `events` not writable: `EFAULT`.
- F7: readiness reporting by registration mode: level-triggered, `EPOLLET` and `EPOLLONESHOT`; `EPOLLERR`/`EPOLLHUP` always reported; `data` returned verbatim.
- F8: result order and truncation: ready-list order, at most `maxevents`, with the remainder rotated to later calls.
- F9: a caught signal: `EINTR`, never restarted regardless of `SA_RESTART`.
- F10: nested instances: an epoll descriptor in the set reports readable when its own set has events.

## Determinism analysis

`linux_determinism: conditional`, kept from the migration placeholder. Given the same set of ready descriptors at the moment of the call, the result is fixed by the kernel's ready list. What is not fixed natively is which descriptors are ready when, because that depends on when other threads and processes write, when timers fire and when signals arrive; the order in which descriptors become ready also decides the order of returned events (F8). The call repeats when every producer feeding the watched descriptors is inside one deterministic schedule and no descriptor is connected to the outside world.

### Nondeterminism sources

- Timing of the writers, peers and timers that make watched descriptors ready, which also fixes ready-list order
- Timeout expiry against host time
- Signal arrival during the wait, which decides between an event count and `EINTR`
- Descriptors connected outside the guest (host sockets, terminals, files written by host processes)

## Hermit design

Target policy: `model`. Detcore owns the waiting, and the kernel only answers zero-timeout readiness probes:

- Under sequentialized threads in a plain run, `handle_internal_epoll_wait` (`detcore/src/syscalls/io.rs:1315`) turns any nonzero timeout into `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) with an `InternalIOPolling` resource (`detcore/src/syscalls/io.rs:1327`). Each attempt is the same call with timeout 0 (`detcore/src/syscalls/helpers.rs:1039`); a result of 0 means "would have blocked" and the thread retries at its next scheduler turn. A positive timeout becomes an absolute virtual deadline, and a non-positive one becomes no deadline (`positive_millis_as_nanos`, `detcore/src/syscalls/helpers.rs:1612`), which gives F1 and F3. At the deadline the call returns 0 (`detcore/src/syscalls/helpers.rs:1052`, check at `detcore/src/syscalls/helpers.rs:1505`).
- A signal that arrives while the thread waits ends the call with `EINTR` (`detcore/src/syscalls/helpers.rs:1477`; the `EpollWait` errno is pinned by the unit test `signal_interruption_errno_matches_linux_restart_policy` at `detcore/src/syscalls/helpers.rs:1686`), which matches F9.
- In record/replay modes a zero-timeout call takes an empty scheduler turn and is recorded (`detcore/src/syscalls/io.rs:1300`); other timeouts, and every call when threads are not sequentialized, go through `record_or_replay_blocking` (`detcore/src/syscalls/io.rs:1307`, helper at `detcore/src/syscalls/helpers.rs:111`), which takes the thread out of the run set while the kernel blocks.
- Argument errors (F4, F5, F6) come from the kernel on the first probe and end the loop, because only a result of 0 counts as "would have blocked".
- Readiness rules and event order (F7, F8, F10) are the kernel's; because every producer inside the guest runs in scheduler order, the ready list is built in the same order on every run.

Why `model` and not the alternatives: letting the kernel block a sequentialized thread would stall the whole schedule (that is what `record_or_replay_blocking` exists to avoid outside strict runs), and a host timeout would make expiry depend on wall time; `emulate` would need Detcore to reimplement readiness for every descriptor kind; `refuse` would break event loops.

Target (owner decisions 13, 14): a blocking wait is a zero-timeout probe plus a scheduler wait, and the timeout is measured on virtual time. Today's sequentialized-run path already has that form (`handle_internal_epoll_wait`, `detcore/src/syscalls/io.rs:1315`, deadline from `millis_duration_to_absolute_timeout` at `detcore/src/syscalls/io.rs:1324`). In record/replay and in runs without sequentialized threads, a nonzero timeout still waits in the kernel through `record_or_replay_blocking` (`detcore/src/syscalls/io.rs:1307`); that path releases the turn instead of holding it, but the thread rejoins at a host-chosen time. `epoll_wait` is too widely used to refuse, so no blocking form is refused.

What remains:

- **No scheduler turn for a zero timeout in a plain strict run.** `handle_internal_epoll_wait` injects a zero-timeout call directly (`detcore/src/syscalls/io.rs:1322`), while `handle_internal_poll` (`detcore/src/syscalls/io.rs:1108`) and `handle_internal_epoll_pwait` (`detcore/src/syscalls/io.rs:1259`) first take a `zero_timeout_poll_request` turn; the comment at `detcore/src/syscalls/io.rs:1248` calls omitting that turn a scheduling regression for `epoll_pwait`. A guest that spins on `epoll_wait(..., 0)` waiting for a sibling thread therefore relies on preemption alone to let the sibling run. This is from code reading; T7 would measure it.
- **Descriptors connected outside the guest.** Readiness of a host socket or terminal depends on when outside data arrives relative to the retry turns, so such a wait stays as nondeterministic as the outside world; record/replay is the mode that captures it.

## Recommended tests

- T1 (covers F2, F3, F7, F10): level-triggered, `EPOLLET` and `EPOLLONESHOT` registrations on pipes, a socketpair, a timerfd and a signalfd, a nested epoll instance, zero and 1000 ms timeouts, under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/epoll_determinism.c` (cell `c-programs/epoll-determinism`, `tests/e2e/manifests/c-programs.yaml:1318`, verify ci on ptrace).
- T2 (covers F2): register, observe ready with a zero timeout, deregister, observe empty.
  Existing candidate: `tests/c/epoll_readiness.c` (cell `c-programs/epoll-readiness`, `tests/e2e/manifests/c-programs.yaml:11508`, verify ci true with ptrace enabled).
- T3 (covers F1, F9): two writer threads feed a pipe and an eventfd while the main thread waits with timeout -1; a handler-installed signal (with `SA_RESTART`) interrupts one wait and must give `EINTR`.
  Existing partial candidate: mode `epoll` of `tests/c/ipc_determinism.c` (timeout -1 at line 361), but its cell `c-programs/ipc-determinism` (`tests/e2e/manifests/c-programs.yaml:2603`) passes only `pipe-order` on ptrace, so that mode does not run in CI. No test sends a signal during `epoll_wait`.
- T4 (covers F8): several descriptors become ready together and `maxevents` is smaller than their number; the returned order and the rotation across calls must be identical across runs.
  Existing partial candidate: `tests/c/socket_epoll_ordering.c` (cell `c-programs/socket-epoll-ordering`, `tests/e2e/manifests/c-programs.yaml:13718`, verify ci true with ptrace enabled), which orders accepted loopback connections but uses `maxevents` equal to the connection count.
- T5 (covers F4, F5, F6): raw calls with `maxevents` 0, -1 and `INT_MAX`, a closed `epfd`, a pipe as `epfd`, and `events` NULL with a ready descriptor; expect the errors recorded under Additional information.
- T6 (covers F3): a 50 ms timeout with nothing ready returns 0 after exactly 50 ms of virtual time, identical across runs, with no wall-clock sleep.
- T7 (covers F2): one thread spins on `epoll_wait(..., 0)` until a sibling writes a pipe; the run must finish and the iteration count must be identical across `--verify` runs. This is the `epoll_wait` counterpart of `tests/c/epoll_pwait_zero_timeout_progress.c`, which calls only `SYS_epoll_pwait`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (zero-timeout probes retried at scheduler turns against a virtual deadline; blocking record/replay path); linux_determinism stays conditional; 10 flag behaviours and 7 tests listed; gap from code reading: zero-timeout strict path takes no scheduler turn; tracking_status stays todo, with an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 13 and 14 applied (Target paragraph: probe-plus-wait on virtual time, already realized on the sequentialized path; record/replay still waits in the kernel); target_policy stays model; no new F/T ids; no questions removed (the zero-timeout turn question is not decided by 6-15)
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/epoll_readiness.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `epoll_wait`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_wait&body=Discussion+for+the+%60epoll_wait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_wait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_wait%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `epoll_wait`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should the plain strict zero-timeout path take a `zero_timeout_poll_request` turn like `poll` and `epoll_pwait`? The `epoll_pwait` comment treats the missing turn as a regression; for `epoll_wait` it has not been measured.

### 2026-10-03 host probe

Host: Linux 7.1.3 x86_64. A C program called the glibc wrapper (and once the raw syscall) on an epoll instance with one registered pipe. Results: `maxevents` 0, -1 and `INT_MAX` → `EINVAL`; pipe as `epfd` → `EINVAL`; fd 999 as `epfd` → `EBADF`; timeout 0 with nothing ready → 0; with the pipe readable, `events` NULL and `events` = 8 → `EFAULT`; timeout -5 → 1; raw `syscall(SYS_epoll_wait, ...)` → 1.
