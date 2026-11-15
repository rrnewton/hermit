---
schema: syscall-pages/v1
syscall: kill
nr_x86_64: 62
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/625
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+kill&body=Discussion+for+the+%60kill%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fkill.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60kill%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`kill(2)` sends a signal to a process, a process group or every process the caller may signal, or with signal 0 only checks that the target exists. Hermit resolves the target in its own thread model, forwards the cases whose recipient is unambiguous to the kernel, tells its scheduler about the pending signal, and refuses with `ENOSYS` the cases it cannot yet deliver deterministically: process groups (except `SIGKILL`), broadcast, and process-directed signals to a multithreaded process.

## Linux semantics

`pid > 0` names one process, `pid == 0` the caller's process group, `pid == -1` every process the caller may signal except init and itself, and `pid < -1` process group `-pid`. A process-directed signal is queued on the whole thread group; the kernel then wakes one thread that does not block it, chosen from live CPU state. Signal 0 performs the existence and permission checks without sending anything. Errors: `EINVAL` for an invalid signal number, `ESRCH` when no target exists, `EPERM` when the caller may signal none of the targets. A zombie (exited but not yet reaped) process still exists, so `kill` on it returns 0. When a process signals itself with an unblocked signal, at least one signal is delivered before `kill` returns.

### Flags and options

- F1: `sig == 0`: existence and permission probe; nothing is queued.
- F2: `pid > 0` naming a single-threaded process: the signal is queued for it.
- F3: `pid > 0` naming a multithreaded process: the kernel picks the recipient thread among those not blocking the signal.
- F4: `pid == 0`: every process in the caller's process group.
- F5: `pid == -1`: every process the caller may signal, except init and the caller.
- F6: `pid < -1` with `SIGKILL`: every process in process group `-pid`.
- F7: `pid < -1` with any other signal: every process in process group `-pid`.
- F8: an invalid signal number fails with `EINVAL`.
- F9: `ESRCH` when the PID or group has no process; a zombie target still counts as existing and returns 0.
- F10: `EPERM` when the caller lacks permission to signal the target.
- F11: self-signal: an unblocked signal sent to the caller's own process is delivered before `kill` returns.

## Determinism analysis

Several inputs to `kill` depend on host timing. Whether the target still exists, or is already a zombie or reaped, depends on how far the target has run. For a multithreaded target, the kernel picks the thread that handles the signal from which threads are running, blocked or masking it at that instant. When the signal lands in the target's execution, and so what the target has done by then, is also host-timed, as is the interleaving of deliveries to the members of a process group or broadcast set. A program that signals a single-threaded process it has synchronised with, or itself, sees a repeatable result; nearly anything else does not. `linux_determinism` stays `nondeterministic`.

## Hermit design

`target_policy` is `model`: Detcore resolves the target from its own thread tree, forwards only the cases with one possible recipient, and makes the delivery a scheduler event. The cases it cannot model yet are refused with `ENOSYS` rather than passed through. Code at hermit f4055eb9cae4:

- The dispatch arm is `detcore/src/lib.rs:2732`, which calls `handle_kill` at `detcore/src/syscalls/signal.rs:617-669`. Its doc comment (`signal.rs:610-616`) states the policy, and it carries `TODO-HUMAN-REVIEW` markers for https://github.com/rrnewton/hermit/pull/663, https://github.com/rrnewton/hermit/pull/1058 and https://github.com/rrnewton/hermit/pull/1119 (`signal.rs:606-609`).
- Without thread sequentialization, the call is forwarded unchanged (`signal.rs:622-624`).
- F1: signal 0 is forwarded for any `pid` (`signal.rs:626-628`), so the kernel answers the probe in the fixed PID namespace.
- F6: `can_forward_process_group_signal` (`signal.rs:233-239`) forwards `SIGKILL` to a process group (`pid < -1`) on backends whose guests use real namespace PIDs (`signal.rs:631-639`). `SIGKILL` cannot be blocked or caught, so the recipient choice does not matter.
- F4, F5, F7: every other `pid <= 0` returns `ENOSYS` (`signal.rs:640-642`).
- F2, F3, F9: `resolve_kill_targets` (`detcore/src/tool_global.rs:4161`) asks the scheduler's `process_signal_targets` (`detcore/src/scheduler.rs:3261-3269`), which returns the live threads of the group sorted by thread id, or nothing if `pid` is not a thread-group leader. `deterministic_kill_target` (`signal.rs:222-229`) then returns `ESRCH` for no thread, the thread for one, and `ENOSYS` for several.
- The call is forwarded to the kernel as `kill`, or as a `tgkill` to the resolved thread on backends that need thread-directed signals (`signal.rs:654-665`). F8 and F10 therefore come from the kernel.
- `notify_cross_task_signal` (`signal.rs:734-752`) then tells the scheduler that the target has a pending signal, unless the signal is 0 or the target is the caller (`signal.rs:831-833`). For `SIGKILL` the scheduler also records the process kill (`detcore/src/tool_global.rs:1679-1688`). This is what wakes a target parked in the scheduler, for example in a child wait.
- `signal.rs:646-651` reserves the exit for a process's `SIGKILL` to itself before the multithread check. It is active only on KVM (`signal.rs:578-580`).

Gaps, read from the code and not yet run:

- F3: a process-directed signal to a multithreaded process returns `ENOSYS` (`signal.rs:222-229`), even when only one thread could take it. Real programs that signal a threaded server or runtime fail this way.
- F9: `process_signal_targets` keeps only threads with an entry in `next_turns` (`scheduler.rs:3266`). A zombie child that the parent has not reaped would then give `ESRCH`, where Linux returns 0.
- F8: an invalid signal sent to a multithreaded process gets `ENOSYS` before the kernel can return `EINVAL`.

## Recommended tests

- T1 (covers F1, F9): `kill(pid, 0)` on a live child, on an exited but unreaped child, on a reaped child and on an unused PID; Linux gives 0, 0, `ESRCH`, `ESRCH`. Record what Hermit returns for the zombie.
- T2 (covers F2, F11): a process sends itself `SIGUSR1` with a handler installed and checks that the handler ran before `kill` returned; a parent and a single-threaded child signal each other; output must match across runs.
  Existing candidate: `tests/c/signal_determinism.c` (cell at `tests/e2e/manifests/c-programs.yaml:8142`, verify ci on ptrace), with `kill(getpid(), ...)` at lines 224 and 704 and `kill(getppid(), SIGUSR1)` at line 452.
- T3 (covers F3): a parent sends `SIGUSR1` to a child with two threads, one blocking the signal; record the result. Today it should be `ENOSYS`; once delivery is modeled, the non-blocking thread must take it in every run.
- T4 (covers F4, F5, F7): `kill(0, SIGUSR1)`, `kill(-1, SIGUSR1)` and `kill(-pgid, SIGTERM)`; record that each returns `ENOSYS`, and that no process received a signal.
- T5 (covers F6): `kill(-pgid, SIGKILL)` on a process group of two children; both must die, and the parent's `wait4` statuses must match across runs.
  Existing candidate: `tests/c/waitid_signal_interrupt.c` sends `SIGKILL` to a single child (`kill(child, SIGKILL)` at lines 149, 282 and 350), not to a group.
- T6 (covers F8): `kill(getpid(), 9999)` and `kill(child, 65)` return `EINVAL`.
  Existing candidate: `tests/c/errno_path_identity.c:120` (cell at `tests/e2e/manifests/system-utils.yaml:1787`, verify ci on ptrace) checks the self case.
- T7 (covers F10): with the child's UID changed by `setresuid`, `kill(child, SIGUSR1)` from an unprivileged parent must return `EPERM`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/625: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (Detcore resolves the recipient, forwards single-recipient and process-group SIGKILL cases, refuses group, broadcast and multithreaded targets with ENOSYS); linux_determinism stays nondeterministic; 11 flag behaviours and 7 tests listed; open questions on modeling the ENOSYS cases and on zombie targets; no coverage token yet because no test has been run against this list, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `kill`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+kill&body=Discussion+for+the+%60kill%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fkill.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60kill%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: kill" (https://github.com/rrnewton/hermit/issues/625), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### Open questions

- The `ENOSYS` refusals for multithreaded targets (F3) and for `pid == 0`, `pid == -1` and non-`SIGKILL` process groups (F4, F5, F7) are a design decision for the owner: should Detcore model the kernel's recipient choice (the first thread, by thread id, that does not block the signal) so that these cases are delivered, or should they stay refused? The doc comment at `detcore/src/syscalls/signal.rs:610-616` says they remain refused "until Detcore models eligible signal masks".
- Does `kill` on an unreaped zombie child return `ESRCH` under Hermit (`detcore/src/scheduler.rs:3261-3269`)? T1 should settle it; if so, it is a defect to file.
