---
schema: syscall-pages/v1
syscall: getpgid
nr_x86_64: 121
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/374
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpgid&body=Discussion+for+the+%60getpgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpgid%60%2C+or+in+what+the+page+says%3F%0A
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

`getpgid(pid)` returns the process group ID of the process named by `pid` (0 means the caller), as seen in the caller's PID namespace. Detcore answers from its own process model, so the guest's root process reports its own pid as its group instead of the 0 the host kernel produced when the group leader sat outside the container's PID namespace. An unknown or negative pid gets `ESRCH`; in a nested PID namespace the kernel answers.

Manual reference: [getpgid(2)](https://man7.org/linux/man-pages/man2/getpgid.2.html).

## Linux semantics

The kernel (`do_getpgid` in `kernel/sys.c`) takes one argument, a pid. If it is 0, the target is the caller; otherwise the kernel looks the pid up as a task id in the caller's PID namespace, so a thread id of another thread resolves to that thread's process. An id that is not found, including any negative value, fails with `ESRCH`. For a target other than the caller, the LSM hook `security_task_getpgid` runs and may refuse. The result is the pid of the target's process-group leader translated into the caller's namespace; if that leader is not visible there, the translation gives 0. There is no permission rule tying the target to the caller's session: any visible process can be queried.

A child inherits its parent's group across `fork` and `clone` and keeps it across `execve`; all threads of a process share it. The group changes only through `setpgid` and `setsid`.

### Flags and options

- F1: `pid == 0`: return the caller's process group (the same value as `getpgrp()`).
- F2: `pid` names another process, or any thread, visible in the caller's PID namespace: return that process's group.
- F3: `pid` not found in the caller's namespace (an exited and reaped process, an unused id, or a negative value such as -1): `ESRCH`.
- F4: the target's group leader is not visible in the caller's namespace: the call succeeds and returns 0.
- F5: an LSM denies the query of another process through `security_task_getpgid`: the hook's error, typically `EPERM` or `EACCES`.

## Determinism analysis

The value is a pid. Natively a process inherits its group from the shell or service that started it, so the value changes each run: the measured native run of `tests/c/session_identity.c` printed `sid=81608 pgid=82040` (`tests/c/session_identity.c:32`). Under Hermit the guest starts in a fresh PID namespace whose root has pid 3 (`ROOT_DETPID` at `detcore/src/consts.rs:30`), and the group inherited from the launcher has its leader outside that namespace, so the kernel reports 0 (F4). The fixture records three identical ptrace runs printing `before sid=0 pgid=0` (`tests/c/session_identity.c:29`) and `pgid=3` after `setpgid(0, 0)` (`tests/c/session_identity.c:30`), and says this stability comes from the PID and user namespace, not from a Detcore handler (`tests/c/session_identity.c:36`). Targets named by pid (F2, F3) are guest pids, which repeat because the namespace allocates them in the order Hermit serializes process creation. Whether a pid has been reaped yet (F3 against F2) depends on the order of `exit`, `wait` and the query, which Hermit also serializes. An LSM denial (F5) is host policy and would repeat on one host. So the call is `conditional` (previously `nondeterministic`): it repeats given the fresh PID namespace and serialized process creation; the legacy "host-assigned PID/PGID values" are what a guest would see without that namespace.

### Nondeterminism sources

- The launcher's process group, which reaches the guest only as "outside the namespace" (0); without a fresh PID namespace it would be a host pid that changes each run.
- Ordering of `fork`, `exit`, `wait`, `setpgid` and `setsid` against the query; Hermit's serialization fixes it.
- Host LSM policy (F5), fixed for a given host.

### Cross-host variation

- Host LSM policy: whether `security_task_getpgid` denies a query of another process (F5). The other values come from Hermit's fresh PID namespace and do not vary.

## Hermit design

Current behaviour (hermit 239ca43d3092): target policy `model`, resolving the open question below in favour of Detcore's process model. The typed arms at `detcore/src/lib.rs:7906` route to `handle_group_or_session_getter` (`detcore/src/syscalls/misc.rs:1058`), which resolves the target task (0 is the caller; a negative pid is `ESRCH`) and reads its group or session from shared Detcore state, returning `ESRCH` for a task the model does not know. A caller in a nested PID namespace is answered by the kernel instead. The forwarding analysis below is kept as the record of the earlier behaviour and of why its 0 answers for the guest root process were wrong for guests even though they repeated.

Target policy: `investigate` (unchanged). Today `getpgid` is classified `PassThrough` at `detcore/src/syscall_classification.rs:701`, in the group marked `TODO-HUMAN-REVIEW(#663)` at `detcore/src/syscall_classification.rs:700` with `getpgrp`, `getppid` and `getsid`, inside the pass-through block whose comment at `detcore/src/syscall_classification.rs:691` calls them "conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it (`passthrough` at `detcore/src/lib.rs:367`), and the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins it as `PassThrough` at `detcore/src/syscall_classification.rs:2067`.

Forwarding is deterministic, so this is not a determinism defect. It is not adopted as the target, because three views of the same group disagree:

- The kernel, through this syscall: 0 for the inherited group, then the guest's own pid after `setpgid` or `setsid`.
- Detcore's model: the scheduler starts the root in a group and session equal to its own id (`process_group: child_dettid` at `detcore/src/scheduler.rs:995`, `session: child_dettid` at `detcore/src/scheduler.rs:996`); `handle_setpgid` (`detcore/src/syscalls/misc.rs:584`) mirrors each change with `set_process_group` (`detcore/src/syscalls/misc.rs:601`), and `waitpid(0, ...)` selects children by that model group (`detcore/src/syscalls/threads.rs:1613`). So before any `setpgid`, Detcore treats the root's group as 3 while `getpgid(0)` returns 0.
- Hermit's procfs rewriting: `sanitize_status` (`detcore/src/procfs.rs:1354`) replaces the `NSpgid:` and `NSsid:` lines with 0 unconditionally (`detcore/src/procfs.rs:1418`), and `sanitize_stat` writes 0 into the pgrp and session fields of `/proc/<pid>/stat` for virtualized identities (`detcore/src/procfs.rs:1323`). So after `setpgid(0, 0)` the syscall says 3 and `/proc/self/status` says 0.

0 is also not a usable group id: passed back to `setpgid` it means "the caller's pid", so `setpgid(0, getpgid(0))`, a no-op natively, creates a new group in the guest's root. The fixture comment at `tests/e2e/determinism-stress/pid_tid_identity.c:30` calls the 0 from `getpgid(0)` something that "looks like a virtualization gap rather than a virtualized value", and `tests/c/session_identity.c:45` records the open question whether Hermit should model process groups and sessions at all. The options are under Open questions; `getpgid` must follow whatever is decided for `getpgrp` and `getsid`, and for F2 it must also answer consistently for other guest processes, including a thread id that resolves to its process.

Existing evidence: `tests/c/session_identity.c` calls `getpgid(0)` at `tests/c/session_identity.c:63`, `tests/c/session_identity.c:71` and `tests/c/session_identity.c:85` (cell `c-programs/session-identity` at `tests/e2e/manifests/c-programs.yaml:7993`, verify on ptrace in CI); `tests/e2e/determinism-stress/pid_tid_identity.c:89` calls `getpgid(0)` (cell in `tests/e2e/manifests/determinism-stress-c.yaml:254`). Both exercise only F1 and F4; no test at hermit f4055eb9cae4 calls `getpgid` on another pid or on a missing one.

## Recommended tests

- T1 (covers F1, F4): under `hermit run --verify` on ptrace, print raw `syscall(SYS_getpgid, 0)` from the root, a `fork` child, a non-main thread and a child after `execve`; assert they agree with `SYS_getpgrp` and with the value the chosen policy names (today 0 for the inherited group).
- T2 (covers F2): after the root forks two children, one of which calls `setpgid(0, 0)` and one of which creates a thread, assert that the parent's `getpgid(child)` and `getpgid(thread_tid)` equal what each child reports for itself, and that a parent's `setpgid(child, child)` before the child's `execve` is seen by both sides.
- T3 (covers F3): assert `getpgid(-1)`, `getpgid` of a reaped child and `getpgid` of an unused pid (for example `INT_MAX`) each return -1 with `ESRCH`, identically across `--verify` runs.
- T4 (covers none): run the T1-T3 binary under `--verify` on the CI host and assert no `EPERM` or `EACCES` appears. It no longer claims F5: an LSM denial is host policy outside Hermit's control, which U1 records; this is the place to add a denial case if a host LSM policy is ever part of the CI image.
- T5 (covers F1, F2): assert `getpgid(pid)` agrees with the `NSpgid:` line of `/proc/<pid>/status` and field 5 of `/proc/<pid>/stat` before and after `setpgid`. This fails today after `setpgid` because procfs reports 0 (`detcore/src/procfs.rs:1418`), and should pass under whichever policy the owner picks.
- U1 (covers F5; host-config): an LSM denial from `security_task_getpgid` depends on the host's security policy, which the CI host does not set up. Hermit forwards the call unchanged (`detcore/src/syscall_classification.rs:701`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/374: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism nondeterministic -> conditional (the root and its unchanged descendants read pgid 0 from the PID namespace on every run, per tests/c/session_identity.c:29); kept target_policy investigate and tracking_status todo with the open question shared with getpgrp: the kernel reports 0 for the inherited group, Detcore models it as 3 (detcore/src/scheduler.rs:995) and procfs always reports 0 (detcore/src/procfs.rs:1418), so which value should Hermit present; 5 flags; 5 tests; no coverage token recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host LSM policy decides F5); F5 LSM denial recorded as host-config U1 and T4 no longer claims it; no open question resolved


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · reclassified: passthrough -> determinized (process groups and sessions answered from Detcore's model); Summary and Hermit design updated, target_policy investigate -> model.

## Discussion

[Start a discussion about `getpgid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpgid&body=Discussion+for+the+%60getpgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpgid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getpgid" (https://github.com/rrnewton/hermit/issues/374), legacy variant A2.

### Open questions

- Which process group should the guest see? (a) Keep pass-through and accept 0 as the documented value of the root's inherited group; (b) have the launcher make the guest's root the leader of its own group and session, so the kernel reports 3 and agrees with Detcore's model at `detcore/src/scheduler.rs:995`; or (c) answer `getpgrp`, `getpgid` and `getsid` from Detcore's process-group model, as the write side already mirrors into it. Whichever is chosen, the procfs rewriting at `detcore/src/procfs.rs:1418` and `detcore/src/procfs.rs:1323` should report the same value. This is the same question as in the `getpgrp` body (https://github.com/rrnewton/hermit/issues/605) and `tests/c/session_identity.c:45`. Until the owner answers, target_policy stays `investigate` and tracking_status stays `todo`.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=getpgid -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(121, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
getpgid(-1)                     = -1 ESRCH (No such process)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
