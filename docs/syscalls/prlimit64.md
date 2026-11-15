---
schema: syscall-pages/v1
syscall: prlimit64
nr_x86_64: 302
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/326
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+prlimit64&body=Discussion+for+the+%60prlimit64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprlimit64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60prlimit64%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 5
---

## Summary

Gets and/or sets one resource limit (`RLIMIT_*`) of a process, atomically, with 64-bit limit values. Hermit answers it from a per-process virtual limit table with fixed initial values. It never reads the host's limits, accepts real changes only for `RLIMIT_STACK`, `RLIMIT_NOFILE` and `RLIMIT_CORE` without raising the hard limit, and allows only the calling process as target.

## Linux semantics

`prlimit64(pid, resource, new_rlim, old_rlim)` supersedes `getrlimit`/`setrlimit` and can target another process. The order inside the kernel is:

1. Copy in `*new_rlim` (`EFAULT` on a bad pointer).
2. Look up the target (`ESRCH` if no such pid).
3. Check permission against the target's credentials (`EPERM`).
4. Validate the resource number (`EINVAL`) and the new value (`EINVAL` when the soft limit exceeds the hard limit; `EPERM` when the hard limit is raised without `CAP_SYS_RESOURCE`, or when the `RLIMIT_NOFILE` hard limit exceeds `fs.nr_open`), apply it, and capture the old value under the same lock.
5. Copy out `*old_rlim`. This happens after the change took effect, so an `EFAULT` here leaves the new limit in place.

Limits live in the process (`signal_struct`): every thread shares them, `fork` copies them, and `execve` keeps them. The kernel enforces them in many places (open, mmap, brk, stack growth, write, fork, CPU accounting, core dumps, signal queues).

### Flags and options

- F1: query only: `new_rlim` NULL, `old_rlim` non-NULL; the current soft and hard limit are written to `*old_rlim`.
- F2: set only: `new_rlim` non-NULL, `old_rlim` NULL.
- F3: set and get in one call: `*old_rlim` receives the value from before the change.
- F4: both pointers NULL: the pid and resource are still checked; the call has no other effect and returns 0.
- F5: `pid` 0 or the caller's own pid: acts on the calling process.
- F6: `pid` of another process: permitted when the caller's real, effective and saved uid and gid all match the target's, or with `CAP_SYS_RESOURCE`; otherwise `EPERM`. A pid that does not exist gives `ESRCH`.
- F7: `pid` naming a thread of the calling process (a TID other than the thread-group id): the kernel finds the thread and acts on its process's shared limits.
- F8: `resource`: the 16 valid resources `RLIMIT_CPU` (0) through `RLIMIT_RTTIME` (15). Any value at or above `RLIM_NLIMITS` (16) gives `EINVAL`.
- F9: soft limit above the hard limit: `EINVAL`.
- F10: raising the hard limit without `CAP_SYS_RESOURCE`: `EPERM`. Raising the `RLIMIT_NOFILE` hard limit above `fs.nr_open`: `EPERM` even with the capability.
- F11: lowering the hard limit, or moving the soft limit anywhere at or below the hard limit: allowed without privilege, for every resource. A lowered hard limit cannot be raised again without privilege.
- F12: `RLIM64_INFINITY` (all ones) means no limit, in both input and output.
- F13: an exact no-op (new value equal to the current one) succeeds for every valid resource.
- F14: enforcement side effects of a lowered limit:
  - `RLIMIT_NOFILE`: `open`, `dup`, `socket` and similar fail with `EMFILE` above the soft limit;
  - `RLIMIT_CPU`: `SIGXCPU` at the soft limit, `SIGKILL` at the hard limit;
  - `RLIMIT_FSIZE`: `SIGXFSZ` and `EFBIG` on writes past the limit;
  - `RLIMIT_STACK`, `RLIMIT_AS` and `RLIMIT_DATA`: stack growth, `mmap` and `brk` fail; `RLIMIT_STACK` also shapes the `execve` memory layout;
  - `RLIMIT_NPROC`: `fork` and `clone` fail with `EAGAIN`;
  - `RLIMIT_CORE`: core dump size; 0 disables core dumps;
  - `RLIMIT_MEMLOCK`, `RLIMIT_SIGPENDING`, `RLIMIT_MSGQUEUE`, `RLIMIT_NICE`, `RLIMIT_RTPRIO`, `RLIMIT_RTTIME` and `RLIMIT_LOCKS`: their respective subsystems.
- F15: inheritance: threads share one set of limits, `fork` gives the child a copy, and `execve` keeps them.
- F16: `EFAULT` ordering: a bad `new_rlim` fails before any lookup or change; a bad `old_rlim` fails after the change was applied.
- F17: initial values: inherited from the parent, so they reflect the host's `ulimit` and PAM configuration (for example a `RLIMIT_NOFILE` hard limit of 524288 on one host and 1048576 on another).
- F18: host-enforced limit below the reported one: infrastructure error (owner decision 5 interim). When the host's real limit is below the pair `prlimit64` reports, a failure it causes in a later call (for example `EMFILE` from `openat` far below the reported 1048576) is reported as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=<name> errno=<NAME>`, not handed to the guest.

## Determinism analysis

On Linux, `prlimit64` on the caller is a read and write of inherited process state. Its results repeat exactly when the starting limits repeat, so the call is `conditional`: the starting values (F17) come from the host's configuration and differ across machines and users, which is the legacy host-state leak this issue was filed for. The legacy body classed the call `nondeterministic`; nothing in the call depends on timing for a self-target, so `conditional` is the better fit. Targeting another process (F6) also depends on which pids exist and on their credentials.

### Nondeterminism sources

- Starting limits inherited from the host's `ulimit`, systemd and PAM configuration (F17), leaked verbatim to a query.
- The pid space and the credentials of other processes, for F6 and F7.
- Enforcement (F14) makes later syscalls fail at host-dependent points when the limits are host-dependent.

## Hermit design

Target policy: `model`. Detcore keeps a virtual per-process limit table and answers every query from it. Owner decision 5 (2026-10-04: limits become real and deterministic) adds that the reported pair is the limit actually in force: a later call fails at exactly the soft limit the table holds (F14). Hermit gets there per resource, in one of two ways: Detcore enforces the limit itself (for `RLIMIT_NOFILE`, by counting the guest's descriptor table), or Hermit configures the guest's host rlimit to the reported value at launch and on each accepted change, so the two cannot disagree. This body does not choose between them. Once a resource is enforced or configured, the unprivileged changes Linux allows (F11), lowering included, can be accepted; until then `EPERM` for a real change stays the honest answer.

Interim, until a resource reaches the target (F18): a failure caused by a host-enforced limit that the reported limit would not predict is a determinism failure, not a guest failure. Detcore reports it as `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=<name> errno=<NAME>` through the infrastructure-error convention instead of passing it through. Today it is passed through silently; for example the error arm of `handle_openat` hands the host's `EMFILE` to the guest unchanged (`detcore/src/syscalls/files.rs:1130`-`1132`).

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): this replaces the interim refusal above. Queries return fixed deterministic values with soft equal to hard for every resource: `RLIMIT_STACK` 8 MiB, `RLIMIT_NOFILE` 1048576, `RLIMIT_MEMLOCK` 0 (`mlock`, `mlock2` and `mlockall` are unsupported and return `EPERM`), and `RLIM_INFINITY` for the rest. glibc's `getrlimit(3)`, bash `ulimit`, the JVM, Go and Python runtimes, and daemons that close every descriptor up to `RLIMIT_NOFILE` read them at startup, so they stay fixed rather than refused. A lowering through `new_rlim` is accepted for all 16 resources, forwarded to the guest's host limit and recorded in the table. A raise of the hard limit is `EPERM` (F10). A raise of a lowered soft limit back up to the hard limit is accepted and forwarded, as Linux allows without privilege (F11; the reasoning is in the `setrlimit` body). Any host-enforced surprise that remains is the `cause=host-rlimit` infrastructure error (F18).

Code at hermit f4055eb9cae4:

- Dispatch (`detcore/src/lib.rs:2775`) goes to `handle_prlimit64` (`detcore/src/syscalls/sysinfo.rs:222`), tagged `TODO-HUMAN-REVIEW(#534)` (`detcore/src/syscalls/sysinfo.rs:221`). Its doc comment (`detcore/src/syscalls/sysinfo.rs:212`-`219`) states the contract: deterministic process-local queries, virtual changes only, and no request to the host kernel to enforce them.
- Initial values (`detcore/src/tool_local.rs:278`-`295`): every resource is `RLIM64_INFINITY` for soft and hard, except `RLIMIT_STACK` (8 MiB soft, infinite hard) and `RLIMIT_NOFILE` (1048576 soft and hard). The table has `RLIMIT_RTTIME + 1` = 16 entries (`detcore/src/tool_local.rs:284`), and `get` returns `None` for any larger resource (`detcore/src/tool_local.rs:299`). This removes the F17 host leak. `getrlimit` and `setrlimit` read and write the same table (`detcore/src/syscalls/sysinfo.rs:163`, `detcore/src/syscalls/sysinfo.rs:188`).
- Order inside the handler:
  1. An invalid resource returns `EINVAL` (`detcore/src/syscalls/sysinfo.rs:229`-`236`), before `new_rlim` is read.
  2. `*new_rlim` is read (`detcore/src/syscalls/sysinfo.rs:238`-`246`); a bad pointer gives `EFAULT`.
  3. The pid must be 0 or the guest's deterministic pid, falling back to the physical pid before one is assigned (`detcore/src/syscalls/sysinfo.rs:123`-`129`, used at `detcore/src/syscalls/sysinfo.rs:250`). Anything else gets `EPERM` (`detcore/src/syscalls/sysinfo.rs:251`).
  4. `validate_resource_limit_mutation` (`detcore/src/syscalls/sysinfo.rs:131`-`157`): soft above hard gives `EINVAL` (`detcore/src/syscalls/sysinfo.rs:136`); an exact no-op succeeds for every resource (`detcore/src/syscalls/sysinfo.rs:142`); any real change to a resource other than `RLIMIT_STACK`, `RLIMIT_NOFILE` and `RLIMIT_CORE` gives `EPERM` (`detcore/src/syscalls/sysinfo.rs:147`-`151`); raising the hard limit gives `EPERM` (`detcore/src/syscalls/sysinfo.rs:153`). The change is applied at `detcore/src/syscalls/sysinfo.rs:265`.
  5. `*old_rlim` receives the previous value after the change (`detcore/src/syscalls/sysinfo.rs:272`-`278`), as on Linux (F3, F16).
- Inheritance (`detcore/src/lib.rs:1566`-`1578`): `CLONE_THREAD` children share the table through one `Arc`; other clones get a snapshot copy. This matches F15 for threads and `fork`. Nothing resets the table at `execve`, which also matches Linux.

Differences from Linux that remain (each is a deliberate restriction or an open question, not a host leak):

- F6: any other pid gets `EPERM`, including a same-credential child that Linux would allow, and a pid that does not exist (Linux: `ESRCH`).
- F7: by the code, a sibling thread's TID is neither 0 nor the deterministic pid, so it gets `EPERM`; Linux acts on the process. The deterministic pid is a process id (`detcore/src/tool_local.rs:1723`). Not measured (H1).
- F8 versus F16: an invalid resource is checked before `new_rlim` is read, so `prlimit64(0, 16, (void *)1, NULL)` returns `EINVAL` under Hermit and `EFAULT` on Linux.
- F11: lowering the soft or hard limit of a resource outside the three mutable ones (for example `RLIMIT_CPU` or `RLIMIT_AS`) gets `EPERM`, while Linux allows it unprivileged.
- F14: accepted changes are not enforced. A guest that lowers `RLIMIT_NOFILE` to 16 can still open more files, and a lowered `RLIMIT_CORE` changes nothing on the host. The host's own limits still apply, invisibly, underneath the virtual ones. This is the gap to the decision 5 target.
- F10, F18: the virtual `RLIMIT_NOFILE` hard limit (1048576) can exceed the host's real hard limit, so a guest that trusts the reported value can hit the host `EMFILE` earlier than it expects, unmarked (`detcore/src/syscalls/files.rs:1130`-`1132`).

Target (owner decision 6, 2026-10-04): as an emulated call, `handle_prlimit64` makes Linux's checks in the kernel's order. `prlimit64` in `kernel/sys.c` copies `*new_rlim` first (`EFAULT`, F16), then looks up the target (`ESRCH`) and checks permission (`check_prlimit_permission`, `EPERM`, F6), then calls `do_prlimit`, which checks the resource (`EINVAL`, F8), soft above hard (`EINVAL`, F9), an `RLIMIT_NOFILE` hard limit above `nr_open` and a hard-limit raise without `CAP_SYS_RESOURCE` (`EPERM`, F10); `*old_rlim` is written last (`EFAULT` after the change, F16). The handler should move the resource check (`detcore/src/syscalls/sysinfo.rs:229`-`236`) after the read of `*new_rlim` (`detcore/src/syscalls/sysinfo.rs:238`-`246`) and after its pid check (`detcore/src/syscalls/sysinfo.rs:250`-`251`), so `prlimit64(0, 16, (void *)1, NULL)` gives `EFAULT`. Which pids the pid step accepts is the separate targets question under Open questions; the order holds either way. T5 pins it.

Why `model` and not the alternatives: `passthrough` is the host leak the legacy issue describes; `refuse` would break every program that queries its limits at startup (shells, `ulimit`, JVMs, sanitizers, Python). The `setrlimit` body now also says `model` for the same table. The host limits Hermit leaves in place are the same on every run on one host, but they differ between hosts and disagree with the reported values, which owner decision 5 rules out; hence the enforcement target above.

## Recommended tests

- T1 (covers F1, F8, F12, F17): query every resource 0 through 15 with `prlimit64(0, r, NULL, &old)` and compare against `getrlimit` and raw `SYS_getrlimit`; expect the narrowed table above (soft equal to hard; `RLIMIT_STACK` 8 MiB, `RLIMIT_NOFILE` 1048576, `RLIMIT_MEMLOCK` 0, the rest `RLIM_INFINITY`) on every host; then resource 16 gives `EINVAL`. By the code this fails at this SHA on `RLIMIT_STACK` and `RLIMIT_MEMLOCK` (`detcore/src/tool_local.rs:278`-`292`).
  Existing candidate: `tests/c/resource_determinism.c:100`-`125` and `tests/c/resource_determinism.c:244`-`245`, cell `c-programs/resource-determinism` (`tests/e2e/manifests/c-programs.yaml:7549`, verify `ci` `ptrace: true`). `tests/c/rlimit_identity.c` (cell `c-programs/rlimit-identity`, `tests/e2e/manifests/c-programs.yaml:14992`, verify `ci` `ptrace: true`) prints the `RLIMIT_NOFILE` hard limit.
- T2 (covers F2, F3, F5, F11): lower the `RLIMIT_NOFILE` soft limit with the caller's own pid and with pid 0, check that `*old_rlim` holds the earlier value and that a following query sees the change, then restore it.
  Existing candidates: `tests/c/dbt_prlimit_self.c:22`-`63` (cell `c-programs/dbt-prlimit-self`, `tests/e2e/manifests/c-programs.yaml:1045`, verify `ci: true`) and `tests/c/resource_determinism.c:177`-`193`. Extend with a lowered hard limit that cannot be raised again, and with a lowered `RLIMIT_CPU` soft limit, which must now succeed and read back (the code at this SHA returns `EPERM`, `detcore/src/syscalls/sysinfo.rs:147`-`151`).
- T3 (covers F13): an exact no-op on `RLIMIT_CORE` and on `RLIMIT_CPU` (a resource Hermit refuses to change today) both succeed.
  Existing candidate: `tests/c/resource_determinism.c:196`-`215` covers `RLIMIT_CORE` only.
- T4 (covers F9, F10): soft above hard gives `EINVAL`; raising the hard limit gives `EPERM`.
  Existing candidate: `tests/c/resource_determinism.c:247`-`255`.
- T5 (covers F6, F8, F16): another process's pid gives `EPERM`; a bad `new_rlim` with another pid gives `EFAULT`; an invalid resource with another pid gives `EINVAL`; an invalid resource with a bad `new_rlim` and pid 0 gives `EFAULT`, as on Linux (owner decision 6; the code at this SHA returns `EINVAL`); a bad `old_rlim` on a real change returns `EFAULT` and a following query shows the change applied.
  Existing candidates: `tests/c/resource_determinism.c:228`-`243` and `tests/c/dbt_prlimit_self.c:47`-`56`. The bad-`old_rlim` and order cases are new.
- T6 (covers F7): from a second thread, call `prlimit64` with the first thread's TID and with the second thread's own TID. Record Hermit's result against Linux's (success on the shared process limits).
- T7 (covers F4): `prlimit64(0, RLIMIT_NOFILE, NULL, NULL)` returns 0; with resource 16 it returns `EINVAL`.
- T8 (covers F15): a change made before `fork` is seen by the child, a change in the child is not seen by the parent, a change in one thread is seen by another, and a change survives `execve`.
  Existing candidate: `tests/c/resource_determinism.c:259`-`300` covers `fork` only.
- T9 (covers F14): lower the `RLIMIT_NOFILE` soft limit to 16 with `prlimit64(0, ...)` and open files until failure; assert `EMFILE` exactly when the next descriptor would be 16 (owner decision 5 target). Lower `RLIMIT_CORE` to 0 and query it back. By the code the `EMFILE` assertion fails at this SHA, because the lowered limit is not enforced.
- T10 (covers F18): launch Hermit under a lowered host limit (`prlimit --nofile=64:64` on the launcher). The guest reads `RLIMIT_NOFILE` with `prlimit64(0, RLIMIT_NOFILE, NULL, &old)` (1048576) and opens descriptors until one fails. Assert that the run reports `HERMIT_HOST_NONDETERMINISM cause=host-rlimit syscall=openat errno=EMFILE` and ends with the infrastructure-error exit convention (exit 122 with `class=infrastructure-error cause=host-rlimit` on stderr, as proposed in the error-handling convention), not with the guest seeing `EMFILE` near descriptor 64. If Hermit instead detects at launch that it cannot configure the reported limit, the same report at launch satisfies the test. Today the guest gets the bare `EMFILE`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/326#issuecomment-5872921334
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/326: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; prlimit64 records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/326 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_prlimit64 answers from a fixed per-process virtual table shared with getrlimit/setrlimit, self-target only, real changes only to STACK/NOFILE/CORE without raising the hard limit, never enforced); linux_determinism nondeterministic -> conditional (a read of inherited process state); 17 flag behaviours and 9 tests listed; open questions on enforcement, other-pid/TID targets, error order and model-vs-emulate wording; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (reported pair is the limit in force, enforced by Detcore or configured on the host per resource; lowering accepted once a resource reaches it; interim host-rlimit infrastructure error, passed through today at files.rs:1130); target_policy stays model; T9 now asserts EMFILE at 16; new F18, T10; removed the enforcement, restricted-resources and model-vs-emulate wording questions (setrlimit is now model too); added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (Target: copy-in EFAULT, then target lookup and permission, then resource EINVAL, soft>hard EINVAL, hard-raise EPERM, old_rlim EFAULT last, per prlimit64/do_prlimit in kernel/sys.c; T5 now asserts EFAULT for an invalid resource with a bad new_rlim); target_policy stays model; no new F/T ids; removed the error-order question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target narrowed (fixed table with soft equal to hard; a lowering of any resource is forwarded to the host and recorded; a hard raise is EPERM; a soft raise back up to the hard limit is accepted and forwarded, F11); target_policy stays model; T1 expects the narrowed table; T2's lowered RLIMIT_CPU soft limit must now succeed; removed the standing-policy proposal from Open questions; the targets question stays
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T4 covered by tests/c/resource_determinism.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `prlimit64`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+prlimit64&body=Discussion+for+the+%60prlimit64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprlimit64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60prlimit64%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: prlimit64" (https://github.com/rrnewton/hermit/issues/326), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/326.

Unit tests at hermit f4055eb9cae4 for the helpers above: `prlimit_self_target_prefers_deterministic_process_identity` (`detcore/src/syscalls/sysinfo.rs:774`), `prlimit_accepts_exact_noop_for_restricted_resource` (`detcore/src/syscalls/sysinfo.rs:788`), `prlimit_rejects_actual_change_to_restricted_resource` (`detcore/src/syscalls/sysinfo.rs:816`), `prlimit_rejects_core_hard_limit_raise` (`detcore/src/syscalls/sysinfo.rs:848`), and for the table `defaults_are_fixed_and_cover_linux_resources` (`detcore/src/tool_local.rs:951`) and `cloned_process_state_changes_independently` (`detcore/src/tool_local.rs:978`). These are cargo unit tests, not ptrace runs.

### Open questions

- Targets (F6, F7): should a guest pid of a child or sibling process, or a TID of the caller's own thread group, be served from that process's table instead of `EPERM`? H1: a sibling thread's TID gets `EPERM` today (T6).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=prlimit64`, and by confirming hermit currently routes `prlimit64` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
prlimit64(0, RLIMIT_STACK, NULL, {rlim_cur=8192*1024, rlim_max=RLIM64_INFINITY}) = 0
```

That routing claim is history: at hermit f4055eb9cae4 the typed handler above serves the call.

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/326#issuecomment-5872921334
