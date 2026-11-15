---
schema: syscall-pages/v1
syscall: poll
nr_x86_64: 7
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+poll&body=Discussion+for+the+%60poll%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpoll.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60poll%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 8
---

## Summary

`poll(fds, nfds, timeout)` waits until one of a set of descriptors is ready for I/O, or until a timeout in milliseconds expires, and reports per-descriptor readiness in `revents`. Detcore handles it in `handle_poll` (`detcore/src/syscalls/io.rs:402`): under the sequentialized scheduler it never lets the kernel block, but turns a waiting poll into repeated zero-timeout probes with a deadline in logical time; in record/replay the result and the `revents` array are recorded and replayed.

Manual reference: Linux man-pages poll(2).

## Linux semantics

For each entry, the kernel asks the file's `poll` method which of the requested `events` hold, always adding `POLLERR`, `POLLHUP` and `POLLNVAL` when they apply. If any entry is ready, or `timeout` is 0, the call returns at once with the number of entries whose `revents` is nonzero. Otherwise the thread sleeps until a wakeup on one of the files, a signal, or the timeout. The timeout is rounded up to the system clock granularity.

`revents` is written back entry by entry, so a fault partway through the array leaves the earlier entries updated and still returns `EFAULT`. Regular files and block devices always report readable and writable. A signal handler ends the wait with `EINTR`; poll is not restarted after a handler even with `SA_RESTART`.

### Flags and options

- F1: `timeout > 0`: wait at most that many milliseconds; return 0 if nothing became ready
- F2: `timeout == 0`: probe once and return immediately
- F3: `timeout < 0`: wait with no time limit
- F4: `nfds == 0` (`fds` may be NULL): no descriptors, so the call is a sleep for `timeout` (or forever when negative)
- F5: requested input events: `POLLIN`, `POLLRDNORM`, `POLLRDBAND`, `POLLPRI` and `POLLRDHUP` (peer shut down its writing half), each reported according to the file type
- F6: requested output events: `POLLOUT`, `POLLWRNORM`, `POLLWRBAND`
- F7: events reported without being requested: `POLLERR`, `POLLHUP` (for example a pipe whose writers all closed) and `POLLNVAL` (the descriptor is not open); `events == 0` still receives them
- F8: negative `fd` in an entry: the entry is skipped and its `revents` set to 0
- F9: regular files and block devices: always ready for reading and writing
- F10: return value: the count of entries with nonzero `revents`, not the count of events
- F11: `nfds` above `RLIMIT_NOFILE`: `EINVAL`
- F12: `fds` not readable, or `revents` not writable: `EFAULT`, with `revents` of entries before the fault already written; `ENOMEM` if the kernel cannot allocate its table
- F13: a signal handler runs during the wait: `EINTR`, never restarted

## Determinism analysis

`linux_determinism: nondeterministic` (changed from the rendered `conditional`). Readiness depends on what other threads, processes and outside endpoints have done by the instant of the probe, and a waiting poll ends when the host scheduler wakes it or the real-time timeout expires. Two runs of the same program can see different `revents`, different return counts, or a timeout in one run and readiness in the other. The call repeats only when every descriptor is in the same state at the moment of the call and no wait is involved, for example a zero-timeout probe of regular files.

### Nondeterminism sources

- The order in which the host schedules the producer and the poller, which decides whether data, space or a hangup is visible at the probe
- Real-time timeouts: how much work other threads do before the deadline passes
- External endpoints (network sockets, host pipes, ttys) whose readiness depends on the outside world
- Signal arrival time, which decides whether and when the wait ends with `EINTR`

## Hermit design

Target policy: `model`. The kernel remains the authority on per-descriptor readiness, and Detcore owns waiting and time. At hermit f4055eb9cae4, the dispatch arm `detcore/src/lib.rs:2609` calls `handle_poll` (`detcore/src/syscalls/io.rs:402`), which has three paths:

- **Zero timeout with the sequentialized scheduler** (`detcore/src/syscalls/io.rs:408`, F2): Detcore first takes a scheduler turn (`zero_timeout_poll_request`, `detcore/src/syscalls/io.rs:78`), so a busy polling loop cannot starve its producer, then injects the call, or records/replays it in record/replay modes. The turn is a strong `SchedYield` only when `discover_live_file_metadata` is set and the thread has a loopback peer (the SaBRe libcurl case described above that function).
- **Waiting poll in run mode** (`handle_internal_poll`, `detcore/src/syscalls/io.rs:1094`; F1, F3, F4): a positive timeout becomes an absolute logical-time deadline through `millis_duration_to_absolute_timeout` (`detcore/src/syscalls/helpers.rs:1593`). `positive_millis_as_nanos` (`detcore/src/syscalls/helpers.rs:1612`) treats 0 and negative values as having no deadline, matching Linux. `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) then repeats a zero-timeout copy of the call (`into_nonblocking`, `detcore/src/syscalls/helpers.rs:996`) once per scheduler turn under the `InternalIOPolling` resource:
  - a result of 0 counts as "would have blocked", so an empty poll (F4) waits in logical time and returns 0 at the deadline (`timeout_return_val`, `detcore/src/syscalls/helpers.rs:1009`; deadline check at `detcore/src/syscalls/helpers.rs:1505`);
  - a signal delivered while waiting ends the call with `EINTR` (`signal_interrupt_errno`, `detcore/src/syscalls/helpers.rs:1004`; F13).
- **Record/replay, or no sequentialization** (`detcore/src/syscalls/io.rs:423`, `handle_external_poll` at `detcore/src/syscalls/io.rs:1122`): `nfds == 0` with a zero timeout is a sleep request returning 0. Anything else goes through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:111`), which deschedules the thread under `BlockingExternalIO` while it blocks in the kernel; the comment at `detcore/src/syscalls/helpers.rs:156` notes that such a thread rejoins in a nondeterministic order. The recorder captures the result and the `revents` array (`hermit-cli/src/recorder/network.rs:299`); the replayer writes the recorded `revents` back without touching any descriptor (`hermit-cli/src/replayer/network.rs:276`), including the partial copy-out of F12.

Every readiness decision (F5 to F12) comes from the kernel, so Hermit reproduces Linux's file-type rules, `POLLNVAL`, skipped negative descriptors and error precedence without re-implementing them.

Why `model` and not the alternatives: `passthrough` lets the thread sleep in the kernel where the deterministic scheduler cannot see it, and ties the timeout to real time. `emulate` would need Detcore to know the readiness of every file type. `refuse` would break event loops in nearly every server, shell and build tool.

Target (owner decisions 10, 13, 14): in sequentialized run every waiting poll, on internal and external descriptors alike, is a non-blocking probe plus a scheduler wait against a virtual-time deadline. `handle_internal_poll` already realizes this (`detcore/src/syscalls/io.rs:1094`, deadline from `millis_duration_to_absolute_timeout` at `detcore/src/syscalls/helpers.rs:1593`), so the `is-external-poll` TODO at `detcore/src/syscalls/io.rs:429` should not route host sockets or ttys to `record_or_replay_blocking`: decision 14 keeps the probe form. Record/replay and unsequentialized run still use `record_or_replay_blocking` (`detcore/src/syscalls/io.rs:1137`), which deschedules the thread rather than holding the turn. Under decision 10, `print_poll` unwraps `call.fds()` (`detcore/src/syscalls/io.rs:63`) on that path; with debug logging enabled, `poll(NULL, 1, 100)` in record mode panics the tracer where Linux returns `EFAULT`, so the unwrap must go.

What remains: the run-mode path treats every descriptor as guest-internal; the comment at `detcore/src/syscalls/io.rs:429` leaves the split for external descriptors as a TODO. A poll on a host socket or tty is therefore probed against logical time, and what it sees depends on when outside data arrives. Hermit accepts that as external input rather than removing it, which is why `linux_determinism` stays `nondeterministic`.

## Recommended tests

- T1 (covers F2, F5, F6, F7, F10): zero-timeout `poll` on a pipe: `POLLOUT` on the empty write end, `POLLIN` after a write, not ready after the read drains it, `POLLHUP` after the writer closes; check each return count, under `hermit run --verify` on ptrace and through record and replay.
  Existing candidate: `tests/c/poll_readiness.c` (cell `c-programs/poll-readiness`, `tests/e2e/manifests/c-programs.yaml:14796`, verify ci and replay ci on ptrace).
- T2 (covers F1, F4, F9): an empty poll with a 300 ms timeout returns 0, then poll two regular files with timeouts of -1, 0 and 300, one entry requesting every event and one requesting none; exactly one entry may report events.
  Existing candidate: `tests/rust/poll.rs`, which the test inventory (`tests/e2e/manifests/inventory/test-files.json:3629`) says is built as cargo binary `poll` and selected by a Rust integration test. I did not find the selecting test by name, so whether it runs on ptrace in CI is unconfirmed.
- T3 (covers F3): a thread blocks in `poll(fds, 1, -1)` on a pipe until a second thread writes after a fixed amount of guest work; the return value and `revents` must be identical between `--verify` runs.
- T4 (covers F1): a poll with a 50 ms timeout on a pipe nobody writes returns 0, and the guest's `CLOCK_MONOTONIC` advanced by at least 50 ms of logical time, identically in both runs.
- T5 (covers F8): an entry with `fd = -1` alongside a ready pipe: its `revents` is 0 and the count is 1.
- T6 (covers F7): poll a closed descriptor number with `events = POLLIN`: `revents == POLLNVAL` and the count is 1.
- T7 (covers F11, F12): `nfds` one above `RLIMIT_NOFILE` gives `EINVAL`; a two-entry array whose second `revents` lies on a read-only page gives `EFAULT` with the first `revents` written, under record and replay.
  Existing candidates: `record_poll_invalid_nfds_preserves_einval` (`hermit-cli/tests/record_replay.rs:2157`) and `record_poll_partial_revents_copyout` (`hermit-cli/tests/record_replay.rs:2137`), both using `tests/c/record_replay_poll_partial_copyout.c`.
- T8 (covers F13): a thread blocked in `poll(..., -1)` receives a signal whose handler uses `SA_RESTART`; poll must return `EINTR`, in both runs at the same logical point.
- T9 (covers F5): `POLLRDHUP` on a socketpair after the peer calls `shutdown(SHUT_WR)`, and `POLLPRI` on a regular file (never set).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> nondeterministic (readiness and wakeups depend on host scheduling, real time and external endpoints); target_policy model (the kernel decides readiness, Detcore turns waits into zero-timeout probes against logical-time deadlines and records/replays revents); 13 flag behaviours and 9 tests listed; open question on run-mode polls of external descriptors; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decisions 10, 13 and 14 (probe plus virtual-time wait is the target for internal and external descriptors; print_poll unwrap at io.rs:63 named as a guest-reachable panic); target_policy unchanged; no new F/T ids; removed the external-descriptor BlockingExternalIO question.
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T7 covered by hermit-cli/tests/record_replay.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `poll`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+poll&body=Discussion+for+the+%60poll%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpoll.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60poll%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `poll`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`linux_determinism` changed from the rendered `conditional` to `nondeterministic`: readiness and the end of a wait depend on host scheduling, real time and external endpoints, not only on the arguments and state the program controls.

### Open questions

- `tests/rust/poll.rs` is described by the test inventory as selected by a Rust integration test, but no `hermit-cli/tests` file names it with the literal `"poll"` (the only such literals, at `hermit-cli/tests/pidfd_creation.rs:25` and `hermit-cli/tests/record_replay.rs:2142`, are a list of syscall names and an argument to a C fixture). Which test runs it, and on which backend, is unconfirmed.
