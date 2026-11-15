---
schema: syscall-pages/v1
syscall: rt_sigtimedwait
nr_x86_64: 128
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigtimedwait&body=Discussion+for+the+%60rt_sigtimedwait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigtimedwait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigtimedwait%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 7
---

## Summary

`rt_sigtimedwait(set, info, timeout, sigsetsize)` waits until one of the signals in `set` is pending for the calling thread, removes it from the pending set and returns its number, optionally with its `siginfo_t`. Detcore handles it (`handle_rt_sigtimedwait`, `detcore/src/syscalls/signal.rs:531`) by polling: every attempt is the real call with a zero timeout, retried at scheduler turns, and the caller's timeout is measured in Hermit's virtual time. It is not recorded for record/replay.

Manual reference: Linux man-pages sigtimedwait(2).

## Linux semantics

The kernel checks its arguments in this order:

1. `sigsetsize` must equal the kernel's signal-set size (8 bytes on x86_64), or the call fails with `EINVAL` before any pointer is read.
2. `set` is copied in (`EFAULT` if unreadable), then `timeout` if non-NULL (`EFAULT` if unreadable).
3. A non-NULL `timeout` must be a valid timespec, with `tv_sec >= 0` and `0 <= tv_nsec < 1000000000`, or the call fails with `EINVAL`.

`SIGKILL` and `SIGSTOP` are removed from `set` silently. If a signal from the set is already pending, the call returns at once. Otherwise the signals in `set` are unblocked for the wait only, and the thread sleeps until one becomes pending, until the timeout expires, or until a handled signal outside the set interrupts it. The wait set is the copy taken at entry; later changes to the caller's buffer have no effect.

When several signals from the set are pending, the lowest-numbered one is taken first, a thread-directed signal before a process-directed one, and queued realtime signals in arrival order. The return value is the signal number. If `info` is non-NULL the kernel copies the `siginfo_t` out after dequeuing the signal, so an unwritable `info` consumes the signal and then reports `EFAULT`.

A handled signal outside the set ends the wait with `EINTR` after its handler runs, even if the handler was installed with `SA_RESTART`. Expiry returns `EAGAIN`. The timeout is relative and runs on `CLOCK_MONOTONIC`.

### Flags and options

- F1: `sigsetsize` other than 8: `EINVAL`, before `set` or `timeout` is read
- F2: `set` or `timeout` unreadable: `EFAULT`
- F3: `timeout == NULL`: wait without limit
- F4: `timeout` of zero: poll; return a pending signal from the set or fail at once with `EAGAIN`
- F5: positive `timeout`: wait at most that long, then `EAGAIN`
- F6: invalid `timeout` (negative `tv_sec`, or `tv_nsec` outside `0..999999999`): `EINVAL`
- F7: a signal from the set is pending, or becomes pending: it is dequeued and its number returned. Lowest number first, thread-directed before process-directed, realtime signals queued in arrival order. `SIGKILL` and `SIGSTOP` in `set` are ignored.
- F8: `info` non-NULL: the dequeued signal's `siginfo_t` is copied out (`si_signo`, `si_code`, sender pid and uid or `si_value`). An unwritable `info` gives `EFAULT` after the signal was consumed. `info == NULL` returns the number only.
- F9: a handled signal outside the set interrupts the wait: `EINTR` after the handler runs, regardless of `SA_RESTART`
- F10: the wait set is the snapshot taken at entry; another thread changing the buffer during the wait has no effect

## Determinism analysis

`linux_determinism: conditional`, kept from the rendered body. Given the same pending signals at entry, the result is fixed: the dequeue order (F7) is a function of the pending set, and the argument checks (F1, F2, F6) depend only on the arguments. What varies between runs is which signals are pending when the call looks, and when a blocking wait ends. That depends on when other threads, other processes, timers and the terminal raise signals, and for F5 on how much real time passes. The call repeats when every signal source is inside one deterministic schedule and the timeout is measured on a clock that schedule controls.

### Nondeterminism sources

- Arrival time and order of signals from other threads, processes, timers and the terminal
- Real elapsed time against the timeout (F5)
- Which signal interrupts a blocking wait, and whether it arrives before a signal from the set (F9)

## Hermit design

Target policy: `model`. Detcore keeps the kernel as the owner of pending signals and of the dequeue order, and makes the waiting itself a scheduler decision: the thread never sleeps in the kernel, and time passes only in virtual time. The code at hermit f4055eb9cae4:

- The typed arm `Syscall::RtSigtimedwait(s) => self.handle_rt_sigtimedwait(guest, s).await` (`detcore/src/lib.rs:2725`) reaches `handle_rt_sigtimedwait` (`detcore/src/syscalls/signal.rs:531`).
- F1: `validate_kernel_sigset_size` (`detcore/src/syscalls/signal.rs:118`) returns `EINVAL` before any pointer is read (`detcore/src/syscalls/signal.rs:537`). `tests/c/signal_delivery_sequence.c:304` checks this with an inaccessible timeout.
- F3 to F5: a non-NULL timeout is read from guest memory and turned into a target virtual time, the current `thread_observe_time` plus the timeout (`detcore/src/syscalls/signal.rs:546`). The call then goes to `retry_nonblocking_syscall_with_timeout` with an `InternalIOPolling` resource (`detcore/src/syscalls/signal.rs:556`).
- Each attempt replaces the caller's timeout with a zero timespec (`into_nonblocking`, `detcore/src/syscalls/helpers.rs:1147`), so the kernel never blocks. `EAGAIN` means "would have blocked" (`detcore/src/syscalls/helpers.rs:1158`). The retry loop runs the real call with `inject_with_retry` (`detcore/src/syscalls/helpers.rs:1494`). Once virtual time reaches the target, it returns `EAGAIN` (`detcore/src/syscalls/helpers.rs:1503`; timeout value at `detcore/src/syscalls/helpers.rs:1167`). A zero timeout (F4) therefore makes a single attempt and returns the kernel's answer.
- F7, F8: when a signal from the set is pending, the zero-timeout attempt dequeues it in the kernel's order and fills `info`, so the result and any `EFAULT` for `info` are the kernel's.
- F9: if the scheduler wakes the thread with a signal before the next attempt, the loop returns `EINTR` (`detcore/src/syscalls/helpers.rs:1476`; errno chosen at `detcore/src/syscalls/helpers.rs:1162`).
- Record/replay: the doc comment (`detcore/src/syscalls/signal.rs:527`) says the call is not passed to the record/replay layer because signals are not recorded, and the helper keeps the raw `inject_with_retry` path for it (`detcore/src/syscalls/helpers.rs:1445`).

What remains before the code fully realizes `model`:

- **No `EINVAL` for an invalid timeout (F6).** The timespec is converted with `Duration::from_secs(ts.tv_sec as u64) + Duration::from_nanos(ts.tv_nsec as u64)` (`detcore/src/syscalls/signal.rs:548`) without validation, and the kernel only ever sees the zero timespec. A negative `tv_sec` becomes a huge unsigned value; the addition saturates (`detcore-model/src/time.rs:292`), so nothing panics, but the guest waits with an effectively unbounded timeout where Linux returns `EINVAL`. A `tv_nsec` of 1000000000 or more is accepted as extra seconds.
- **The wait set is re-read on every attempt (F10).** The comment at `detcore/src/syscalls/signal.rs:538` says so directly: another thread can change the set between retries, and fixing it needs a scratch-stack copy shared with the zero-timeout object.
- **`EFAULT` for an unreadable timeout (F2) comes from Detcore's own read** (`read_value`, `detcore/src/syscalls/signal.rs:547`), not from the kernel. `read_kernel_sigset` documents that `PTRACE_PEEKDATA` can read a `PROT_NONE` page for reads of 8 bytes or less (`detcore/src/syscalls/signal.rs:131`); a timespec is 16 bytes, so this probably still faults, but no test checks a valid-size call with an inaccessible timeout.
- **Signals from the set that arrive while the thread is parked.** The loop returns `EINTR` on any signal wake-up before retrying (`detcore/src/syscalls/helpers.rs:1476`), without checking whether the signal is in the wait set. If the scheduler ever wakes a parked `rt_sigtimedwait` for a signal in its set, the guest would see `EINTR` where Linux returns the signal. This is a reading of the loop, not a measurement; T5 checks it.
- **Not recorded.** A recording does not capture the result, so replay reruns the real call and depends on the same signals being pending.

## Recommended tests

- T1 (covers F1, F2, F6): raw `syscall(SYS_rt_sigtimedwait, ...)`: `sigsetsize` 9 with an inaccessible timeout gives `EINVAL`; an inaccessible `set` with size 8 gives `EFAULT`; an inaccessible timeout with size 8 gives `EFAULT`; timeouts `{-1, 0}` and `{0, 1000000000}` give `EINVAL`. Hermit is expected to fail the last two today.
  Existing partial coverage: `tests/c/signal_delivery_sequence.c:304` (size 9 gives `EINVAL`), cell `c-programs/signal-delivery-sequence` (`tests/e2e/manifests/c-programs.yaml:17172`, verify ci on ptrace).
- T2 (covers F4, F7): with `SIGUSR1` and `SIGUSR2` blocked and both raised, two zero-timeout calls return `SIGUSR1` then `SIGUSR2`; a third returns `EAGAIN`. A set at a page boundary works.
  Existing partial coverage: `tests/c/signal_delivery_sequence.c:395` (mask at a page boundary, zero timeout, `EAGAIN`), and `tests/c/sigtimedwait-timeout-0s.c` (cell `c-programs/sigtimedwait-timeout-0s`, `tests/e2e/manifests/c-programs.yaml:8342`, verify ci on ptrace), which prints the return value and errno of a zero-timeout call.
- T3 (covers F3, F7, F8): a second thread sends `SIGTERM` with `pthread_kill` while the first waits with `timeout == NULL` and `SIGTERM` blocked. The waiter must return `SIGTERM`, with `si_code == SI_TKILL` and the sender's pid in `info`. Repeat with `sigqueue` and check `si_value`. With `info = (void *)1`, the call gives `EFAULT` and the signal is no longer pending.
  Existing candidate: `tests/c/sigtimedwait-no-timeout.c` (cell `c-programs/sigtimedwait-no-timeout`, `tests/e2e/manifests/c-programs.yaml:8274`, verify ci on ptrace). It does the `pthread_kill` part but does not check the return value or `info`.
- T4 (covers F5): a 1 s timeout with no signal returns `EAGAIN`, and `clock_gettime(CLOCK_MONOTONIC)` advances by at least 1 s of virtual time. The same call with the signal sent after 100 ms returns the signal.
  Existing candidate: `tests/c/sigtimedwait-timeout-1s.c` (cell `c-programs/sigtimedwait-timeout-1s`, `tests/e2e/manifests/c-programs.yaml:8414`, verify ci on ptrace), which does not check the return value.
- T5 (covers F7, F9): block `SIGALRM`, arm `setitimer` for 50 ms, and call `rt_sigtimedwait` for `SIGALRM` with a 5 s timeout: it must return `SIGALRM`, not `EINTR`. Separately, a handled `SIGUSR1` with `SA_RESTART`, raised by another thread during a wait for `SIGUSR2`, must give `EINTR` and leave `SIGUSR2` untouched.
  Related existing test: the `sigtimedwait-sa-restart` scenario of `tests/c/signal_determinism.c` (`tests/c/signal_determinism.c:370`). It runs only from `signal_interrupts_rt_sigtimedwait_despite_sa_restart` in `hermit-cli/tests/signal_determinism.rs:273`; the e2e cell for that file runs only the `itimer-delivery` scenario.
- T6 (covers F10): a waiter blocks for `SIGUSR1` with a 1 s timeout; a sibling thread rewrites the waiter's set buffer to `SIGUSR2` and then sends `SIGUSR2`. Linux keeps waiting and times out; Hermit is expected to return `SIGUSR2` today.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel owns pending signals and dequeue order; Detcore polls with a zero timeout at scheduler turns and measures the timeout in virtual time); linux_determinism stays conditional; 10 flag behaviours and 6 tests; known gaps: no EINVAL for an invalid timespec, set re-read per retry, not recorded; no coverage token, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rt_sigtimedwait`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigtimedwait&body=Discussion+for+the+%60rt_sigtimedwait%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigtimedwait.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigtimedwait%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `rt_sigtimedwait`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`hermit-cli/tests/signal_determinism.rs` runs the `sigtimedwait-sa-restart` scenario, but it builds the backend flag with `format!` and also has a DBT-only test, so under the coverage rules it cannot carry a coverage token for this body. The three `sigtimedwait-*` programs are also run by `hermit-cli/tests/hermit_modes.rs:228`.
