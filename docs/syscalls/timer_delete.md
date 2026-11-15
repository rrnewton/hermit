---
schema: syscall-pages/v1
syscall: timer_delete
nr_x86_64: 226
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_delete&body=Discussion+for+the+%60timer_delete%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_delete.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_delete%60%2C+or+in+what+the+page+says%3F%0A
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

`timer_delete(timerid)` disarms and destroys a POSIX per-process timer created by `timer_create`. Detcore implements the whole POSIX timer family itself on virtual time (`detcore/src/lib.rs:2783`). Its handler (`detcore/src/syscalls/time.rs:612`) removes the timer from the process's timer table, cancels its pending expiration on the deterministic scheduler, and returns `EINVAL` for an id that names no live timer.

Manual reference: Linux man-pages timer_delete(2).

## Linux semantics

The call disarms the timer if it is armed, so no further expirations happen, and frees it. The id then becomes invalid, and a later `timer_create` may reuse it. Timer ids belong to the creating process: any thread of that process may delete the timer, another process cannot. Timers are not inherited by a `fork` child, and a successful `execve` deletes them all.

A signal the timer already generated and that is still pending when the timer is deleted is a special case. Whether it is still delivered has differed between kernel versions: older kernels leave the already-queued signal in place, and the reworked POSIX timer code in recent kernels discards signals that belong to a deleted timer. Programs should not depend on either behaviour.

### Flags and options

- F1: a live, disarmed timer of the calling process: freed, returns 0, and the id becomes invalid.
- F2: a live, armed timer, one-shot or periodic: disarmed first, so no further expiration or signal happens, then freed. Returns 0.
- F3: a signal the timer generated before deletion that is still pending, because it is blocked or not yet delivered. Linux kernel versions differ on whether it is still delivered.
- F4: an id that names no live timer of the calling process: never created, deleted already (a double delete), or created by another process. Gives `EINVAL`.
- F5: an id that was valid before a successful `execve`, or that the parent created before `fork`, used in the child. Gives `EINVAL`.
- F6: the timer's notification kind: `SIGEV_SIGNAL`, `SIGEV_NONE`, or `SIGEV_THREAD_ID`, which glibc also uses to implement `SIGEV_THREAD`. Each is deleted the same way.
- F7: a timer created by another thread of the same process: any thread may delete it.

## Determinism analysis

`linux_determinism: conditional`, unchanged. The return value and the effect on the timer table depend only on the calling process's own timers, so a program that deletes a disarmed timer, or an armed timer whose next expiry is far away, repeats exactly. It does not repeat when an armed timer is deleted close to its expiry. Whether the expiry happened just before the call, and so whether one more signal is generated or delivered, depends on host timing; F3 adds a dependence on the kernel version.

### Nondeterminism sources

- The race between the call and the timer's next expiry on the host clock
- Whether a signal generated just before deletion is delivered, which varies with the kernel version

## Hermit design

Target policy: `model`. The kernel never holds these timers. Detcore keeps a per-process timer table with ids handed out as 0, 1, 2, ... in creation order (`detcore/src/tool_local.rs:186`), and arms expirations on the scheduler's virtual clock. A guest timer id therefore means nothing to the kernel, so `passthrough` is impossible, and `refuse` would break every program that cleans up a timer. The handler at `detcore/src/syscalls/time.rs:612` realizes the model:

- It looks up the timer's stored signal with `PosixTimers::signal` (`detcore/src/tool_local.rs:249`). An unknown id gives `EINVAL` (`detcore/src/syscalls/time.rs:625`), which covers F4.
- It removes the timer from the table (`PosixTimers::remove`, `detcore/src/tool_local.rs:254`; `detcore/src/syscalls/time.rs:628`).
- If the timer has a signal the scheduler can deliver, it calls `register_posix_timer` with no deadline (`detcore/src/syscalls/time.rs:632`; `detcore/src/tool_global.rs:3993`). The scheduler then removes the timer's event (`detcore/src/scheduler.rs:5817`; `remove_posix_timer`, `detcore/src/scheduler/timed_waiters.rs:270`), including its periodic re-arm, which covers F2. Because the race with expiry is decided in virtual time, a run repeats exactly where Linux would be timing dependent.
- Timers that have no scheduler event need no cancellation. `timer_create` stores no deliverable signal for `SIGEV_NONE` (`detcore/src/syscalls/time.rs:472`), nor for a signal number that the `nix` `Signal` type cannot represent, such as a real-time signal (`detcore/src/syscalls/time.rs:479`). `SIGEV_THREAD_ID` is treated as process-directed (`detcore/src/syscalls/time.rs:475`). All three are deleted through the same path, which covers F6.
- The table is shared by threads created with `CLONE_THREAD` and replaced by an empty one for a new process (`detcore/src/lib.rs:1561`); exec clears it (`detcore/src/tool_local.rs:258`). That covers F5 and F7.

What remains before the code fully realizes `model`:

- **Already-pending signal (F3).** The handler only cancels the scheduler's future event. By this reading of the code it does not withdraw a timer signal that has already been generated and is still pending, which matches the older Linux behaviour rather than the newer one. This is a hypothesis from the handler, not traced through Detcore's pending-signal code; T3 settles it. Which Linux behaviour Hermit should follow is listed under Open questions.
- The handler and the timer table carry `TODO-HUMAN-REVIEW(#869)` tags, which name https://github.com/rrnewton/hermit/pull/869; that change awaits human review.

## Recommended tests

- T1 (covers F1, F2, F4): under `hermit run --verify` on ptrace, delete a disarmed timer and an armed one (both return 0), delete the same id again, and delete an id never created (both `EINVAL`).
  Existing candidate: `tests/c/timer_create_determinism.c` (cell `c-programs/timer-create-determinism`, `tests/e2e/manifests/c-programs.yaml:10299`, verify ci on ptrace). It checks that deleting an armed timer returns 0 (`tests/c/timer_create_determinism.c:62`) but has no double delete or unknown id.
- T2 (covers F2, F6): arm periodic 10 ms timers with `SIGEV_SIGNAL`, `SIGEV_NONE` and `SIGEV_THREAD`, let `SIGEV_SIGNAL` fire once, delete all three, sleep 100 ms, and assert no further signal or thread callback arrives.
  Related existing test: `tests/bin/posix_timer_test.c` (cell `bin-c/posix-timer-test`, `tests/e2e/manifests/bin-c.yaml:4`, verify ci on ptrace) deletes a `SIGALRM` timer after its one shot fired (`tests/bin/posix_timer_test.c:108`), but does not check for later signals.
- T3 (covers F3): block `SIGUSR1`, arm a 1 ms one-shot timer, sleep 10 ms so its signal is pending, delete the timer, unblock, and record whether the handler runs. Record the host kernel's answer next to Hermit's.
- T4 (covers F5): `timer_delete` on a pre-exec id after a successful `execve`, and in a `fork` child on an id the parent created, must give `EINVAL`.
  Related existing test: `run_ptrace_exec_deletes_posix_timers_and_preserves_itimer` at `hermit-cli/tests/cli.rs:11505` checks exec deletion through `timer_gettime` (`hermit-cli/tests/fixtures/exec_posix_timers.c:89`), not through `timer_delete`.
- T5 (covers F7): thread B deletes an armed timer created by thread A, and the timer's signal never arrives.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (Detcore owns POSIX timers on virtual time; the handler removes the timer and cancels its scheduler event, EINVAL for unknown ids); linux_determinism stays conditional (deletion racing expiry is timing dependent on Linux); 7 flag behaviours and 5 tests listed; open question on an already-pending timer signal, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `timer_delete`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_delete&body=Discussion+for+the+%60timer_delete%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_delete.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_delete%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `timer_delete`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The POSIX timer handlers were added by https://github.com/rrnewton/hermit/pull/207. Signal delivery for timer expirations, and the scheduler cancellation in this handler, came later in https://github.com/rrnewton/hermit/pull/869.

### Open questions

- When a timer is deleted while a signal it generated is still pending (F3), should Hermit deliver that signal, like older kernels, or discard it, like recent kernels? Both are deterministic under Hermit; the choice decides which Linux version Hermit imitates. Owner decision.
