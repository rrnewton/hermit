---
schema: syscall-pages/v1
syscall: timer_getoverrun
nr_x86_64: 225
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_getoverrun&body=Discussion+for+the+%60timer_getoverrun%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_getoverrun.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_getoverrun%60%2C+or+in+what+the+page+says%3F%0A
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

`timer_getoverrun(timerid)` returns the overrun count of a POSIX per-process timer. That is the number of extra expirations that happened between the time the timer's last signal was generated and the time it was delivered. Detcore implements the whole POSIX timer family itself on virtual time (`detcore/src/lib.rs:2783`). Its handler (`detcore/src/syscalls/time.rs:589`) returns 0 for any live timer of the calling process and `EINVAL` otherwise, because it does not count coalesced expirations.

Manual reference: Linux man-pages timer_getoverrun(2).

## Linux semantics

A POSIX timer delivers at most one queued signal at a time. If the timer expires again while its signal is still pending, for example because the signal is blocked or the target thread has not run yet, the kernel queues nothing more. It counts the extra expiration instead. When the signal is delivered or accepted, that count is latched, and `timer_getoverrun` returns the latched value for the most recently delivered signal. The value is clamped to `DELAYTIMER_MAX`, which is `INT_MAX` on Linux.

The timer id is valid only in the process that created it. Timers are not inherited by a `fork` child, and a successful `execve` deletes them all. An id that does not name a live timer of the calling process gives `EINVAL`.

### Flags and options

- F1: a live timer of the calling process with no coalesced expiration: a one-shot timer, or a periodic timer whose signals were each delivered before the next expiry. Returns 0.
- F2: a periodic timer that expired again while its previous signal was still pending, because the signal was blocked or delivery was late. Returns the number of extra expirations counted before the last delivery.
- F3: the overrun count is clamped to `DELAYTIMER_MAX` (`INT_MAX`) and never wraps.
- F4: a timer created with `SIGEV_NONE`. It generates no signal, so there is no delivery to latch a count against; the value reflects only the kernel's internal bookkeeping.
- F5: an id that names no live timer of the calling process: never created, already deleted with `timer_delete`, or created by another process. Gives `EINVAL`.
- F6: an id that was valid before a successful `execve`, or that the parent created before `fork`, used in the child. Gives `EINVAL`, because exec deletes timers and fork does not copy them.

## Determinism analysis

`linux_determinism: nondeterministic`. Under Linux the overrun count of a periodic timer is the number of expirations that fell inside the window between signal generation and delivery. How long that window lasts depends on host scheduling latency, on when the target thread next runs, and on how long the program keeps the signal blocked measured in real time. Two runs of the same program therefore see different counts whenever the interval is short compared with that latency. The call repeats only for F1 cases where every signal is delivered well inside one interval, and for the error cases F5 and F6, which depend only on the process's own timer table.

### Nondeterminism sources

- Host scheduling latency between signal generation and delivery
- Real elapsed time while the program holds the timer signal blocked
- Host load, which changes how many expirations fit in that window

## Hermit design

Target policy: `model`. The kernel never holds these timers. Detcore keeps a per-process timer table with ids handed out as 0, 1, 2, ... in creation order (`detcore/src/tool_local.rs:186`), and arms expirations on the deterministic scheduler's virtual clock. A guest timer id therefore means nothing to the kernel, so `passthrough` is impossible, and `refuse` would break programs that use POSIX timers. The handler realizes a deterministic answer:

- `handle_timer_getoverrun` (`detcore/src/syscalls/time.rs:589`) looks the id up with `PosixTimers::contains` (`detcore/src/tool_local.rs:243`). A live timer gives `Ok(0)`; anything else gives `EINVAL` (`detcore/src/syscalls/time.rs:606`). Its doc comment states the limitation: "coalesced expiration accounting is not modeled, so the overrun count is always 0 for a live timer".
- The table is shared by the threads of one process and replaced by an empty one for a new process (`detcore/src/lib.rs:1561`), and exec clears it (`PosixTimers::clear_for_exec`, `detcore/src/tool_local.rs:258`). That gives F5 and F6 their Linux answer.

What remains before the code fully realizes `model`:

- **Coalesced expirations (F2, F3).** A periodic timer is re-armed at `deadline + interval` every time the scheduler pops its event (`detcore/src/scheduler/timed_waiters.rs:333`), one event per expiry, and nothing counts expirations that happen while the previous signal is still pending. The overrun count is always 0 under Hermit, where Linux would report a positive value. A faithful model would count, in virtual time, the expirations that fell between generation and delivery of the timer's signal; that count would be deterministic because both events sit on the virtual clock. Whether a second standard signal generated while the first is pending is merged or queued twice under Hermit is decided outside this handler; this body has not traced it, so that part is unverified.
- **`SIGEV_NONE` (F4).** Hermit returns 0. Linux's value for such a timer depends on internal bookkeeping and is rarely meaningful, so 0 is an acceptable model; T3 should record what the host returns.

## Recommended tests

- T1 (covers F1, F5): under `hermit run --verify` on ptrace, create a one-shot `SIGUSR1` timer with raw `syscall(SYS_timer_create, ...)`, arm it, let it fire, and check that `timer_getoverrun` returns 0. Then check `EINVAL` for an id that was never created and for the same id after `timer_delete`. No existing test calls `timer_getoverrun`; `tests/standalone/strict_timer_create.sh` names it only in a comment.
- T2 (covers F2): block `SIGUSR1`, arm a periodic timer with a 1 ms interval, sleep 10 ms, unblock, and read the overrun count in the handler. Linux returns about 9. Record the Hermit value (0 at this SHA) and keep the test as the acceptance check for overrun modeling.
- T3 (covers F4): a periodic `SIGEV_NONE` timer read with `timer_getoverrun` after several intervals. Record the host and Hermit values side by side.
- T4 (covers F3): keep a periodic timer's signal blocked across more than `INT_MAX` virtual expirations, for example a 1 µs interval across about 36 virtual minutes reached by sleeping, and check that the count saturates at `INT_MAX`. This needs overrun modeling first.
- T5 (covers F6): `timer_getoverrun` on a pre-exec id after a successful `execve`, and in a `fork` child on an id the parent created, must give `EINVAL`.
  Related existing test: `run_ptrace_exec_deletes_posix_timers_and_preserves_itimer` at `hermit-cli/tests/cli.rs:11505` checks exec deletion through `timer_gettime` (`hermit-cli/tests/fixtures/exec_posix_timers.c:89`), not through `timer_getoverrun`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (Detcore owns POSIX timers on virtual time; the handler returns 0 for a live timer and EINVAL otherwise, but does not count coalesced expirations); linux_determinism nondeterministic (the Linux count depends on signal-delivery latency); 6 flag behaviours and 5 tests listed; no test calls timer_getoverrun yet, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `timer_getoverrun`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_getoverrun&body=Discussion+for+the+%60timer_getoverrun%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_getoverrun.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_getoverrun%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `timer_getoverrun`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The POSIX timer handlers were added by https://github.com/rrnewton/hermit/pull/207. Signal delivery for timer expirations came later, in https://github.com/rrnewton/hermit/pull/869, which the code's `TODO-HUMAN-REVIEW(#869)` tags name.

### Open questions

- Should Hermit count coalesced expirations in virtual time (F2, F3)? The count would be deterministic, but it requires knowing when the timer's pending signal is delivered. Until then, programs that use overruns to catch up on missed periods see 0 under Hermit.
