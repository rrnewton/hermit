---
schema: syscall-pages/v1
syscall: timer_gettime
nr_x86_64: 224
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_gettime&body=Discussion+for+the+%60timer_gettime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_gettime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_gettime%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`timer_gettime(timerid, curr_value)` reports how long remains until a POSIX timer's next expiration and its reload interval. Detcore emulates it (`detcore/src/syscalls/time.rs:568`): it computes both from its own per-process timer table and the virtual clock, so the reported time depends only on virtual time, never on the host clock.

Manual reference: Linux man-pages timer_gettime(2).

## Linux semantics

`curr_value->it_value` receives the time left until the next expiration, and `curr_value->it_interval` the reload period, both as relative times even when the timer was armed with `TIMER_ABSTIME`. A disarmed timer, or a one-shot timer that has already expired, reports zero for `it_value`. A periodic timer reports the time to its next period. A `SIGEV_NONE` timer is reported the same way; it just never signals.

The kernel looks up the timer id first and then writes the result, so an unknown or deleted id gives `EINVAL` even when `curr_value` is bad, and a valid id with a bad `curr_value` gives `EFAULT`. Timer ids belong to the process: a forked child has none of its parent's timers, and after a successful `execve` every old id is invalid.

### Flags and options

- F1: armed one-shot timer: `it_value` is the time left until expiration and `it_interval` is zero.
- F2: periodic timer: `it_value` is the time to the next period and `it_interval` is the reload period.
- F3: disarmed timer, or a one-shot timer that has already expired: `it_value` is zero.
- F4: `SIGEV_NONE` timer: reported the same way as a signalling timer.
- F5: unknown or deleted timer id: `EINVAL`, checked before `curr_value` is written.
- F6: `curr_value` NULL or pointing at bad memory, with a valid id: `EFAULT`.
- F7: process scope: ids from the parent are invalid in a forked child, and every id is invalid after a successful `execve`.
- F8: timer on a CPU-time clock: `it_value` is the CPU time the target process or thread must still consume before expiration, which does not shrink while the target is blocked.

## Determinism analysis

`linux_determinism: nondeterministic`. The remaining time is measured on a host clock, so it reflects however much real time passed since the timer was armed, which depends on host load and scheduling. The call repeats only for a disarmed or expired timer and for its error results.

### Nondeterminism sources

- Remaining time (F1, F2, F4): elapsed real time since arming.
- Whether a one-shot timer has already expired when the call runs (F1, F3): host timing.

## Hermit design

Target policy: `model`. The handler at `detcore/src/syscalls/time.rs:568` (dispatch arm `detcore/src/lib.rs:2787`) never forwards the call:

- It reads the virtual time (`detcore/src/syscalls/time.rs:575`), looks the timer up in the per-process table (`detcore/src/syscalls/time.rs:576-579`, `detcore/src/tool_local.rs:234-240`) and writes `it_value` and `it_interval` (`detcore/src/syscalls/time.rs:581-585`).
- The remaining time comes from `remaining_ns` (`detcore/src/tool_local.rs:312-322`): the gap to the deadline while it is in the future, the time to the next period for an expired periodic timer, and zero for a disarmed or expired one-shot timer (F1, F2, F3). Timers without a deliverable signal use the same computation (F4).
- An unknown id gives `EINVAL` (`detcore/src/syscalls/time.rs:580`, F5).
- The table is per process: shared by threads, empty in a new process (`detcore/src/lib.rs:1561-1564`), and cleared by a successful exec (`detcore/src/lib.rs:1760-1765`) (F7).

Why `model` and not the alternatives: the timer lives only in Detcore's table, so there is no host timer to forward to, and refusing would break every program that reads its timers.

What remains before the code fully realizes `model`:

- **Error order (F5, F6).** A NULL `curr_value` is checked before the id (`detcore/src/syscalls/time.rs:574`), so a NULL pointer with an unknown id gives `EFAULT` where Linux gives `EINVAL`.
- **Periodic timer exactly at a deadline.** When the virtual time equals the deadline of a periodic timer, `remaining_ns` returns the full interval (`detcore/src/tool_local.rs:315-317`), treating that period as already passed. This is consistent and deterministic, but the boundary has no test.

Target (owner decisions 13 and 6): the remaining time is measured on the timer's own clock, as the `timer_settime` body describes. The clock names are those defined in the `clock_gettime` body.

- A virtual realtime or virtual uptime timer reports the gap to its deadline on that clock. Both clocks advance with the same logical time, so the relative value is today's.
- A CPU-time timer reports the virtual CPU time its target still has to consume (F8). Today it reports elapsed virtual time (`detcore/src/syscalls/time.rs:575`).

Kernel order (`SYSCALL_DEFINE2(timer_gettime)` in `kernel/time/posix-timers.c`): `lock_timer` gives `EINVAL` for an unknown id, then `put_itimerspec64` gives `EFAULT`. So the target moves the `curr_value` check after the lookup (`detcore/src/syscalls/time.rs:574` before `detcore/src/syscalls/time.rs:580` today).

`linux_determinism` is `nondeterministic` for the native call; under Hermit the result is a function of virtual time.

## Recommended tests

- T1 (covers F1, F3): arm a one-shot timer for 300 s and check that `it_value` is positive and `it_interval` zero; arm one for 10 ms, wait for it, and check `it_value` is zero, under `hermit run --strict --verify` on ptrace.
  Existing candidate for the first half: `tests/c/timer_create_determinism.c` (cell `c-programs/timer-create-determinism`, `tests/e2e/manifests/c-programs.yaml:10299`, verify ci on ptrace), which prints only whether time remains.
- T2 (covers F2, F7): a periodic timer keeps its interval and stays armed across a failed exec, and every id gives `EINVAL` after a successful exec.
  Existing candidate: `run_ptrace_exec_deletes_posix_timers_and_preserves_itimer` at `hermit-cli/tests/cli.rs:11505`, driving `hermit-cli/tests/fixtures/exec_posix_timers.c`. Unit tests: `settime_reports_previous_arming_and_remaining_uses_virtual_clock` at `detcore/src/tool_local.rs:877` and `periodic_remaining_advances_past_each_deadline` at `detcore/src/tool_local.rs:908`.
- T3 (covers F4): a `SIGEV_NONE` timer armed for 50 ms reports a decreasing `it_value` across two reads separated by a sleep, then zero after the deadline.
- T4 (covers F5, F6): a deleted id gives `EINVAL`; a deleted id with NULL `curr_value` gives `EINVAL`; a valid id with NULL or unmapped `curr_value` gives `EFAULT`. The second case is expected to fail at this SHA.
- T5 (covers F7): a forked child calling `timer_gettime` on a parent timer id gets `EINVAL`, and a second thread reads a timer the first thread armed.
- T6 (covers F8): arm a `CLOCK_THREAD_CPUTIME_ID` timer for 50 ms, then sleep 1 s of virtual time and read it. `it_value` must still be 50 ms minus the CPU time consumed before the sleep. After 20 ms of spinning it must have dropped by 20 ms. The values must be identical across `hermit run --verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (remaining time and interval computed from Detcore's per-process timer table and the virtual clock); linux_determinism nondeterministic; gap: NULL curr_value checked before the id (EFAULT where Linux gives EINVAL); 7 flag behaviours and 5 tests listed; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 and 6 (remaining time on the timer's own clock, virtual CPU time for CPU-time timers; kernel order id EINVAL before curr_value EFAULT); target_policy stays model; new F8, T6; no open questions existed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `timer_gettime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_gettime&body=Discussion+for+the+%60timer_gettime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_gettime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_gettime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `timer_gettime`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The timer table and its unit tests (`posix_timers_tests`, `detcore/src/tool_local.rs:861`) were added by commit 208f574f1a (2026-07-22, "Land missing --strict syscall handlers: identity/fs + POSIX timers + membarrier"); signal delivery was added by commit 5687ea1683 (2026-07-27), whose subject names https://github.com/rrnewton/hermit/pull/869. The sibling bodies `timer_create.md` and `timer_settime.md` describe creation, arming and the notification gaps; this call is unaffected by them because it never sends a signal.
