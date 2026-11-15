---
schema: syscall-pages/v1
syscall: setpgid
nr_x86_64: 109
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/435
  - https://github.com/rrnewton/hermit/issues/449
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setpgid&body=Discussion+for+the+%60setpgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetpgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setpgid%60%2C+or+in+what+the+page+says%3F%0A
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

`setpgid(pid, pgid)` moves a process into a process group, or makes it the leader of a new one; shells use it for job control, and the group then selects targets for `kill(-pgid)`, `wait4(0 or -pgid)` and `waitid(P_PGID)`. Detcore handles it (`detcore/src/syscalls/misc.rs:584`): the kernel validates and applies the change, and on success Detcore mirrors the new group into its own process table, which its child-wait selectors read instead of host `/proc` state.

Manual reference: [setpgid(2)](https://man7.org/linux/man-pages/man2/setpgid.2.html).

## Linux semantics

The kernel (`kernel/sys.c`, `setpgid`) resolves both arguments in the caller's PID namespace and then applies these rules in order:

1. `pid == 0` means the calling process. The kernel uses the thread-group leader, so a call from any thread acts on the whole process.
2. `pgid == 0` means "use the target's pid", which makes the target the leader of a group named after itself.
3. `pgid < 0` is `EINVAL`.
4. The target must be the caller itself or a child of the caller; otherwise `ESRCH`. A target that is not a thread-group leader is `EINVAL`.
5. A child target must be in the caller's session (`EPERM` otherwise) and must not have called `execve` yet (`EACCES` otherwise).
6. A session leader cannot change its group (`EPERM`).
7. When `pgid != pid`, a process group with that ID must already exist in the caller's session; otherwise `EPERM`.

On success the target's group changes immediately. The group ID is inherited across `fork` and kept across `execve`. Only the process's own group membership changes; existing members of the old group are untouched. The self-targeted host probe under Additional information, `setpgid(0, 0) = 0`, created or kept the caller's own group.

Because both a shell and its child usually call `setpgid` on the child (so that the group exists whichever runs first), the classic outcome of a job-control launch depends on which of the two calls runs first, and on whether the child has already reached `execve` when the parent's call runs.

### Flags and options

- F1: `pid == 0`: act on the calling process. From a non-leader thread it still acts on the thread-group leader.
- F2: `pgid == 0`: use the target's pid as the group ID, making the target a group leader.
- F3: target is a child of the caller in the same session that has not called `execve`: allowed. After the child's `execve`, the same call fails with `EACCES`.
- F4: `pgid` names an existing group in the caller's session (`pgid != pid`): the target joins it. This is how a shell puts every member of a pipeline into one job.
- F5: target is neither the caller nor a child of the caller, or no such pid exists: `ESRCH`.
- F6: `EPERM` cases: the target is a session leader; a child target is in a different session; `pgid != pid` and no group with that ID exists in the caller's session.
- F7: `EINVAL` cases: `pgid < 0`; the target pid names a thread that is not a thread-group leader.
- F8: effects of a successful change: the new group is inherited by later `fork` children, kept across `execve`, and selects targets for `kill(-pgid)`, `wait4(0)`, `wait4(-pgid)`, `waitid(P_PGID, ...)` and terminal job control.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification of both https://github.com/rrnewton/hermit/issues/435 and its duplicate https://github.com/rrnewton/hermit/issues/449. The rules are a pure function of the process tree, the session membership, whether a child has already called `execve`, and the pid values. Given the same process tree and the same order of `fork`, `execve` and `setpgid` calls, the result repeats. It varies when that order varies (the shell/child race, or the `EACCES` outcome when the child execs first) and when pid values differ from run to run.

### Nondeterminism sources

- Parent/child scheduling order: which of the two `setpgid` calls in a job-control launch runs first, and whether the child has already reached `execve` (F3, F6)
- PID allocation: the `pid` and `pgid` arguments, and the group IDs reported later, are pid values
- Session membership and controlling-terminal state inherited from whatever launched the program
- The duplicate issue's framing: the arguments and results live in Hermit's virtual pid namespace and must be consistent with it. At this SHA that holds because the guest runs in a PID namespace and Detcore uses the guest-visible numbers directly (see Hermit design); no separate translation exists.

## Hermit design

Target policy: `model`. The kernel stays authoritative for validation and for the live group membership that signal delivery and terminals use; Detcore keeps a deterministic mirror for the decisions it owns. The code at hermit f4055eb9cae4 already does this:

- `handle_setpgid` (`detcore/src/syscalls/misc.rs:584`) injects the call unchanged (`detcore/src/syscalls/misc.rs:589`), so every validation rule above (F3, F5, F6, F7) is the kernel's. A failure returns through `?` before the mirror is touched.
- On success it resolves `pid == 0` to the caller's process and `pgid == 0` to the target (`detcore/src/syscalls/misc.rs:591-600`), matching F1 and F2, and records the group with `set_process_group` (`detcore/src/syscalls/misc.rs:601`, `detcore/src/tool_global.rs:4122`, `detcore/src/scheduler.rs:1073`).
- The mirror is inherited by process children at clone time (`detcore/src/scheduler.rs:1028`), which matches F8's `fork` inheritance.
- Consumers of the mirror: `wait4(0)` reads the caller's group from it (`detcore/src/syscalls/threads.rs:1613`), and a terminal-events-only `waitid(P_PGID, 0)` does too (`detcore/src/syscalls/threads.rs:1864-1867`). Explicit negative pids and explicit `P_PGID` ids are taken from the arguments (`detcore/src/syscalls/threads.rs:1616`, `detcore/src/syscalls/threads.rs:1869`).

Why `model` and not the alternatives:

- `passthrough` would leave the wait selectors with no deterministic source of group membership; the handler's doc comment (`detcore/src/syscalls/misc.rs:581-583`) states that the mirror exists so group-selecting waits do not consult host `/proc` state.
- `emulate` would have to reimplement the session, exec and leader checks and still could not change the kernel's group, which `kill(-pgid)` and terminal job control need.
- `refuse` would break every job-control shell.

Gaps between the code and a complete `model`:

- **Pid identity is assumed, not translated.** The handler builds `DetPid::from_raw(call.pid())` and `DetPid::from_raw(call.pgid())` (`detcore/src/syscalls/misc.rs:594`, `detcore/src/syscalls/misc.rs:599`). Detcore's thread IDs are the raw tids Reverie reports (`detcore/src/lib.rs:1661`, which carries `TODO(T78538674): virtualize pid/tid`), so this is correct only while the guest and the tracer see the same pid numbers inside Hermit's PID namespace. The ptrace selector test (`hermit-cli/tests/terminal_child_selectors.rs:68`) relies on exactly this when it calls `setpgid(child, child)` and then `wait4(-child)` (`tests/c/terminal_child_selectors.c:134-138`).
- **A silent mirror miss.** `set_process_group` returns `false` when the target is not in the scheduler's process table (`detcore/src/scheduler.rs:1074-1076`), and the handler discards that result (`detcore/src/syscalls/misc.rs:601`). The kernel change still happened, so a later `wait4(0)` could select by a stale group. No test exercises this.
- **One wait path still reads the host.** A `waitid(P_PGID, 0)` that also asks for stop or continue events snapshots the group from host `/proc` (`detcore/src/syscalls/threads.rs:1818-1822`, `detcore/src/syscalls/threads.rs:710-720`), not from the mirror.
- **The read side passes through.** `getpgid`, `getpgrp` and `getsid` have no handler. The comment in `tests/c/session_identity.c:12-17` records this (its dispatch line numbers are stale at this SHA; the arms are `detcore/src/lib.rs:2442-2443`). The values they return are stable because of the PID namespace, not because of the mirror.
- **Record/replay.** The handler uses `guest.inject`, not `record_or_replay`, so replay re-executes the call against the kernel. That is needed, because the group is live kernel state that later signals and waits depend on.

The shell/child race (F3) is resolved by Detcore's sequential schedule: both calls run in schedule order, so a given schedule always gives the same winner and the same `EACCES` outcome.

## Recommended tests

- T1 (covers F1, F2): in the root guest process, call `setpgid(0, 0)` and check that `getpgid(0) == getpid()`; repeat from a second thread and check the same result for the whole process. Run under `hermit run --verify` on ptrace.
  Existing partial coverage: `tests/c/session_identity.c:66-71` (cell `c-programs/session-identity`, `tests/e2e/manifests/c-programs.yaml:7993`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:8023-8025`) calls `setpgid(0, 0)` from the leader only. `sidecar_preserves_session_and_process_group_identity` (`hermit-cli/tests/run_evidence.rs:526`) asserts the same `setpgid rc=0` line.
- T2 (covers F3, F4, F8): fork child A and call `setpgid(A, A)`; fork child B and call `setpgid(B, A)` so B joins A's group; check `kill(-A, SIGTERM)` reaches both, then reap both with `wait4(-A)` and with `waitid(P_PGID, A)`. Also fork a grandchild from A after the change and check, from A, that `wait4(0)` reaps it.
  Existing partial coverage: `ptrace_terminal_child_selectors_follow_linux_contract` (`hermit-cli/tests/terminal_child_selectors.rs:68`) runs `tests/c/terminal_child_selectors.c`, whose `setpgid(child, child)` is followed by `wait4(-pgid)` (`tests/c/terminal_child_selectors.c:134-138`) and `waitid(P_PGID, pgid)` (`tests/c/terminal_child_selectors.c:151-158`). It does not join an existing group or signal a group. The same file also runs a KVM test (`hermit-cli/tests/terminal_child_selectors.rs:84`), so it cannot be cited as a ptrace-only coverage path.
- T3 (covers F3): with a pipe handshake that forces the child to finish `execve` first, check that the parent's `setpgid(child, child)` returns `EACCES` and that the result is identical across `--verify` runs.
- T4 (covers F5, F6, F7): an error matrix, each case checked for its errno and for an unchanged group afterwards (a later `wait4(0)` still selects by the old group): a pid that is neither the caller nor its child (`ESRCH`); a child that has called `setsid` (`EPERM`); a session leader changing its own group (`EPERM`); `pgid` naming no existing group (`EPERM`); `pgid == -1` (`EINVAL`); the tid of a non-leader thread as `pid` (`EINVAL`).
- T5 (covers F1, F4): a job-control launch in which both the parent and the child call `setpgid` on the child, run under `--verify` and under record and replay on ptrace. Every run must agree on the final group and on each call's return value.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/435: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/449 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (the kernel validates and applies the change; Detcore mirrors the group for its wait selectors); linux_determinism stays conditional; merged duplicate https://github.com/rrnewton/hermit/issues/449 (its "pids must be virtualized" claim reconciled: the guest-visible pid is used directly as the Detcore pid); 8 flag behaviours and 5 tests listed; no coverage token, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setpgid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setpgid&body=Discussion+for+the+%60setpgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetpgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setpgid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setpgid" (https://github.com/rrnewton/hermit/issues/435), legacy variant A1.

Duplicate https://github.com/rrnewton/hermit/issues/449 was filed the same day with the same title (legacy variant A1) and was merged into this body on 2026-10-03; its comment history (0 comments at migration) stays on that issue, and this issue's comment history stays here. What came from each:

- Both: classification CONDITIONAL, recommended action NEEDS-HANDLER, and the host observation `setpgid(0, 0) = 0`.
- From https://github.com/rrnewton/hermit/issues/435: the nondeterminism sources (parent/child timing, exec state, session membership, PID allocation, controlling-terminal state) and the point that group topology drives waits, terminal job control and group-directed signals.
- From https://github.com/rrnewton/hermit/issues/449: the Linux notes "constrained to the same session; children before exec", and the claim that the pid/pgid arguments live in Hermit's virtual pid namespace and must be mapped.
- Reconciled: https://github.com/rrnewton/hermit/issues/449 says the arguments "must be virtualized". At hermit f4055eb9cae4 no translation exists; the handler uses the guest-visible numbers as Detcore pids directly (`detcore/src/syscalls/misc.rs:594`, `detcore/src/syscalls/misc.rs:599`), which is correct only because the guest runs in Hermit's PID namespace. The Hermit design section records this as an assumption, not as virtualization. The legacy NEEDS-HANDLER recommendation is superseded: a handler exists.

### Open questions

- Should `handle_setpgid` act when `set_process_group` reports a miss (`detcore/src/syscalls/misc.rs:601`), for example by failing closed, instead of leaving the mirror stale?
- Should `getpgid`, `getpgrp` and `getsid` read the mirror instead of passing through? `tests/c/session_identity.c:45-49` records that this design question is open and that the cell's pinned values would change if a model were built.

### 2026-07-24 host strace evidence

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setpgid`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setpgid` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setpgid(0, 0)                   = 0
```

The duplicate https://github.com/rrnewton/hermit/issues/449 recorded the same line from `strace -f -e trace=setpgid` on the same kind of harness.

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
