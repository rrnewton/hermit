---
schema: syscall-pages/v1
syscall: ppoll
nr_x86_64: 271
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/26
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ppoll&body=Discussion+for+the+%60ppoll%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fppoll.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ppoll%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 12
---

## Summary

`ppoll(fds, nfds, tmo_p, sigmask, sigsetsize)` waits until a descriptor in a `pollfd` array has an event, the timeout expires, or a signal arrives. It can swap in a temporary signal mask atomically for the wait. Detcore models it (`detcore/src/syscalls/io.rs:928`). Under sequentialized `hermit run`, an unmasked wait becomes zero-timeout probes against a deadline in virtual time. A masked wait gets one probe, and if that probe would block, the call fails closed with `ENOSYS`. Record/replay records the outputs.

Manual reference: Linux man-pages poll(2) (section "ppoll"), signal(7).

## Linux semantics

The kernel checks arguments in this order:

1. copy the timeout (`EFAULT`)
2. validate the timeout (`EINVAL`)
3. install the temporary mask (`EINVAL` for a bad size, `EFAULT` for an unreadable mask)
4. check `nfds` against `RLIMIT_NOFILE`
5. copy the array

It then polls every entry and writes `revents` back for the whole array. The return value is the number of entries with a non-zero `revents`, or 0 on timeout.

The raw syscall writes the unslept time back into `*tmo_p`. The exceptions are an initially zero timeout, which is never written, and a copy-out fault, which is ignored with the result kept. glibc's `ppoll()` passes a private copy, so callers of the wrapper never see the write-back. The original signal mask is restored on return.

Readiness depends on what peers, other processes and outside I/O do to the descriptors. A signal handler that runs during the wait ends it with `EINTR`. Per signal(7), `ppoll` is never restarted after a handler, even with `SA_RESTART`.

### Flags and options

- F1: `nfds`: number of entries in `fds`. A value above `RLIMIT_NOFILE` gives `EINVAL`. `nfds = 0` with `fds = NULL` is a pure timed sleep that signals can interrupt.
- F2: `events` request bits (`POLLIN`, `POLLPRI`, `POLLOUT`, `POLLRDHUP`, `POLLRDNORM`, `POLLRDBAND`, `POLLWRNORM`, `POLLWRBAND`). `revents` reports the requested bits that are ready, plus `POLLERR` and `POLLHUP`, which are reported even when not requested.
- F3: per-entry descriptor rules: a negative `fd` is ignored and gets `revents = 0`. A descriptor that is not open gets `POLLNVAL` and counts toward the return value; it is not an error.
- F4: NULL `tmo_p`: wait without a time limit.
- F5: zero `tmo_p`: test once and return immediately. The timeout is not written back, which matters when `tmo_p` overlaps the `fds` array.
- F6: finite `tmo_p`: wait up to that long. The raw syscall writes the remainder back, also after `EINTR`. A fault during that copy-out (for example on a read-only page) is ignored and the result kept.
- F7: malformed `tmo_p` (negative `tv_sec`, or `tv_nsec` outside `[0, 1e9)`) gives `EINVAL`. An unreadable `tmo_p` gives `EFAULT`.
- F8: NULL `sigmask`: the signal mask is not changed.
- F9: non-NULL `sigmask`: `sigsetsize` must be 8 (`EINVAL` otherwise). An unreadable mask gives `EFAULT`. The mask is applied atomically for the wait and restored on return.
- F10: unreadable `fds` array gives `EFAULT`.
- F11: a signal handler runs during the wait: `EINTR`, never restarted even with `SA_RESTART`. The remaining time is still written back.

## Determinism analysis

`linux_determinism: nondeterministic` (changed from the migration's placeholder `conditional`). The sources are the same as for `pselect6`. When an event arrives, whether the timeout fires first, and the remaining time written back all depend on host scheduling and the real clock. Signal arrival decides between `EINTR` and a normal return. The call repeats only for a zero timeout against descriptors whose state is already settled.

### Nondeterminism sources

- Readiness timing of the polled descriptors
- Timeout expiry against the real clock, and the remainder written back
- Signal arrival relative to the wait; the temporary mask decides which signals can interrupt it

## Hermit design

Target policy: `model`. The kernel judges readiness and argument errors, and Detcore owns the wait. `handle_ppoll` (`detcore/src/syscalls/io.rs:928-986`) works as follows.

- The timeout is read and validated first (`detcore/src/syscalls/io.rs:933-937`, `ppoll_timeout_duration` at `detcore/src/syscalls/io.rs:189-196`), matching the kernel's first check (F7).
- **Zero timeout** (`detcore/src/syscalls/io.rs:939-952`):
  - In record/replay the call takes a scheduler turn first.
  - Detcore injects a zero-timeout probe built by `prepare_ppoll_probe` (`detcore/src/syscalls/io.rs:988-1014`). It checks `sigsetsize` and copies the mask, with Hermit's perf-event signal bit cleared (`sanitize_ppoll_signal_mask`, `detcore/src/syscalls/io.rs:386-389`), into scratch memory.
  - The probe is recorded in record/replay and injected in run.
  - The guest's timeout is not written back (comment at `detcore/src/syscalls/io.rs:949-951`), which keeps F5.
- **Kernel-owned wait.** `ppoll_uses_kernel_wait` (`detcore/src/syscalls/io.rs:391-397`) selects the kernel for unsequentialized run and for unmasked calls in record/replay. Such calls run as `record_or_replay_blocking` with the sanitized mask in scratch memory (`detcore/src/syscalls/io.rs:953-980`), so the raw call still updates the guest's timeout.
- **Modelled wait** (`handle_internal_ppoll`, `detcore/src/syscalls/io.rs:1017-1070`):
  - Without a mask, `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) probes with a zero timeout on each `InternalIOPolling` turn until the call returns non-zero, a signal resumes the thread with `EINTR`, or virtual time passes the deadline (`detcore/src/syscalls/io.rs:1059-1069`). The nonblocking form is defined at `detcore/src/syscalls/helpers.rs:1016`.
  - With a mask (`detcore/src/syscalls/io.rs:1039-1057`), Detcore issues one masked probe. If the probe would have blocked, it returns `ENOSYS` (`detcore/src/syscalls/io.rs:1046-1048`). The comment there says that keeping the mask in effect while the thread is parked would need pending-signal state in the scheduler. This branch is also taken for masked calls in record/replay.
  - Every modelled exit writes back `timeout - elapsed` in virtual time and ignores a copy-out fault (`write_ppoll_remaining`, `detcore/src/syscalls/io.rs:1072-1090`).

Why `model` and not the alternatives:

- `passthrough` would let a sequentialized thread sleep in the kernel holding the only guest turn.
- `emulate` cannot judge readiness without the kernel.
- `refuse` would break the many event loops that use `ppoll`.

Target (owner decisions 13, 14): a masked wait is a non-blocking probe plus a scheduler wait, with the snapshotted mask carried on every probe as `pselect6` does (`detcore/src/syscalls/io.rs:614-625`), against a virtual-time deadline. A pending signal that the mask unblocks then ends a probe with `EINTR` at a scheduler-chosen point (F11). Today the masked branch returns `ENOSYS` when its single probe would block (`detcore/src/syscalls/io.rs:1046-1048`); that is an interim refusal under decision 14, not the target, because Linux has no configuration in which `ppoll` returns `ENOSYS` for a masked wait and a program that unblocks a signal only for the duration of the wait, the pattern `ppoll` exists for, has no fallback for it. The pinning tests at `tests/c/ppoll_simulation.c:363` and `tests/c/ppoll_simulation.c:491` change with the fix.

What remains before the code fully realizes `model`:

- **Masked waits that would block.** A `ppoll` with a non-NULL mask whose first probe finds nothing ready returns `ENOSYS` in sequentialized run and in record/replay. Linux would wait. F9 is realized only when an entry is already ready or the timeout is zero. `tests/c/ppoll_simulation.c:363` and `tests/c/ppoll_simulation.c:491` pin this refusal as the expected result. `pselect6` handles the same situation by carrying the mask on every probe (`detcore/src/syscalls/io.rs:614-625`).
- **F11 for masked waits.** A refused masked wait has no window in which a signal could interrupt it.
- The handler carries the tag `TODO-HUMAN-REVIEW(PR-273)` (`detcore/src/syscalls/io.rs:927`), added by commit efdcfd71a3 "ppoll determinization". This task could not resolve it to a repository number with confidence.

## Recommended tests

- T1 (covers F2, F5, F6): under `hermit run --strict --verify` on ptrace:
  - a readable pipe with a mask and a 1 s timeout returns 1 with `POLLIN`
  - a drained pipe with a zero timeout returns 0
  - a 20 ms timeout returns 0
  - a raw 5 ms sleep with `nfds = 0` writes back `{0, 0}`
  - a raw ready call writes back a positive remainder below its input
  Existing candidates: `run_default_workload` (`tests/c/ppoll_simulation.c:425`) and `run_raw_timeout_copyout` (`tests/c/ppoll_simulation.c:34`). The cell is `c-programs/ppoll-simulation` (`tests/e2e/manifests/c-programs.yaml:5421`, verify ci on ptrace). The integration test `ppoll_waits_use_nonblocking_probes_and_verify` (`hermit-cli/tests/ppoll_simulation.rs:170`) also asserts the `InternalIOPolling` probe trace and runs `--verify` on ptrace.
- T2 (covers F4): an infinite wait on an empty pipe is woken by a second thread.
  Existing candidate: `tests/c/ppoll_simulation.c:508`.
- T3 (covers F5, F6): the write-back edge cases:
  - a zero timeout that overlaps the `pollfd` entry is not written back, and `POLLHUP` survives
  - a timeout on a read-only page keeps the result and is not changed
  Existing candidates: `run_zero_timeout_alias` (`tests/c/ppoll_simulation.c:158`), `run_masked_readonly_timeout` (`tests/c/ppoll_simulation.c:202`), and `ppoll_readonly_zero_timeout_preserves_ready_result` at `hermit-cli/tests/ppoll_simulation.rs:30`.
- T4 (covers F1, F7, F10): the error cases:
  - `nfds = RLIMIT_NOFILE + 1` gives `EINVAL`
  - `tv_nsec = 1e9` gives `EINVAL`
  - a timeout pointer of 1 gives `EFAULT`
  - an unreadable `fds` gives `EFAULT`
  Existing candidates: `run_invalid_nfds` (`tests/c/ppoll_simulation.c:131`), `tests/c/ppoll_simulation.c:476` and `tests/c/ppoll_simulation.c:483`, and `record_poll_invalid_nfds_preserves_einval` at `hermit-cli/tests/record_replay.rs:2157`. No existing assertion covers `EFAULT` for `fds` with `nfds > 0`.
- T5 (covers F8, F9): mask handling:
  - `sigsetsize = 4` gives `EINVAL`
  - an inaccessible mask gives `EFAULT`, in both the sequentialized and the kernel-wait paths
  - a masked call on a ready pipe succeeds
  - a masked call that would block waits until the pipe becomes readable or the timeout passes, and a signal the mask unblocks interrupts it with `EINTR` (owner decision 14); Hermit returns `ENOSYS` today, so this fails until the target lands
  Existing candidates: `run_masked_fail_closed` (`tests/c/ppoll_simulation.c:338`), `run_inaccessible_mask_kernel_wait` (`tests/c/ppoll_simulation.c:379`) with `ppoll_nonsequentialized_kernel_wait_checks_mask_access` at `hermit-cli/tests/ppoll_simulation.rs:99`, and `tests/c/signal_delivery_sequence.c:352-358`. No existing test asserts `EINVAL` for a wrong `sigsetsize`.
- T6 (covers F3): a negative `fd` gets `revents = 0` and does not count. A closed descriptor gets `POLLNVAL` and counts. No existing test asserts either.
- T7 (covers F11): an unmasked 1 s `ppoll` interrupted by a handled signal from another thread gives `EINTR`, with a remaining time below 1 s that is identical across `--verify` runs. No existing test sends a signal into `ppoll`.
- T8 (covers F2, F6, F9): record and replay of partial `revents` copy-out and timeout copy-out, plus a masked zero-timeout probe that must reach the recorder without a blocking-external resource.
  Existing candidates: `record_ppoll_partial_revents_and_timeout_copyout` (`hermit-cli/tests/record_replay.rs:2147`), `record_poll_and_ppoll_ready_pipe` (`hermit-cli/tests/record_replay.rs:2278`), and the record part of `ppoll_waits_use_nonblocking_probes_and_verify` (`hermit-cli/tests/ppoll_simulation.rs:170`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · facts-bot · code_facts changed: tests_naming 3 -> 4.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (unmasked waits are zero-timeout probes against a virtual-time deadline; record/replay records outputs); linux_determinism conditional -> nondeterministic (readiness, timeout and signal timing); masked waits that would block fail closed with ENOSYS, left as an open question for the owner; 11 flag behaviours and 8 tests listed; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decisions 13 and 14 (masked wait carries the mask on every probe plus a virtual-time scheduler wait; ENOSYS at io.rs:1046-1048 is interim; T5 expectation updated); target_policy unchanged; no new F/T ids; removed the masked-ENOSYS question.
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/ppoll_simulation.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `ppoll`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ppoll&body=Discussion+for+the+%60ppoll%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fppoll.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ppoll%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `ppoll`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

Related cell: `c-programs/ppoll-readv` (`tests/e2e/manifests/c-programs.yaml:5363`, program `tests/c/ppoll_readv.c`) exercises a masked `ppoll` on a pipe together with `readv`.

### Open questions

- `TODO-HUMAN-REVIEW(PR-273)` could not be resolved to a repository number from the local history.
