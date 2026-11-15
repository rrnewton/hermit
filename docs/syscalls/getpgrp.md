---
schema: syscall-pages/v1
syscall: getpgrp
nr_x86_64: 111
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/605
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpgrp&body=Discussion+for+the+%60getpgrp%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpgrp.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpgrp%60%2C+or+in+what+the+page+says%3F%0A
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

`getpgrp()` returns the process group ID of the calling process, as seen in the caller's PID namespace. Detcore answers from its own process model, so the guest's root process reports its own pid as its group instead of the 0 the host kernel produced when the group leader sat outside the container's PID namespace. In a nested PID namespace the kernel answers.

## Linux semantics

`getpgrp()` is equivalent to `getpgid(0)`. The kernel returns the pid of the caller's process-group leader, translated into the caller's PID namespace. It takes no arguments and cannot fail. If the group leader is not visible in the caller's namespace, the translation gives 0. A child inherits its parent's group across `fork` and `clone`, and keeps it across `execve`; all threads of a process share it. The group changes only through `setpgid` (on the caller, or on a child by its parent before the child execs) and `setsid`, which makes the caller leader of a new session and of a new group whose ID is its own pid. Glibc's `getpgrp()` issues `SYS_getpgrp`, not `getpgid` (checked 2026-10-03 on glibc 2.34, x86_64).

Manual reference: [getpgrp(2)](https://man7.org/linux/man-pages/man2/getpgrp.2.html).

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

The value is a pid. Natively, a process inherits its group from the shell or service that started it, so the value differs from run to run: the measured native run of `tests/c/session_identity.c` printed `sid=81608 pgid=82040` (`tests/c/session_identity.c:32`). Under Hermit the guest starts in a fresh PID namespace whose leader is the guest's root at pid 3 (`ROOT_DETPID` at `detcore/src/consts.rs:30`), and the group it inherits from the launcher has its leader outside the namespace, so the kernel reports 0. The comment in `tests/c/session_identity.c` records three identical ptrace runs printing `before sid=0 pgid=0` (`tests/c/session_identity.c:29`) and `pgid=3` after `setpgid(0, 0)` (`tests/c/session_identity.c:30`), and states that this stability comes from the PID and user namespace, not from a Detcore handler (`tests/c/session_identity.c:38`). Later group changes come from the guest's own `setpgid` and `setsid` calls, which Hermit serializes. So the call is `conditional`: it repeats given a fresh PID namespace and serialized process creation, and the legacy "host process-group identifier" is what a guest would see without that namespace.

### Nondeterminism sources

- The launcher's process group, which reaches the guest only as "outside the namespace" (0). Without a fresh PID namespace, it would be a host pid that changes each run.
- Ordering of `setpgid` and `setsid` against `fork` and `getpgrp` in other guest processes; Hermit's serialization fixes it.

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `model`, resolving the open question below in favour of Detcore's process model. The typed arms at `detcore/src/lib.rs:7906` route to `handle_group_or_session_getter` (`detcore/src/syscalls/misc.rs:1058`), which resolves the target task (0 is the caller; a negative pid is `ESRCH`) and reads its group or session from shared Detcore state, returning `ESRCH` for a task the model does not know. A caller in a nested PID namespace is answered by the kernel instead. The forwarding analysis below is kept as the record of the earlier behaviour and of why its 0 answers for the guest root process were wrong for guests even though they repeated.

Target policy: `investigate` (unchanged). Today `getpgrp` is classified `PassThrough` at `detcore/src/syscall_classification.rs:702`, in the group marked `TODO-HUMAN-REVIEW(#663)` at `detcore/src/syscall_classification.rs:700` with `getpgid`, `getppid` and `getsid`, inside the pass-through block whose comment at `detcore/src/syscall_classification.rs:691` calls them "conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it, and the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins it as `PassThrough` at `detcore/src/syscall_classification.rs:2068`.

Forwarding is deterministic, so this is not a determinism defect. It is not adopted as the target, because the value the guest sees disagrees with Detcore's own model and with what a program could see on Linux outside a container:

- Detcore models process groups for the write side. `handle_setpgid` (`detcore/src/syscalls/misc.rs:584`) injects `setpgid` and then mirrors the change with `set_process_group` (`detcore/src/syscalls/misc.rs:601`) "so group-selecting waits do not consult host `/proc` state", and `handle_setsid` (`detcore/src/syscalls/misc.rs:564`) records a new session; both are dispatched at `detcore/src/lib.rs:2442` and `detcore/src/lib.rs:2443`. The scheduler starts the root process in a group and session equal to its own id (`process_group: child_dettid` at `detcore/src/scheduler.rs:995`, `session: child_dettid` at `detcore/src/scheduler.rs:996`), and `waitpid(0, ...)` selects children by that model group (`detcore/src/syscalls/threads.rs:1613`). So before any `setpgid`, Detcore treats the root's group as 3 while `getpgrp` returns 0.
- 0 is not a valid process group. Passed back to the kernel it means "the caller's own group" or "the caller's pid": `setpgid(0, getpgrp())`, a no-op natively, makes the guest's root the leader of a new group under Hermit. The fixture comment at `tests/e2e/determinism-stress/pid_tid_identity.c:30` calls the 0 from `getpgid(0)` something that "looks like a virtualization gap rather than a virtualized value".
- The fixture that pins the current values says plainly that it "does NOT presuppose the design" and that there is an open question whether Hermit should model process groups and sessions at all (`tests/c/session_identity.c:45`).

The options are listed under Open questions. Whichever is chosen, `getpgrp` must agree with `getpgid(0)`, with the `NSpgid:` line of `/proc/<pid>/status` that Hermit's procfs rewriting handles (`detcore/src/procfs.rs:1362`), and with the group Detcore's wait selection uses.

Existing evidence: no test at hermit f4055eb9cae4 calls `getpgrp` or `SYS_getpgrp`. The nearest are `getpgid(0)` calls: `tests/c/session_identity.c:63`, `tests/c/session_identity.c:71` and `tests/c/session_identity.c:85` (cell `c-programs/session-identity` at `tests/e2e/manifests/c-programs.yaml:7993`, verify with ptrace CI), and `tests/e2e/determinism-stress/pid_tid_identity.c:89` (cell `determinism-stress-c/pid-tid-identity`, `tests/e2e/manifests/determinism-stress-c.yaml:254`).

## Recommended tests

- T1 (covers none): under `hermit run --verify` on ptrace, print raw `syscall(SYS_getpgrp)` and `getpgid(0)` from the root, a `fork` child, a non-main thread and a child after `execve`, and assert they agree with each other and with the value the chosen policy names (today 0; 3 if the root becomes a group leader).
- T2 (covers none): after `setpgid(0, 0)` in the root and in a child, and after `setsid()` in a child that is not a group leader, assert raw `SYS_getpgrp` returns the caller's pid, and that a parent's `setpgid(child, child)` before the child's `execve` is seen by the child.
- T3 (covers none): assert `setpgid(0, getpgrp())` leaves `getpgrp()` unchanged, as it does natively. This fails today in the root process if the 0 analysis above holds, and should pass under whichever policy the owner picks.
- T4 (covers none): assert the `NSpgid:` line of `/proc/self/status` and field 5 of `/proc/self/stat` equal raw `SYS_getpgrp`, and that `waitpid(0, ...)` and `waitpid(-getpgrp(), ...)` reap the same child.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/605: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism nondeterministic -> conditional (the root and its unchanged children read pgid 0 from the PID namespace on every run, per tests/c/session_identity.c:29); kept target_policy investigate and tracking_status todo with an open question for the owner: the guest sees pgid 0 while Detcore's model puts the root in group 3 (detcore/src/scheduler.rs:995), so should Hermit present 3 (make the root a group and session leader, or model getpgrp from Detcore's process groups) or accept 0; no flags; 4 tests; no coverage token recorded

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: passthrough -> determinized (process groups and sessions answered from Detcore's model); Summary and Hermit design updated, target_policy investigate -> model.

## Discussion

[Start a discussion about `getpgrp`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpgrp&body=Discussion+for+the+%60getpgrp%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpgrp.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpgrp%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getpgrp" (https://github.com/rrnewton/hermit/issues/605), legacy variant B.

### Open questions

- Which process group should the guest see? (a) Keep pass-through and accept 0 as the documented value for the root's inherited group; (b) have the launcher make the guest's root the leader of its own group and session, so the kernel reports 3 and agrees with Detcore's model at `detcore/src/scheduler.rs:995`; or (c) answer `getpgrp`, `getpgid` and `getsid` from Detcore's process-group model, as the write side already mirrors into it. The same decision covers `getpgid` and `getsid` (the `TODO-HUMAN-REVIEW(#663)` group at `detcore/src/syscall_classification.rs:700`) and matches the open question recorded at `tests/c/session_identity.c:45`. Until the owner answers, target_policy stays `investigate` and tracking_status stays `todo`.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
