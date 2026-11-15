---
schema: syscall-pages/v1
syscall: setsid
nr_x86_64: 112
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setsid&body=Discussion+for+the+%60setsid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetsid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setsid%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`setsid()` makes the calling process the leader of a new session and of a new process group, detached from any controlling terminal; daemons and terminal multiplexers call it. Detcore handles it (`detcore/src/syscalls/misc.rs:564`): the kernel performs the call, and on success Detcore records the new session and group in its own process table. With `--kill-daemons` the handler also marks the caller as a daemon.

Manual reference: [setsid(2)](https://man7.org/linux/man-pages/man2/setsid.2.html).

## Linux semantics

The kernel (`kernel/sys.c`, `ksys_setsid`) acts on the caller's thread group. It fails with `EPERM` when a process group whose ID equals the caller's pid already exists, which is always true when the caller is already a group leader; otherwise it creates a new session and a new process group, both with the caller's pid as their ID, drops any controlling terminal association for the new session, and returns the new session ID (the caller's pid).

The new session has no controlling terminal. Afterwards `open("/dev/tty")` fails with `ENXIO`, and the session leader may acquire a terminal by opening one without `O_NOCTTY`. Children created after the call inherit the session and group. Because the caller has left its old session, a `setpgid` that names one of the old session's groups now fails with `EPERM`.

The call takes no arguments; its result depends only on whether the caller is a group leader and on the caller's pid.

### Flags and options

- F1: success when the caller is not a process-group leader: a new session and process group are created with the caller's pid as their ID, and the call returns that pid.
- F2: `EPERM` when a process group with the caller's pid as its ID already exists (the caller is a group leader, for example after `setpgid(0, 0)` or as the initial process of a job).
- F3: called from a thread that is not the thread-group leader: the call still acts on the whole thread group and returns the thread-group leader's pid.
- F4: effects after success: the new session has no controlling terminal (`open("/dev/tty")` gives `ENXIO`), later `fork` children inherit the session and group, and `setpgid` into a group of the old session gives `EPERM`.

## Determinism analysis

`linux_determinism: conditional`. No legacy classification existed (see Additional information); this is the judgement for v1. The success/`EPERM` outcome is a pure function of the caller's group-leader state, which is itself the result of earlier `fork`, `setpgid` and `setsid` calls, and the success value is the caller's pid. The call repeats when the process tree, the order of those calls and the pid values repeat.

### Nondeterminism sources

- PID allocation: the return value is the caller's pid
- Group-leader state at the time of the call, which depends on the order of earlier `setpgid` calls in a parent/child race
- Whether a controlling terminal existed beforehand, which depends on how the program was launched (an interactive terminal or not); this decides what F4's terminal detachment observably changes

## Hermit design

Target policy: `model`. The kernel performs the call and keeps the live session state; Detcore mirrors the result. The code at hermit f4055eb9cae4 does this:

- `handle_setsid` injects the call unchanged (`detcore/src/syscalls/misc.rs:569`), so the `EPERM` rule (F2) and the thread-group behaviour (F3) are the kernel's. A failure returns through `?` before the mirror is touched.
- On success it calls `create_session` for the caller's process (`detcore/src/syscalls/misc.rs:570-571`), which sets both the mirrored session and the mirrored process group to the caller's pid (`detcore/src/scheduler.rs:1081-1088`, reached through the global request at `detcore/src/tool_global.rs:4135`). The process group half matters: Detcore's `wait4(0)` and terminal-events-only `waitid(P_PGID, 0)` selectors read the mirrored group (`detcore/src/syscalls/threads.rs:1613`, `detcore/src/syscalls/threads.rs:1864-1867`), so after `setsid` they select by the new group.
- New processes inherit both mirrored fields from their parent (`detcore/src/scheduler.rs:1028-1029`), which matches F4's inheritance.
- With `--kill-daemons` (`detcore-model/src/config.rs:491-494`: "Kill all remaining tasks iff daemons are the only ones left. Disabled by default.") the handler calls `guest.daemonize()` after the call (`detcore/src/syscalls/misc.rs:575-577`), so a process that detached into its own session does not keep a Hermit run alive once only daemons remain. This is Hermit run-lifetime policy, not Linux behaviour.

Why `model` and not the alternatives:

- `passthrough` would leave the mirrored process group stale after the caller changes group, so `wait4(0)` would select by the old group.
- `emulate` cannot work: the kernel must create the session for terminal semantics and for later `setpgid` validation.
- `refuse` would break every daemon.

Gaps:

- The mirrored session ID is read only to copy it into children (`detcore/src/scheduler.rs:1029`) and by a scheduler test (`detcore/src/scheduler/exec_teardown_tests.rs:271`), so session membership is not used for any Detcore decision; validation of session rules is left to the kernel.
- `create_session` returns `false` when the caller is missing from the process table (`detcore/src/scheduler.rs:1082-1084`), and the handler discards the result (`detcore/src/syscalls/misc.rs:571`).
- `getsid` passes through. Its stable value under Hermit comes from the PID namespace, as `tests/c/session_identity.c:35-43` records.
- The handler uses `guest.inject`, not `record_or_replay`, so replay re-executes the call against the kernel. That is needed: the session is live kernel state.

## Recommended tests

- T1 (covers F1, F4): fork a child that is not a group leader and have it call `setsid`; check the return value equals `getpid()`, `getsid(0)` and `getpgid(0)` both equal it, `open("/dev/tty", O_RDWR)` fails with `ENXIO`, a grandchild forked afterwards reports the same session, and a `setpgid` into the parent's group fails with `EPERM`. Then from the child check that `wait4(0)` reaps the grandchild. Run under `hermit run --verify` on ptrace.
- T2 (covers F2): call `setsid` from a group leader and check `EPERM`.
  Existing partial coverage: `tests/c/session_identity.c:79-85` calls `setsid` after `setpgid(0, 0)` and prints the `EPERM` result (cell `c-programs/session-identity`, `tests/e2e/manifests/c-programs.yaml:7993`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:8023-8025`), and `sidecar_preserves_session_and_process_group_identity` (`hermit-cli/tests/run_evidence.rs:526`) asserts that line.
- T3 (covers F3): in a forked non-leader child, call `setsid` from a second thread; check that the return value is the thread-group leader's pid and that `getsid(0)` from the main thread agrees.
- T4 (covers none): with `--kill-daemons`, a child that calls `setsid` and then blocks forever must not keep the run alive after the parent exits; without the flag the same program must wait. This checks Hermit policy, not a Linux behaviour.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (the kernel creates the session; Detcore mirrors session and group for its wait selectors, plus --kill-daemons); linux_determinism conditional confirmed; 4 flag behaviours and 4 tests listed; no coverage token, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setsid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setsid&body=Discussion+for+the+%60setsid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetsid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setsid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `setsid`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The Linux semantics above are from the kernel source (`kernel/sys.c`) as known to the author; no man page or kernel source was installed on the host used for this pass, so T1 to T3 should confirm them.
