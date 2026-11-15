---
schema: syscall-pages/v1
syscall: wait4
nr_x86_64: 61
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+wait4&body=Discussion+for+the+%60wait4%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwait4.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60wait4%60%2C+or+in+what+the+page+says%3F%0A
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

`wait4(2)` waits for a state change in a child selected by PID or process group, reaps it if it terminated, and optionally returns its status word and resource usage (glibc's `wait` and `waitpid` use it). Hermit decides in its scheduler which child is ready and when, then reaps that exact child through the kernel, and writes an all-zero `rusage`.

## Linux semantics

`pid` selects the children to wait for, `options` selects the kinds of state change and whether to block, `wstatus` receives the encoded status (exit code, terminating signal and core flag, stop or continue), and `rusage` receives the reaped child's accumulated CPU time, page faults, context switches and similar counters. The call returns the reaped or reported child's PID, 0 for `WNOHANG` with matching children but none ready, or an error. A blocking wait sleeps until a matching child changes state; a signal with a handler interrupts it with `EINTR`, or restarts it under `SA_RESTART`. When several matching children are ready at once, the kernel reports the first one in its children list.

### Flags and options

- F1: `pid > 0`: wait for that exact child.
- F2: `pid == -1`: wait for any child.
- F3: `pid == 0`: wait for any child in the caller's process group.
- F4: `pid < -1`: wait for any child in process group `-pid`; `pid == INT_MIN` fails with `ESRCH`.
- F5: `WNOHANG`: return 0 at once if matching children exist but none has changed state.
- F6: `WUNTRACED`: also report children that stopped (`WIFSTOPPED`).
- F7: `WCONTINUED`: also report stopped children resumed by `SIGCONT` (`WIFCONTINUED`).
- F8: `__WCLONE` waits only for "clone" children (exit signal other than `SIGCHLD`); `__WALL` waits for both kinds.
- F9: `__WNOTHREAD`: wait only for children of the calling thread, not of other threads in its group.
- F10: an option bit outside those six (including `WEXITED`, `WSTOPPED` and `WNOWAIT`, which only `waitid` accepts) fails with `EINVAL`.
- F11: `wstatus`: NULL is allowed; otherwise it receives the encoded exit, signal, core-dump, stop or continue status.
- F12: `rusage`: NULL is allowed; otherwise it receives the reaped child's resource usage.
- F13: `ECHILD` when no child matches the selector; `EFAULT` for an unwritable `wstatus` or `rusage`.
- F14: a signal delivered during a blocking wait gives `EINTR`, or a restart under `SA_RESTART`.

## Determinism analysis

Which child is reported, and whether a non-blocking call sees it at all, depends on when each child reaches its state change, which host scheduling decides. A parent that waits for "any" child and prints the PIDs or statuses in reaping order produces different output from run to run; `make -jN` and shell job control are common examples. A `WNOHANG` poll returns 0 or a PID depending on timing. The `rusage` block holds host CPU time and context-switch counts, which differ on every run. An exact-PID blocking wait whose `rusage` is NULL repeats exactly; nearly everything else does not. `linux_determinism` is `nondeterministic`.

## Hermit design

`target_policy` is `model`: Detcore's scheduler decides which child is ready and when, and the kernel only performs the reap of that exact child. Code at hermit f4055eb9cae4:

- The dispatch arm is `detcore/src/lib.rs:2439`, which calls `handle_wait4` at `detcore/src/syscalls/threads.rs:1584-1776`. The doc comment (`threads.rs:1584-1586`) says the call is handled by the scheduler, not by the record/replay layer.
- `threads.rs:1602-1619` builds a selector from `pid`: exact (F1), any (F2), the caller's own process group (F3), or process group `-pid` (F4). `terminal_child_wait_spec` (`threads.rs:513-530`) maps `__WALL`/`__WCLONE` to the exit class and `__WNOTHREAD` to the calling thread (F9).
- If the options contain `WUNTRACED`, `WCONTINUED`, `__WCLONE` or `__WALL`, there is no selector (`threads.rs:1602-1608`), and the call goes through the legacy path: `retry_nonblocking_syscall` at `threads.rs:1748-1752`, which polls the kernel at scheduler turns. The comment at `threads.rs:1597-1601` says stop and continue events need a backend callback first. F6-F8 are therefore served, but not by the modeled path.
- `ready_child_wait` at `detcore/src/scheduler.rs:3207-3212` scans `logically_exited_processes`, a `BTreeSet<DetPid>` (`scheduler.rs:700`), so an any-child wait reports the matching child with the lowest PID among those the scheduler has seen exit. That order is fixed, and usually agrees with Linux's oldest-first children list.
- F5: `threads.rs:1630-1667`. With a ready child, Detcore waits for its physical exit and reaps it with an exact-PID call (`threads.rs:1648-1661`). With matching children but none ready, it returns 0. With no matching child and complete process lineage (`backend_tracks_process_children`, `threads.rs:1622`), it returns `ECHILD` without asking the kernel (F13).
- Blocking waits: `threads.rs:1668-1747`. Detcore blocks the guest's ordinary signals with `block_signals_for_disposition` (`threads.rs:1681-1682`), loops on `wait_for_child_lifecycle` (`threads.rs:1690`), reaps the ready child by exact PID (`threads.rs:1701-1703`), and turns a pending handled signal into `EINTR` or a restart through `interrupted_child_wait_result` (F14).
- `threads.rs:1768-1774` writes an all-zero `rusage` when the call reaped a child and `rusage` is non-NULL (F12), because host CPU and scheduling counters are not deterministic. The status word (F11) is the kernel's; it is read back at `threads.rs:1756` to decide whether the child was consumed.

Two behaviours read from the code differ from Linux; neither has been run:

- `pid == INT_MIN`: `threads.rs:1616` computes `-pid`, which overflows. The `validate` build profile enables overflow checks (top-level `Cargo.toml`, `[profile.validate]`), so that build would panic; a build without them waits on process group `INT_MIN` instead of returning `ESRCH`.
- An invalid option bit (F10) is passed through unchanged. With no matching child, Detcore returns `ECHILD` before the kernel can return `EINVAL` (`threads.rs:1638-1644` for `WNOHANG`, `threads.rs:1725-1727` for a blocking wait).

Target (owner decisions 6, 10 and 14): `handle_wait4` makes Linux's argument checks itself, in `kernel_wait4()`'s order (`kernel/exit.c`), before any selector is built: an option bit outside `WNOHANG|WUNTRACED|WCONTINUED|__WNOTHREAD|__WCLONE|__WALL` gives `EINVAL` (F10), then `pid == INT_MIN` gives `ESRCH` (F4), then the child search, with `ECHILD` (F13) last. Decision 10 makes the `INT_MIN` negation at `threads.rs:1616` a defect in every build profile, because guest input must never crash the tracer; the guard must come before the negation, not only behind an overflow-checking profile. Both are fixed after this pin by https://github.com/rrnewton/hermit/pull/3342 (merged 2026-10-03 as 9dfff2fe0887e6f85c6b6deb54a4d6eb25641ab7), whose `validate_wait4_arguments` runs first in `handle_wait4`; this body's facts stay at f4055eb9cae4 until the next facts pin, and T4 is the evidence. Decision 14 needs no change in form: a blocking wait is already a probe plus a scheduler wait, on scheduler lifecycle events in the modeled path (`wait_for_child_lifecycle`, `threads.rs:1690`) and as a non-blocking poll retried at scheduler turns in the legacy path (`retry_nonblocking_syscall`, `threads.rs:1748-1752`, `detcore/src/syscalls/helpers.rs:1414`). `target_policy` stays `model`.

## Recommended tests

- T1 (covers F1, F11, F12): an exact-PID `wait4` on a child that exits with a known code, with `wstatus` and a non-NULL `rusage`; the status must decode with `WEXITSTATUS`, and `rusage` must read as all zeros in every run.
  Existing candidate: `tests/c/dbt_wait_accounting.c` (cell at `tests/e2e/manifests/c-programs.yaml:17831`, verify ci on ptrace, expected stdout `wait4=7 waitid=9 reaped=2 cpu=zero`), which drives `tests/c/dbt_wait_lifecycle.c` (`wait4` with `rusage` at line 60).
- T2 (covers F2, F5, F13): several children exit in a racy order; the parent loops on `wait4(-1, ..., WNOHANG)` and then blocks on `wait4(-1)`; the reaping order and every return value (0, PIDs, final `ECHILD`) must match across runs.
  Existing candidate: `tests/c/dbt_wait_lifecycle.c:92-94` checks that a `WNOHANG` wait for an already-reaped child returns `ECHILD`.
- T3 (covers F3, F4, F9): waits by process group zero, by negative process group, and with `__WNOTHREAD` from a thread that did not create the child (which must get `ECHILD`) and from the creating thread.
  Existing candidate: `tests/c/terminal_child_selectors.c` (negative process group at line 137, group zero at 144, `WNOHANG|__WNOTHREAD` at 193, cross-thread at 198, `__WNOTHREAD` sibling at 232), run on ptrace by `ptrace_terminal_child_selectors_follow_linux_contract` at `hermit-cli/tests/terminal_child_selectors.rs:68`. That file also has a KVM test, so it cannot serve as a coverage path; a ptrace-only cell is needed.
- T4 (covers F4, F10): raw `syscall(SYS_wait4, INT_MIN, ...)` must return `ESRCH`, and options `WEXITED`, `WNOWAIT` and the unassigned bit `0x10000000` must return `EINVAL` (`0x40000000` is `__WALL`, which wait4 accepts), and `__WALL` alone must not, both with and without a live child.
- T5 (covers F6, F7): a child stopped with `SIGSTOP` and resumed with `SIGCONT`; `wait4` with `WUNTRACED` must report the stop and with `WCONTINUED` the continue, identically across runs.
- T6 (covers F8): a child created by `clone` with exit signal 0 is invisible to a plain `wait4`, found by `__WCLONE` and by `__WALL`.
- T7 (covers F11): a child killed by a signal, with and without a core dump; `WIFSIGNALED`, `WTERMSIG` and `WCOREDUMP` must be identical across runs.
  Existing candidate: `tests/c/signal_waitstatus_identity.c` (cell at `tests/e2e/manifests/c-programs.yaml:16768`, verify ci on ptrace).
- T8 (covers F13): `wait4` with an unmapped `wstatus` or `rusage` must return `EFAULT` without crashing Hermit, and the child must remain reapable if the kernel did not consume it.
- T9 (covers F14): a blocking `wait4` interrupted by a handled signal returns `EINTR` without `SA_RESTART` and keeps waiting with it; the point of interruption must match across runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (scheduler picks the ready child, kernel reaps it by exact PID, rusage zeroed); linux_determinism conditional -> nondeterministic (reaping order, WNOHANG results and rusage depend on host timing); 14 flag behaviours and 9 tests listed; open questions on pid INT_MIN, invalid option bits and the legacy stop/continue path; no coverage token yet because no test has been run against this list, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6, 10 and 14 applied: target is Linux's kernel_wait4 order (EINVAL for unknown options, then ESRCH for INT_MIN) checked by Detcore, and the INT_MIN negation at detcore/src/syscalls/threads.rs:1616 is a defect in every profile (decision 10); both fixed after this pin by https://github.com/rrnewton/hermit/pull/3342; blocking waits already use the probe-plus-scheduler-wait form; target_policy stays model; open questions on INT_MIN and option order removed
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/dbt_wait_lifecycle.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `wait4`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+wait4&body=Discussion+for+the+%60wait4%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fwait4.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60wait4%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `wait4`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`handle_wait4` carries `TODO-HUMAN-REVIEW(PR-587)` on its `rusage` handling (`detcore/src/syscalls/threads.rs:1586`). `waitid` shares the child-wait model; its handler starts at `threads.rs:1783`.

### Open questions

- `WUNTRACED`, `WCONTINUED`, `__WCLONE` and `__WALL` still use the legacy polling path (`threads.rs:1597-1608`). Is moving them onto the modeled path planned, and does the polling path give the same answer in every run when a stop and an exit race?
