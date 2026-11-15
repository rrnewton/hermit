---
schema: syscall-pages/v1
syscall: getppid
nr_x86_64: 110
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/606
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getppid&body=Discussion+for+the+%60getppid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetppid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getppid%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 26
---

## Summary

`getppid()` returns the process ID of the calling process's parent, as seen in the caller's PID namespace. Hermit forwards it to the host kernel. The value is a PID-namespace pid, and that namespace numbering is Hermit's process identity for every pid-returning call, so `getppid` repeats whenever `getpid` does.

## Linux semantics

The kernel returns the thread group ID of the caller's `real_parent`, translated into the caller's PID namespace. It takes no arguments and cannot fail. Every thread in a thread group gets the same answer, because the parent belongs to the group, not the thread. A ptrace tracer does not change it: since Linux 2.6 `getppid` reports the real parent, not the tracer. If the parent is outside the caller's PID namespace, as for a namespace's pid 1, the result is 0. When the parent exits, the kernel reparents the child to the nearest ancestor marked with `PR_SET_CHILD_SUBREAPER`, or else to the namespace's init, and later calls return that pid. A child created with `CLONE_PARENT` (or `CLONE_THREAD`) shares its creator's parent, so its `getppid` returns the creator's parent, not the creator.

Manual reference: [getppid(2)](https://man7.org/linux/man-pages/man2/getppid.2.html).

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

The return value is a pid, and pids are allocated by the kernel. On a host without a fresh PID namespace they depend on every other process on the machine, which is what the legacy text meant by a "host parent PID". Inside a fresh PID namespace, pids are allocated in creation order within that namespace, so the values repeat when the same processes are created and reaped in the same order. Under Hermit, the guest starts in a new PID namespace at a fixed pid (`ROOT_DETPID`, 3, at `detcore/src/consts.rs:30`, "A convention of how we set up our PID namespace leaves us with a starting pid of 3"), and guest threads run one at a time, so `fork`, `clone`, `exit` and the reparenting that follows an exit happen in a fixed order. The result therefore repeats run to run, which is why the call is `conditional` rather than `nondeterministic`: the condition is a fresh PID namespace plus serialized process creation and exit.

### Nondeterminism sources

- Host pid allocation, when there is no fresh PID namespace: the parent's pid then depends on the rest of the machine.
- Reparenting order: which pid an orphan reports depends on whether its parent's exit has completed, and on which ancestor is a subreaper. Hermit's serialization fixes the order; natively it is a race.
- Coupling to `getpid`: the value is only meaningful relative to the pids `getpid`, `fork`, `wait` and `/proc` report. If Hermit ever virtualizes pids instead of relying on the namespace (the `TODO(T78538674): virtualize pid` at `detcore/src/lib.rs:1069`), `getppid` must change with them.

## Hermit design

Target policy: `passthrough`, changed from `investigate`. The code already does this at hermit f4055eb9cae4. `getppid` is classified `PassThrough` at `detcore/src/syscall_classification.rs:703`, in the group marked `TODO-HUMAN-REVIEW(#663)` at `detcore/src/syscall_classification.rs:700` with `getpgid`, `getpgrp` and `getsid`, inside the pass-through block whose comment at `detcore/src/syscall_classification.rs:691` calls these calls "conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it, and the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins it as `PassThrough` at `detcore/src/syscall_classification.rs:2065`.

The legacy recommendation was **NEEDS-HANDLER**, on the grounds that the call "exposes a host parent PID that requires Detcore process-identity mapping". That is not what happens on ptrace: the guest sees PID-namespace pids, and Detcore's own identity is the same numbering. Detcore takes a process's `DetPid` straight from its namespace pid (`DetPid::from_raw(pid.into())` at `detcore/src/lib.rs:1069`), and when it synthesizes procfs text it fills the parent pid by injecting a real `Getppid` into the guest (`detcore/src/syscalls/files.rs:1497`, beside an injected `Getpid` at `detcore/src/syscalls/files.rs:1496`). A Detcore handler for `getppid` would have to return the value the kernel already returns, so the right policy is to keep forwarding it and keep it in step with `getpid`.

Existing evidence, all under `hermit run --verify` on ptrace:

- `tests/e2e/determinism-stress/fork_tree.c:37` calls raw `syscall(SYS_getppid)` in each process of a fork tree and requires a positive parent in every child, allowing 0 only where the comment at `tests/e2e/determinism-stress/fork_tree.c:40` expects it ("Linux reports PPID 0 for PID 1 in a PID namespace"). Cell `determinism-stress-c/fork-tree` (`tests/e2e/manifests/determinism-stress-c.yaml:4`), ptrace CI. It checks the sign only and does not print the value.
- `tests/e2e/determinism-stress/pid_tid_identity.c:88` prints the root's `getppid()`, and `tests/e2e/determinism-stress/pid_tid_identity.c:138` and `tests/e2e/determinism-stress/pid_tid_identity.c:139` print a forked child's value and whether it equals the parent's `getpid()`. The cell `determinism-stress-c/pid-tid-identity` (`tests/e2e/manifests/determinism-stress-c.yaml:254`, `ci: true`, ptrace only) compares stdout between runs.
- `tests/e2e/determinism-stress/guests/pid_tid.c:30` prints the root's ppid, cell `determinism-stress-c/pid-tid` (`tests/e2e/manifests/determinism-stress-c.yaml:196`).
- `tests/c/dbt_pid_virtualization.c:167` asserts that a `vfork` child's raw `getppid` is 3 (`tests/c/dbt_pid_virtualization.c:171`), but its cell (`tests/e2e/manifests/c-programs.yaml:984`) has ptrace CI off, so it is not ptrace evidence.
- `hermit-cli/tests/common/dispatch_stats.rs:35` uses 64 raw `getppid` calls as a dispatch-counting probe (described at `hermit-cli/tests/common/dispatch_stats.rs:10`), and `hermit-cli/tests/fixtures/nonleader_exec.c:199` checks that the result is positive. A handler would change what the counting harness measures.

None of these covers reparenting, `CLONE_PARENT` or the per-thread case, so no coverage token is recorded.

## Recommended tests

- T1 (covers none): under `hermit run --verify` on ptrace, print raw `syscall(SYS_getppid)` from the root, from a `fork` child, a `vfork` child and a grandchild, and from a non-main thread of each, and assert each child's value equals its parent's `getpid()` and every thread in a group prints the same value. Existing partial coverage: `tests/e2e/determinism-stress/pid_tid_identity.c:138` (cell `determinism-stress-c/pid-tid-identity`) and `tests/e2e/determinism-stress/fork_tree.c:37` (cell `determinism-stress-c/fork-tree`).
- T2 (covers none): reparenting: a child whose parent exits reports the namespace init's pid (or a guest subreaper's pid after `prctl(PR_SET_CHILD_SUBREAPER, 1)` in an ancestor), and the printed values match on both runs.
- T3 (covers none): a child created with `clone(CLONE_PARENT)` reports its creator's parent, not its creator.
- T4 (covers none): the parent pid in `/proc/self/stat` and the `PPid:` line of `/proc/self/status` equal raw `getppid` in the root and in a child, so the procfs path at `detcore/src/syscalls/files.rs:1497` and the syscall agree.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/606: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> passthrough and linux_determinism nondeterministic -> conditional: the guest sees PID-namespace pids, which are Detcore's own process identity (detcore/src/lib.rs:1069, ROOT_DETPID at detcore/src/consts.rs:30, injected Getppid at detcore/src/syscalls/files.rs:1497), and they repeat given a fresh namespace and serialized process creation; classified at detcore/src/syscall_classification.rs:703, forwarded at detcore/src/lib.rs:2843; no flags; 4 tests; no coverage token recorded
- 2026-10-03 · hermit f4055eb9cae4 · facts-bot · code_facts changed: tests_naming 3 -> 4.

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getppid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getppid&body=Discussion+for+the+%60getppid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetppid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getppid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getppid" (https://github.com/rrnewton/hermit/issues/606), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
