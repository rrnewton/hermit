---
schema: syscall-pages/v1
syscall: setpriority
nr_x86_64: 141
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/450
  - https://github.com/rrnewton/hermit/issues/451
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setpriority&body=Discussion+for+the+%60setpriority%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetpriority.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setpriority%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`setpriority(which, who, prio)` sets the nice value of a thread, of every thread in a process group, or of every thread owned by a user. Detcore never touches the host. For every valid `which` it returns 0 and discards both `who` and `prio` (`setpriority_result`, `detcore/src/syscalls/misc.rs:162`); an unknown `which` gives `EINVAL`. This body proposes, together with `getpriority`'s tracking body, a virtual nice value per thread, so that the change is visible when read back and `ESRCH` is reported for a target that does not exist.

Manual reference: [setpriority(2)](https://man7.org/linux/man-pages/man2/getpriority.2.html).

## Linux semantics

The kernel checks `which`, clamps `prio` into the nice range -20 to 19, and then applies the value to every task that `which`/`who` selects. Nice is per thread: `PRIO_PROCESS` with a thread-group ID changes only the leader thread, and `who == 0` means the calling thread, not the whole process. The value is inherited across `fork`/`clone` and kept across `execve`. It sets the task's weight in the host's fair scheduler and has no effect on tasks running a real-time policy, though it is still stored.

Two permission rules apply to each target separately:

- The caller's effective uid must equal the target's real or effective uid, or the caller must have `CAP_SYS_NICE` in the target's user namespace. Otherwise the result is `EPERM`.
- Lowering the nice value below its current value needs `RLIMIT_NICE` to allow it (the lowest permitted nice is `20 - rlim_cur`) or `CAP_SYS_NICE` in the initial user namespace. Otherwise the result is `EACCES`. By this reading of the kernel source, root inside a user namespace, which is how Hermit's container maps the guest, does not pass the capability check, so the outcome depends on the host's `RLIMIT_NICE`. This was not measured.

For `PRIO_PGRP` and `PRIO_USER` the kernel applies the change to each matching task in turn. Tasks that pass the checks are changed even if others fail, and the call returns the error from the last task that failed, or 0. Setting the value a task already has succeeds; the 2026-07-24 host trace under Additional information shows `setpriority(PRIO_PROCESS, 0, 0) = 0`.

glibc's `nice(inc)` reads the current value with `getpriority`, calls `setpriority`, and returns the value read back with `getpriority`. `renice` and `nice(1)` are built on these calls.

### Flags and options

- F1: `PRIO_PROCESS` with `who > 0`: the thread whose ID is `who`. A thread-group ID changes only the leader thread.
- F2: `PRIO_PROCESS` with `who == 0`: the calling thread only
- F3: `PRIO_PGRP` with `who > 0`: every thread of every process in process group `who`
- F4: `PRIO_PGRP` with `who == 0`: the caller's process group
- F5: `PRIO_USER` with `who > 0`: every visible thread whose real uid is `who`
- F6: `PRIO_USER` with `who == 0`: the caller's real uid
- F7: no task matches `which`/`who`: `ESRCH`
- F8: `which` is not one of the three selectors: `EINVAL`, checked before anything else
- F9: `prio` outside -20..19 is clamped into that range, not rejected
- F10: ownership check: `EPERM` when the caller's effective uid matches neither the target's real nor effective uid and the caller lacks `CAP_SYS_NICE` in the target's user namespace
- F11: lowering the nice value: `EACCES` unless `RLIMIT_NICE` permits it or the caller has `CAP_SYS_NICE` in the initial user namespace
- F12: several targets (`PRIO_PGRP`, `PRIO_USER`): each task is changed or refused on its own; the result is the last failure's error, or 0, and successful changes are kept
- F13: effect: the new value is what `getpriority`, `sched_getattr`'s `sched_nice` and `/proc/<pid>/stat` report afterwards; it is inherited across `fork`/`clone`, kept across `execve`, and changes the task's host scheduling weight

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Nothing in the call races. Its result is fixed once these inputs are fixed:

- the nice value the program started with, inherited from whoever launched it, which decides whether a request lowers the value (F11);
- `RLIMIT_NICE` and the caller's capabilities (F10, F11);
- the set of tasks that `which`/`who` selects (F3, F5, F7);
- changes made by processes outside the guest, for example a host `renice`.

The applied value also changes host scheduling weight. That changes timing, not results, for a program whose threads Hermit runs one at a time, so it is not a source of different output under Hermit's scheduler.

The two legacy issues disagreed here, and the merge resolves it this way. https://github.com/rrnewton/hermit/issues/450 listed current nice, `RLIMIT_NICE`, capabilities, target membership and host scheduler policy as sources. https://github.com/rrnewton/hermit/issues/451 called the call "effectively a no-op for determinism" because nice does not change computed results under Hermit's scheduler. Both are right about different things: https://github.com/rrnewton/hermit/issues/451 about the effect on scheduling, https://github.com/rrnewton/hermit/issues/450 about the return value. The return value is guest-visible, so issue 450's sources decide `linux_determinism`.

### Nondeterminism sources

- Launch nice value, `RLIMIT_NICE` and capabilities, which decide `EACCES`/`EPERM`
- Target membership for `PRIO_PGRP`/`PRIO_USER`, and whether a `PRIO_PROCESS` target exists
- Outside processes changing nice values
- Host scheduling weight, which perturbs timing only

## Hermit design

Target (owner decision 7, 2026-10-04): `model`, together with `getpriority`. The value `setpriority` sets is modelled per thread and reported by `getpriority`, with no effect on Hermit's own scheduling. Detcore keeps a virtual nice value per guest thread, starting at 0 whatever the host launched Hermit with, and `setpriority` updates it. As an emulated call it makes the checks it keeps in the kernel's order (`setpriority`, `set_one_prio` and `set_one_prio_perm` in `kernel/sys.c`; owner decision 6): `EINVAL` for an unknown `which` before anything else (F8, as today at `detcore/src/syscalls/misc.rs:162`), then the clamp of `prio`, then the target lookup (`ESRCH`), then per target the ownership check (`EPERM`, if it is ever made; see Permissions below) before the nice-lowering check against the guest's `RLIMIT_NICE` (`EACCES`):

- `prio` is clamped to -20..19 (F9).
- `PRIO_PROCESS` resolves `who` against Detcore's own task table, as `handle_sched_setattr` already does with `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`), and returns `ESRCH` when no guest task matches (F1, F7).
- `PRIO_PGRP` and `PRIO_USER` apply to every matching guest thread (F3 to F6, F12).
- The value is inherited across `clone` and kept across `execve`, and `getpriority` reports it (F13).
- The host nice value is never changed. Nice stays inert for scheduling (`detcore/src/syscalls/misc.rs:411`); only the reported value changes.

The code at hermit f4055eb9cae4 is an `emulate` policy:

- The typed arm (`detcore/src/lib.rs:2528`) calls `handle_setpriority` (`detcore/src/syscalls/misc.rs:449`), which returns `setpriority_result(call.which())` (`detcore/src/syscalls/misc.rs:162`). That checks `which` with `is_valid_prio_which` (`detcore/src/syscalls/misc.rs:137`) and returns 0, ignoring `who` and `prio`.
- F8 matches Linux. F2 succeeds, as Linux does when the caller may make the change.
- F7 differs: a target that does not exist returns 0 instead of `ESRCH`.
- F13 differs: a later `getpriority` still returns the raw value 20 (`getpriority_result`, `detcore/src/syscalls/misc.rs:150`), so `nice -n 5 nice` prints 0 and Python's `os.nice(5)` returns 0.
- F9 and F12 are unobservable because nothing is stored.
- F10 and F11 never fail.

Permissions (F10, F11): the model keeps today's answer and accepts the request, apart from the guest's own `RLIMIT_NICE` (next paragraph). This follows the rule `handle_sched_setattr` states for the same question (`detcore/src/syscalls/threads.rs:2294`): an answer that depends on the caller's privileges or the host's configuration is not reproduced, and a negative nice is one of the cases it lists (`detcore/src/syscalls/threads.rs:2300`). The handler's own doc comment for `setpriority` gives a different reason, that the guest runs as a uid-0 container principal so the real call "would succeed anyway". For lowering the value (F11) that is not what the kernel source says, as Linux semantics explains. The accepted answer is the same either way; the reason recorded here is the `sched_setattr` rule.

Owner decision 5 (2026-10-04: limits become real and deterministic) changes the `RLIMIT_NICE` half of F11. The limit the guest reads is the limit in force, so a nice-lowering request is checked against the guest's own reported `RLIMIT_NICE` (lowest permitted nice `20 - rlim_cur`) and refused with `EACCES` past it. That limit is guest state and deterministic, not host configuration, so the `sched_setattr` rule above does not apply to it. Detcore enforces it itself, because the host nice is never changed and no host limit is involved; for the same reason there is no `cause=host-rlimit` infrastructure error for this syscall. With today's reported default of `RLIM64_INFINITY` (`detcore/src/tool_local.rs:278`-`292`) every lowering is permitted, which is what the code returns now (`detcore/src/syscalls/misc.rs:162`); the difference appears only once a guest lowers its own `RLIMIT_NICE`, which it cannot do at this SHA (`detcore/src/syscalls/sysinfo.rs:147`-`151`). The capability half of F11 and the ownership check F10 are unchanged here.

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): `RLIMIT_NICE` is reported as one fixed value with soft equal to hard, and it stays `RLIM_INFINITY`, so `nice -n -5` and `renice -n -5` keep working as they do today and raising nice (`nice -n 10 make`) is unaffected. A raise of the hard limit is `EPERM`, which at `RLIM_INFINITY` only matters after the guest has lowered it. A lowering is recorded and Detcore enforces it as above (T7); the host is not involved, because the host nice is never changed. The refinement lifts the `setrlimit` restriction T7 depends on (`detcore/src/syscalls/sysinfo.rs:147`-`151`).

Why `model` over keeping `emulate`: both are deterministic, so the choice is about faithfulness. The model is one integer per thread plus a lookup over threads Detcore already tracks, and it removes the F7 and F13 differences without letting the host's nice value back in. PIDs under ptrace come from the container's PID namespace (`getpid` is passed through at `detcore/src/syscall_classification.rs:698`), so `who` maps onto guest threads.

`passthrough`, which https://github.com/rrnewton/hermit/issues/451 recommended, is rejected. It would change the real host nice of guest threads, make `EACCES` depend on the host's `RLIMIT_NICE` and launch nice, and still leave `getpriority` constant, so the guest would see its change both accepted and missing.

This contradicts what is pinned today, and owner decision 7 settles it in favour of `model`. `tests/c/getpriority_identity.c` asserts that the value read back after `setpriority` is still 20 (`tests/c/getpriority_identity.c:84`); that step changes with the model (T2), and so does the unit test `setpriority_accepts_any_change_for_valid_target` (`detcore/src/syscalls/misc.rs:1053`). `get_and_set_priority_reject_unknown_which_with_einval` (`detcore/src/syscalls/misc.rs:1062`) stays valid. The `TODO-HUMAN-REVIEW(#806)` comments (`detcore/src/syscalls/misc.rs:159`, `detcore/src/syscalls/misc.rs:440`; https://github.com/rrnewton/hermit/pull/806) are answered by the same decision.

## Recommended tests

- T1 (covers F2, F8): under `hermit run --verify` on ptrace, a raw `syscall(SYS_setpriority, PRIO_PROCESS, 0, 10)` returns 0, and `which = 999` returns `EINVAL`.
  Existing partial coverage: `tests/c/getpriority_identity.c:75` (cell `c-programs/getpriority-identity`, `tests/e2e/manifests/c-programs.yaml:14660`, verify ci on ptrace) checks the successful call, but tests the invalid `which` only for `getpriority`. The compat corpus row `nice` (`tests/e2e/manifests/compat.yaml:182`) runs `nice -n 1 /bin/echo`, which reaches `setpriority` through glibc's `nice()`.
- T2 (covers F2, F9, F13): `setpriority(PRIO_PROCESS, 0, 5)` followed by raw `getpriority` returns 15. A request for 100 reads back 1 (nice 19), and a request for -100 reads back 40 (nice -20). A forked child inherits the value, it survives `execve`, and a second thread keeps its own value. `nice -n 5 nice` prints 5.
  Today this fails, because Hermit returns 20; it replaces step (3) of `tests/c/getpriority_identity.c`.
- T3 (covers F1, F7): `PRIO_PROCESS` with a child's PID changes the child only, and with a non-leader thread's ID changes that thread only. A PID that does not exist in the guest returns `ESRCH`.
- T4 (covers F3, F4, F12): two processes in one process group. `PRIO_PGRP` with the group ID, and with `who = 0`, changes both. A group ID that does not exist returns `ESRCH`.
- T5 (covers F5, F6): `PRIO_USER` with `who = 0`, and with the guest's virtual uid 0, changes every guest process. A uid that no guest process has returns `ESRCH`.
- T6 (covers F10, F11): lowering the value from 0 to -5 succeeds under Hermit whatever the host allows. Run once with the host's `RLIMIT_NICE` set to 0 for the launching shell, to show the host limit does not reach the guest. A child that has switched to uid 1000 calling `setpriority` on its uid-0 parent gets `EPERM` on Linux; record what Hermit returns.
- T7 (covers F11): under `hermit run --verify` on ptrace, a child that has dropped `CAP_SYS_NICE` (for example by switching to uid 1000 with `setuid`, which clears its effective capabilities) lowers its own `RLIMIT_NICE` to 25 with a raw `setrlimit`, so the lowest permitted nice is -5. `setpriority(PRIO_PROCESS, 0, -5)` returns 0 and `setpriority(PRIO_PROCESS, 0, -6)` returns `EACCES`, exactly at the limit it set (owner decision 5). Fails at this SHA: the `setrlimit` returns `EPERM` (`detcore/src/syscalls/sysinfo.rs:147`-`151`), and `setpriority` would accept -6 (`detcore/src/syscalls/misc.rs:162`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/450: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/451 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged duplicate https://github.com/rrnewton/hermit/issues/451 (its PASS-THRU recommendation rejected; its no-scheduling-effect claim kept); linux_determinism stays conditional; target_policy investigate -> model (virtual nice per thread with read-back and ESRCH, permissions accepted per the sched_setattr rule), replacing today's no-op emulation; needs owner confirmation together with getpriority because tests/c/getpriority_identity.c pins the opposite; 13 flag behaviours and 6 tests
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (the RLIMIT_NICE half of F11 follows the guest's reported limit, enforced by Detcore since host nice is never changed, so no host-rlimit infrastructure error applies); target_policy stays model; new T7; removed the question on EACCES via the virtual RLIMIT_NICE; added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (per-thread nice modelled and read back by getpriority with no scheduling effect; kept checks in kernel order EINVAL, clamp, ESRCH, then EPERM before EACCES per set_one_prio); target_policy stays model; no new F/T ids; removed the model-vs-emulate question and the unit-test note (folded into Hermit design)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target narrowed (RLIMIT_NICE fixed with soft equal to hard at RLIM_INFINITY, so negative nice stays permitted; a hard raise is EPERM; a lowering is recorded and enforced by Detcore; no host involvement); target_policy stays model; T7 unchanged; removed the standing-policy proposal and the empty Open questions heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setpriority`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setpriority&body=Discussion+for+the+%60setpriority%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetpriority.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setpriority%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setpriority" (https://github.com/rrnewton/hermit/issues/450), legacy variant A1. Duplicate https://github.com/rrnewton/hermit/issues/451, filed the same day with the same title (legacy variant A1), was merged into this body on 2026-10-03; its comment history (0 comments) stays on that issue, and this issue's own history stays on https://github.com/rrnewton/hermit/issues/450.

What the merge kept from https://github.com/rrnewton/hermit/issues/451, and what it reconciled:

- Its claim that nice does not change computed results or the deterministic schedule is kept, under Determinism analysis.
- Its claim that the target PID and uid must be virtualized is kept, as the requirement that `who` resolves against Detcore's task table.
- Its note that lowering nice below 0 needs privilege is kept, and refined into F11.
- Its **Recommended Action: PASS-THRU** is rejected in favour of a handler, for the reasons under Hermit design. https://github.com/rrnewton/hermit/issues/450 recommended NEEDS-HANDLER, and the code already has a handler.

### 2026-07-24 host strace evidence

Both legacy issues recorded the same probe. Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setpriority`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setpriority` in the UNCLASSIFIED temporary-passthrough block. (That was the state on 2026-07-24; at f4055eb9cae4 the call is Determinized with the handler described above.)

Observed strace line, identical in both issues:
```text
setpriority(PRIO_PROCESS, 0, 0) = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.* The duplicate https://github.com/rrnewton/hermit/issues/451 came from the sweep covering ranks 151-250 of the same list, also host testing and analysis only.
