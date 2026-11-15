---
schema: syscall-pages/v1
syscall: waitid
nr_x86_64: 247
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+waitid&body=Discussion+for+the+%60waitid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwaitid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60waitid%60%2C+or+in+what+the+page+says%3F%0A
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

`waitid(idtype, id, infop, options, rusage)` waits for a child selected by PID, process group, pidfd or "any", to exit, stop or continue, and reports the event in a `siginfo_t`. Detcore handles it in the scheduler (`detcore/src/syscalls/threads.rs:1783`): it decides when the wait can finish, then issues a non-blocking kernel probe to read the event. It zeroes host CPU accounting in the result and rejects a few forms Linux allows.

Manual reference: Linux man-pages waitid(2) and wait(2).

## Linux semantics

The kernel checks `options` first, then `idtype` and `id`. It then scans the caller's children, and with `__WNOTHREAD` unset, the children of every thread in its thread group, for one that matches the selector and has a pending event of a requested kind. If none is ready and `WNOHANG` is not set, the caller sleeps until a child changes state or a signal arrives. A ready child wins over a pending signal.

On success the call returns 0. When a child is reported, the kernel writes six fields of `*infop`: `si_signo` (`SIGCHLD`), `si_errno` (0), `si_code` (`CLD_EXITED`, `CLD_KILLED`, `CLD_DUMPED`, `CLD_STOPPED`, `CLD_TRAPPED` or `CLD_CONTINUED`), `si_pid`, `si_uid` and `si_status`. It does not write `si_utime` or `si_stime`. With `WNOHANG` and nothing ready, it writes the same six fields as zeros. A terminal event reaps the child unless `WNOWAIT` is set. The fifth argument, `rusage`, is Linux-specific: the kernel fills it with the reported child's resource usage.

### Flags and options

- F1: `P_PID` (1): wait for the child with PID `id`. An `id` of 0 or less gives `EINVAL`.
- F2: `P_ALL` (0): wait for any child; `id` is ignored.
- F3: `P_PGID` (2): wait for any child in process group `id`; `id == 0` means the caller's group (Linux 5.4). A negative `id` gives `EINVAL`.
- F4: `P_PIDFD` (3, Linux 5.4): wait for the child the pidfd `id` refers to. A descriptor that is not a pidfd gives `EBADF`. If the pidfd is `O_NONBLOCK` and the child is not ready, `EAGAIN` (Linux 5.10).
- F5: invalid `idtype`: `EINVAL`.
- F6: `WEXITED`: report children that exited or were killed by a signal; the child is reaped unless `WNOWAIT` is set.
- F7: `WSTOPPED`: report children stopped by a signal, or stopped under ptrace (`CLD_TRAPPED`).
- F8: `WCONTINUED`: report stopped children resumed by `SIGCONT`.
- F9: `WNOHANG`: return 0 at once if no matching child is ready, with the reported fields of `*infop` zero.
- F10: `WNOWAIT`: report the event but leave the child waitable, so a later wait sees it again.
- F11: `__WNOTHREAD`, `__WCLONE`, `__WALL`: restrict the scan to the calling thread's own children, wait only for clone children (those that do not send `SIGCHLD` on exit), or wait for all children.
- F12: invalid `options`: no event bit (`WEXITED`, `WSTOPPED`, `WCONTINUED`), or any unknown bit, gives `EINVAL`. This check runs first.
- F13: `infop`: the six fields above are written; NULL is accepted; an unwritable pointer gives `EFAULT`.
- F14: `rusage` (Linux-specific): when a child is reported, filled with that child's resource usage; NULL skips it.
- F15: `ECHILD`: no child matches the selector, including when `SIGCHLD` is set to `SIG_IGN` or has `SA_NOCLDWAIT`, so exited children are reaped automatically.
- F16: signals: a handled signal during a blocking wait gives `EINTR`, or restarts the call when the handler has `SA_RESTART`. A child that is already ready is reported instead of the interruption.

## Determinism analysis

`linux_determinism: conditional`. Which child is reported, and whether a `WNOHANG` call finds one, depends on the order in which the host schedules the children and the parent. `P_ALL` and `P_PGID` waits with several children can report them in different orders on different runs. `rusage` and the CPU time inside it are host measurements. Signal arrival decides whether a blocking wait ends with `EINTR`. The call repeats when only one child can match, or when the children's events are ordered by something other than timing, and the caller ignores `rusage`.

### Nondeterminism sources

- Host scheduling of the children and the parent, which decides which child is ready first and whether `WNOHANG` finds one
- Signal arrival time, which decides `EINTR` and restarts during a blocking wait
- Host CPU accounting in `rusage`, and in the `si_utime` and `si_stime` bytes of the caller's buffer, which Linux leaves unwritten
- External events that stop or continue a child, such as a `SIGSTOP` from outside, for `WSTOPPED` and `WCONTINUED`

## Hermit design

Target policy: `model`. The kernel keeps the process tree and the child's exit state, and Detcore owns every nondeterministic input: when the wait can finish, which child is chosen, signal precedence, and the accounting fields. The handler at `detcore/src/syscalls/threads.rs:1783` is dispatched from `detcore/src/lib.rs:2440`, and `waitid` is in the Determinized list (`detcore/src/syscall_classification.rs:222`). Its doc comment says the call "is handled by the scheduler and not passed to the record/replay layer" (`detcore/src/syscalls/threads.rs:1782`). In order:

- `options` is checked first, as on Linux: `EINVAL` without an event bit or with an unknown bit (`detcore/src/syscalls/threads.rs:1800`) (F12).
- A NULL `infop` gives `EFAULT` (`detcore/src/syscalls/threads.rs:1808`). The comment explains this is deliberate: Linux accepts NULL, but that form needs backend-neutral scratch memory for polling and could leak host `rusage`.
- A blocking `P_PIDFD` wait reads the descriptor's flags from `/proc/<pid>/fdinfo` (`guest_fd_status_flags`, `detcore/src/syscalls/threads.rs:722`). `O_NONBLOCK` turns it into a one-shot probe, which gives `EAGAIN` as on Linux (F4). A blocking pidfd gives `EOPNOTSUPP` (`detcore/src/syscalls/threads.rs:1841`), because a numeric pidfd cannot hold Linux's file reference if another thread closes and reuses it.
- Waits that ask only for `WEXITED`, without `WSTOPPED`, `WCONTINUED`, `__WCLONE` or `__WALL`, go to the lifecycle scheduler (`detcore/src/syscalls/threads.rs:1814`). For `P_PID`, `P_ALL` and `P_PGID` the selector is built at `detcore/src/syscalls/threads.rs:1858`, with `P_PGID 0` resolved through Detcore's own process-group record (`detcore/src/syscalls/threads.rs:1866`). The scheduler picks the ready child, so the order is fixed by the deterministic schedule (F1, F2, F3, F6). When the backend tracks every child, Detcore returns `ECHILD` itself when no child matches (`detcore/src/syscalls/threads.rs:1903`) (F15).
- `WNOHANG` and non-blocking pidfd waits run as one probe (`detcore/src/syscalls/threads.rs:1887`). If a matching child exists but none is ready, Detcore writes a zeroed `siginfo_t` and returns 0 (`detcore/src/syscalls/threads.rs:1899`) (F9).
- Blocking waits block ordinary signals around each probe, then poll the kernel with `options | WNOHANG` (`detcore/src/syscalls/threads.rs:1967`), yielding to the scheduler between attempts. A child that is ready at the probe wins over a pending signal. An interruption becomes `EINTR` (`detcore/src/syscalls/threads.rs:2093`), or a restart when `SA_RESTART` applies (`interrupted_child_wait_result`, `detcore/src/syscalls/threads.rs:687`) (F16).
- When a child is reported, `finish_waitid_result` (`detcore/src/syscalls/threads.rs:446`) zeroes `si_utime` and `si_stime` (`canonicalize_waitid_siginfo`, `detcore/src/syscalls/threads.rs:429`) and writes a zeroed `rusage` if one was passed (`detcore/src/syscalls/threads.rs:470`) (F13, F14). After a terminal event without `WNOWAIT`, Detcore drops the child from its records (`detcore/src/syscalls/threads.rs:2103`) (F10).

Why `model` and not the alternatives: `passthrough` would let a thread sleep in the kernel where the scheduler cannot see it, and the draft fix https://github.com/rrnewton/hermit/pull/2246 measured exactly that, an indefinite spin when a signal arrived during a blocking `waitid` (`ai_docs/2026-08-13-coordinator-incident-cleanup.md`). `emulate` would need Detcore to own the whole process tree, and `refuse` would break every shell and process supervisor.

Deliberate differences from Linux, kept as design choices:

- NULL `infop` gives `EFAULT`, where Linux accepts it (F13).
- A blocking `P_PIDFD` wait on a blocking pidfd gives `EOPNOTSUPP`, where Linux waits (F4).
- `rusage` is all zeros, and `si_utime` and `si_stime` are 0, where Linux reports host CPU time (F14).
- Detcore zeroes the whole `siginfo_t` before every probe (`detcore/src/syscalls/threads.rs:1916`), so bytes Linux would leave untouched, including `si_utime` and `si_stime`, read as 0.

What remains before the code fully realizes `model`:

- **Non-terminal waits still poll the kernel.** `WSTOPPED`, `WCONTINUED`, `__WCLONE` and `__WALL` stay on the older kernel-polling path, and `P_PGID 0` is resolved there from a `/proc` snapshot taken at entry (`snapshot_process_group`, `detcore/src/syscalls/threads.rs:710`). The comments at `detcore/src/syscalls/threads.rs:1812` and `:1855` say this lasts until the backends report stop and continue events and register clone children as processes (F7, F8, F11).
- **An invalid non-null `infop` faults too early.** The known limitation at `detcore/src/syscalls/threads.rs:1848` says it faults on the first probe, before any child is waitable; Linux faults only when it has something to write (F13).
- **`P_PID` with `id <= 0` may give `ECHILD` instead of `EINVAL`.** The scheduler builds an exact selector for any `id` (`detcore/src/syscalls/threads.rs:1861`); when the backend tracks every child and no child has that PID, Detcore returns `ECHILD` before the kernel sees the call. This is a reading of the code, not a measurement (F1, F5).
- **Record and replay** do not see `waitid`; the result is reproduced by re-running the children under the same schedule.

The code tags `TODO-HUMAN-REVIEW(#274)` and `TODO-HUMAN-REVIEW(#2246)` (`detcore/src/syscalls/threads.rs:1779`, `:1780`) name https://github.com/rrnewton/hermit/pull/274, which added this handler, and https://github.com/rrnewton/hermit/pull/2246, the signal-progress fix. A follow-up change landed as https://github.com/rrnewton/hermit/pull/2534.

## Recommended tests

- T1 (covers F1, F6, F9, F12, F13, F14): `EINVAL` for `options == 0`; `WNOHANG` with no event returns 0 with `si_pid == 0` and leaves `rusage` untouched; a blocking `P_PID` wait reports `CLD_EXITED`, status 42, `si_utime == si_stime == 0`, and a zeroed `rusage`; NULL `infop` gives `EFAULT`.
  Existing candidate: `waitid_polls_until_child_exit_and_supports_wnohang` at `detcore/tests/misc/mod.rs:1043` (cargo test target `tests_misc`, `detcore/Cargo.toml:42`). It runs through Detcore's in-process test harness, not on the ptrace backend.
- T2 (covers F2, F3, F15): `P_ALL`, explicit `P_PGID` and `P_PGID 0` waits report the right child; a wait with no matching child gives `ECHILD`.
  Existing candidate: `ptrace_terminal_child_selectors_follow_linux_contract` at `hermit-cli/tests/terminal_child_selectors.rs:68`, running `tests/c/terminal_child_selectors.c` on ptrace (`P_PGID` waits at `tests/c/terminal_child_selectors.c:154` and `:165`).
- T3 (covers F4): an `O_NONBLOCK` pidfd gives `EAGAIN` before the child exits, then reports status 42; a blocking pidfd gives `EOPNOTSUPP`; a non-pidfd descriptor gives `EBADF`.
  Existing candidates: `pidfd_creation_is_tracked_across_descriptor_operations` at `hermit-cli/tests/pidfd_creation.rs:35` with `tests/c/pidfd_waitid_child.c`, and cell `c-programs/pidfd-waitid-child` (`tests/e2e/manifests/c-programs.yaml:5219`, verify ci on ptrace); the `EOPNOTSUPP` case is in the T1 test. The `EBADF` case still needs a test.
- T4 (covers F7, F8, F16): `WSTOPPED` and `WCONTINUED` waits interrupted by a signal return without a panic.
  Existing candidates: `waitid_wcontinued_signal_interrupts_without_panicking` at `hermit-cli/tests/waitid_signal_interrupt.rs:650` and `waitid_wstopped_signal_interrupts_without_panicking` at `hermit-cli/tests/waitid_signal_interrupt.rs:668`. Neither checks the reported `CLD_STOPPED` or `CLD_CONTINUED` event; a test that stops and continues a child and checks both is still needed.
- T5 (covers F16): a blocking wait interrupted by a handled signal returns `EINTR`; with `SA_RESTART` it restarts and the signal mask is preserved; a ready child is reported ahead of the signal.
  Existing candidates in `hermit-cli/tests/waitid_signal_interrupt.rs`: `waitid_interrupted_by_a_signal_returns_instead_of_spinning` (line 462), `waitid_live_sibling_signal_interrupts_without_spinning` (line 491), `waitid_live_sibling_signal_honors_sa_restart_and_preserves_mask` (line 525), `waitid_honors_sa_restart_after_running_the_handler` (line 627), and `waitid_ready_child_path_is_ptrace_l2` (line 686).
- T6 (covers F10, F11): `WNOWAIT` reports the exit and a second `waitid` reports it again; `__WALL` and `__WCLONE` waits on a `clone` child without `SIGCHLD` report its exit; `__WNOTHREAD` ignores a sibling thread's child.
- T7 (covers F1, F5): `P_PID` with `id` 0 and -1, `P_PGID` with `id` -1, and `idtype` 7 give `EINVAL`. Record whether Hermit gives `ECHILD` for `P_PID 0`, as the code suggests.
- T8 (covers F6, F13): `waitid(P_PID, child, &info, WEXITED)` under `hermit run --verify` on ptrace, with both runs printing identical output.
  Existing candidate: `tests/c/dbt_wait_lifecycle.c:79`, cell `c-programs/dbt-wait-lifecycle` (`tests/e2e/manifests/c-programs.yaml:1238`, verify ci on ptrace).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (the kernel keeps the process tree; Detcore's scheduler decides when the wait finishes and which child is chosen, and zeroes host CPU accounting); linux_determinism stays conditional; related gains https://github.com/rrnewton/hermit/pull/274, https://github.com/rrnewton/hermit/pull/2246 and https://github.com/rrnewton/hermit/pull/2534; 16 flag behaviours and 8 tests listed; deliberate differences (NULL infop EFAULT, blocking pidfd EOPNOTSUPP) and remaining gaps (non-terminal waits still poll the kernel, early infop fault, P_PID id<=0 may give ECHILD) recorded; no coverage token because no test has been run against this list, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T8 covered by tests/c/dbt_wait_lifecycle.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `waitid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+waitid&body=Discussion+for+the+%60waitid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwaitid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60waitid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `waitid`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
