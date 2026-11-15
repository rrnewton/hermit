---
schema: syscall-pages/v1
syscall: epoll_pwait
nr_x86_64: 281
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_pwait&body=Discussion+for+the+%60epoll_pwait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_pwait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_pwait%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 10
---

## Summary

`epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize)` waits for events on an epoll instance, as `epoll_wait` does, and optionally swaps in a temporary signal mask atomically for the wait. On x86_64, glibc's `epoll_wait()` uses the plain `epoll_wait` syscall. This one is reached by `epoll_pwait()` callers and by libraries that issue it directly, such as libuv.

Detcore models a NULL-mask call (`detcore/src/syscalls/io.rs:1187-1234`). Under sequentialized run it becomes zero-timeout probes against a virtual-time deadline. A non-NULL mask is still injected raw and blocks in the kernel while holding the scheduler turn.

Manual reference: Linux man-pages epoll_wait(2) (section "epoll_pwait"), epoll(7), signal(7).

## Linux semantics

The kernel works in this order:

1. install the temporary mask, if one is given
2. check `maxevents` and the `events` buffer
3. resolve `epfd`
4. wait until the ready list is non-empty, the timeout expires, or a signal handler runs

It then copies up to `maxevents` ready events out and restores the original mask. The return value is the number of events, or 0 on timeout. Which events are reported, and how often, follows the per-descriptor settings made with `epoll_ctl`. The timeout is in milliseconds and is never written back. Per signal(7), `epoll_pwait` is never restarted after a handler, even with `SA_RESTART`.

### Flags and options

- F1: `epfd`: a descriptor that is not open gives `EBADF`. A descriptor that is not an epoll instance gives `EINVAL`.
- F2: `maxevents`: must be greater than 0 and at most `EP_MAX_EVENTS` (`EINVAL` otherwise). The `events` buffer must be writable for `maxevents` entries (`EFAULT`).
- F3: `timeout < 0`: wait without a time limit.
- F4: `timeout == 0`: return immediately with whatever is ready.
- F5: `timeout > 0`: wait up to that many milliseconds, rounded up to the clock granularity. Nothing is written back.
- F6: `sigmask == NULL`: behaves exactly like `epoll_wait`.
- F7: `sigmask != NULL`: `sigsetsize` must be 8 (`EINVAL`). An unreadable mask gives `EFAULT`. The mask is applied atomically for the wait and restored on return.
- F8: ready-list semantics set by `epoll_ctl`:
  - level-triggered entries are reported while ready
  - `EPOLLET` entries are reported once per new edge
  - `EPOLLONESHOT` entries are disabled after one report
  - `EPOLLEXCLUSIVE` wakes one of several waiters
  - more ready entries than `maxevents` are returned across later calls in round-robin order
- F9: a signal handler runs during the wait: `EINTR`, never restarted even with `SA_RESTART`.

## Determinism analysis

`linux_determinism: nondeterministic` (changed from the migration's placeholder `conditional`). When an event becomes ready relative to the timeout, which events are reported in one call, their order, and whether a signal ends the wait all depend on host scheduling, peer activity and the real clock. The call repeats only for a zero timeout against an epoll set whose state is already settled.

### Nondeterminism sources

- Readiness timing of the watched descriptors, and ready-list order
- Timeout expiry against the real clock
- Signal arrival relative to the wait; the temporary mask decides which signals can interrupt it

## Hermit design

Target policy: `model`, matching `epoll_wait`. `handle_epoll_pwait` (`detcore/src/syscalls/io.rs:1187-1234`, dispatched at `detcore/src/lib.rs:2624`) chooses a path as follows.

- **Non-NULL mask** (`detcore/src/syscalls/io.rs:1217-1221`): Detcore makes an empty resource request (one scheduler turn), then calls `record_or_replay` on the unchanged call. With a non-zero timeout the thread blocks in the kernel while holding the turn. The comment at `detcore/src/syscalls/io.rs:1212-1216` calls this "a known remaining gap". It applies in sequentialized run and in record/replay.
- **NULL mask, record/replay, zero timeout** (`detcore/src/syscalls/io.rs:1222-1226`): the call takes a turn and is recorded.
- **NULL mask, unsequentialized run or record/replay with a timeout** (`detcore/src/syscalls/io.rs:1227-1230`): `record_or_replay_blocking`, so the host decides when the call completes.
- **NULL mask, sequentialized run** (`handle_internal_epoll_pwait`, `detcore/src/syscalls/io.rs:1238-1271`):
  - A zero timeout takes a `zero_timeout_poll_request` turn, so a polling loop cannot starve its producer (`detcore/src/syscalls/io.rs:1254-1262`), then injects the call.
  - Any other timeout runs `retry_nonblocking_syscall_with_timeout` under `InternalIOPolling` (`detcore/src/syscalls/io.rs:1264-1269`). The deadline comes from `millis_duration_to_absolute_timeout` (`detcore/src/syscalls/helpers.rs:1593-1601`). A negative timeout has no deadline (`positive_millis_as_nanos`, `detcore/src/syscalls/helpers.rs:1612-1614`).
  - The nonblocking form is defined at `detcore/src/syscalls/helpers.rs:1069`, with its rationale at `detcore/src/syscalls/helpers.rs:1058-1067`.

The comment at `detcore/src/syscalls/io.rs:1192-1200` records why this routing exists. `cmake` hung forever because libuv's `epoll_pwait(..., -1, NULL, 8)` blocked in the kernel while holding the only turn.

Why `model` and not the alternatives:

- `passthrough` is exactly the deadlock above.
- `emulate` cannot judge readiness without the kernel.
- `refuse` would break libuv, which is used by Node.js and cmake.

What remains before the code fully realizes `model`:

- **Masked waits** (F7, F9) can still deadlock a sequentialized guest, by the same mechanism as the documented `cmake` hang. This is a hypothesis from reading the code; no test exercises a blocking masked `epoll_pwait`. The masked path also passes the guest's mask through without clearing Hermit's perf-event signal bit. `ppoll` and `pselect6` clear it (`sanitize_ppoll_signal_mask`, `detcore/src/syscalls/io.rs:386-389`). Not measured.
- **Inconsistent mask policy across the family.** For a masked wait that would block:
  - `pselect6` carries the mask on every zero-timeout probe (`detcore/src/syscalls/io.rs:614-625`)
  - `ppoll` fails closed with `ENOSYS` (`detcore/src/syscalls/io.rs:1046-1048`)
  - `epoll_pwait` blocks in the kernel

Target (owner decisions 13, 14): every `epoll_pwait` whose wait could block is a zero-timeout probe plus a scheduler wait, with the timeout measured on virtual time. For a non-NULL mask, the snapshotted mask, with Hermit's perf-event signal bit cleared by `sanitize_ppoll_signal_mask`, rides on every zero-timeout probe so the kernel applies it atomically, as `pselect6` already does (`detcore/src/syscalls/io.rs:614-625`). The NULL-mask sequentialized path already has this form (`detcore/src/syscalls/io.rs:1238-1271`). The masked path does not: it takes one turn and then waits in the kernel holding it (`detcore/src/syscalls/io.rs:1217-1221`), the class of the documented `cmake` hang. `epoll_pwait` is used directly by libuv, so it is not refused while the masked probe is built.
- **Record mode with a timeout** uses `record_or_replay_blocking`, whose completion point the host decides. That is acceptable for record, because the outputs are recorded.

## Recommended tests

- T1 (covers F4, F6): a zero-timeout NULL-mask polling loop on an eventfd must let its worker run under `--strict --max-timeslice=disabled`. Existing candidates: `tests/c/epoll_pwait_zero_timeout_progress.c` (default mode), `zero_timeout_epoll_pwait_yields_without_timer_preemption` (`hermit-cli/tests/epoll_pwait_zero_timeout_progress.rs:145`), and `zero_timeout_epoll_pwait_ordinary_strict_smoke` (`hermit-cli/tests/epoll_pwait_zero_timeout_progress.rs:159`).
- T2 (covers F3, F6): an infinite-timeout NULL-mask wait, satisfied by a worker that sleeps 50 ms first, completes under plain `--strict`. Existing candidate: `run_blocking_mode` (`tests/c/epoll_pwait_zero_timeout_progress.c:133`) and `blocking_epoll_pwait_does_not_deadlock_the_scheduler` (`hermit-cli/tests/epoll_pwait_zero_timeout_progress.rs:181`). These tests run `hermit run --strict` without `--verify`, and no manifest cell names this program. Add a `c-programs` cell with verify on ptrace.
- T3 (covers F5): a 20 ms timeout on an empty epoll set returns 0 with output identical across `--verify` runs. No existing test.
- T4 (covers F1, F2): the error cases:
  - a closed `epfd` gives `EBADF`
  - a pipe `epfd` gives `EINVAL`
  - `maxevents = 0` gives `EINVAL`
  - an `events` pointer of 1 gives `EFAULT`
  No existing test.
- T5 (covers F7): masked calls:
  - `sigsetsize = 4` gives `EINVAL`
  - an inaccessible mask gives `EFAULT`
  - a masked call with a ready descriptor returns it
  - a masked infinite wait satisfied by a sleeping worker must complete under `--strict`; today this is expected to hang (hypothesis), so write it as the test for the fix
  No existing test.
- T6 (covers F8): level-triggered, `EPOLLET` and `EPOLLONESHOT` entries on pipes, with `maxevents = 1` across three ready descriptors. Assert the report sequence is identical across `--verify` runs. No existing test calls `epoll_pwait` this way.
- T7 (covers F9): a NULL-mask wait interrupted by a handled signal returns `EINTR` even with `SA_RESTART`, identically across `--verify` runs. No existing test.
- T8 (covers F4, F6, F7): record and replay of a zero-timeout call and a masked ready call reproduce the return values and `events` contents. No existing test.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (NULL-mask waits are zero-timeout probes against a virtual-time deadline); linux_determinism conditional -> nondeterministic (readiness, timeout and signal timing); masked waits still block in the kernel holding the scheduler turn, and the masked-wait policy differs from ppoll and pselect6, left as an open question for the owner; 9 flag behaviours and 8 tests listed; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 13 and 14 applied (Target paragraph: masked waits carry the sanitized mask on each zero-timeout probe, as pselect6 does; today the masked path waits in the kernel holding the turn); target_policy stays model; no new F/T ids; removed the masked-wait family-policy question, which emptied Open questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `epoll_pwait`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_pwait&body=Discussion+for+the+%60epoll_pwait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_pwait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_pwait%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `epoll_pwait`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The NULL-mask routing and its regression test came from hermit commit 2d4c80d868 ("Determinize null-mask epoll_pwait progress"). The sibling `epoll_pwait2` has its own cell, `c-programs/epoll-pwait2` (`tests/e2e/manifests/c-programs.yaml:11446`).
