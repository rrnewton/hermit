---
schema: syscall-pages/v1
syscall: ioprio_get
nr_x86_64: 252
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/619
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ioprio_get&body=Discussion+for+the+%60ioprio_get%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fioprio_get.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ioprio_get%60%2C+or+in+what+the+page+says%3F%0A
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

`ioprio_get(which, who)` returns the I/O scheduling class and level of a thread, or the best one among a process group or a user's processes. Detcore emulates it with fixed answers (`detcore/src/syscalls/threads.rs:2465`): 0 for `IOPRIO_WHO_PROCESS`, 16388 (best-effort class, level 4) for `IOPRIO_WHO_PGRP` and `IOPRIO_WHO_USER`, and `EINVAL` for any other `which`. It never asks the host, so a priority inherited from outside Hermit cannot leak in. It also ignores `who` and anything an earlier `ioprio_set` asked for.

Manual reference: Linux man-pages ioprio_get(2).

## Linux semantics

The result packs the class into bits 13-15 and the level into bits 0-2. Linux 6.5 and later also use bits 3-12 for a priority hint. `IOPRIO_WHO_PROCESS` returns the raw stored value of one thread. That value is 0 (`IOPRIO_CLASS_NONE`) for a thread that never set a priority and did not inherit one, and that thread follows its CPU nice value.

`IOPRIO_WHO_PGRP` and `IOPRIO_WHO_USER` walk every matching thread and return the best priority found, where a lower class number wins and then a lower level. A thread with no explicit priority counts with its effective one: the class follows its scheduling policy and the level is `(nice + 20) / 5`. For nice 0 that is best-effort level 4, 16388, which is what the host returned for both forms.

`who = 0` means the caller, its group, or its real UID. An unknown target gives `ESRCH`, and an unknown `which` gives `EINVAL`. The value is inherited across `fork` and `clone`, so a process started under `ionice -c 3` reads 24576.

### Flags and options

- F1: `which = IOPRIO_WHO_PROCESS` (1): raw value of the thread whose TID is `who`, 0 meaning the caller. It is 0 when nothing was set, reflects an earlier `ioprio_set`, and is inherited from the parent. Measured: 0, then 24576 after setting IDLE, and 24576 in a child forked afterwards.
- F2: `which = IOPRIO_WHO_PGRP` (2): best effective priority in process group `who`, 0 meaning the caller's. Measured: 16388.
- F3: `which = IOPRIO_WHO_USER` (3): best effective priority among threads whose real UID is `who`, 0 meaning the caller's. Measured: 16388.
- F4: any other `which`: `EINVAL`. Measured for `which = 0`.
- F5: `who` names no thread, group or user: `ESRCH`. Measured for PROCESS and PGRP with `who = 999999999` and for USER with `who = 4000000`.
- F6: effective priority for threads with no explicit value, used by F2 and F3: class from the scheduling policy, level `(nice + 20) / 5`

## Determinism analysis

`linux_determinism: conditional` (changed from the legacy `nondeterministic`). The answer is a pure function of the target threads' stored priorities, their nice values and scheduling policies, and which targets exist. It repeats when those are the same. It does not when:

- The process tree inherits a priority from outside. A shell started under `ionice -c 3`, or a job runner that lowers I/O priority, gives the guest a different F1 value than an interactive shell.
- PGRP and USER (F2, F3) include host processes outside the guest, whose priorities and nice values change independently. With `who = 0`, USER covers every process the user owns on the machine.
- A non-zero `who` names host TIDs, groups or UIDs, so `ESRCH` depends on the host process table.

### Nondeterminism sources

- The priority inherited from the process that started Hermit
- Priorities, nice values and scheduling policies of host processes matched by `IOPRIO_WHO_PGRP` and `IOPRIO_WHO_USER`
- Host TID, group and UID allocation, which decides `ESRCH`

## Hermit design

Target policy: `model` (owner decision 7, 2026-10-04); the p3 plan below was `emulate`, and the Target paragraph at the end of this section replaces it. The dispatch arm is `detcore/src/lib.rs:2689`. `handle_ioprio_get` (`detcore/src/syscalls/threads.rs:2465`) returns `virtual_ioprio(call.which())` and never injects the call. `virtual_ioprio` (`detcore/src/syscalls/threads.rs:391`) maps PROCESS to 0, PGRP and USER to `IOPRIO_DEFAULT_EFFECTIVE`, which is best-effort level 4 (`detcore/src/syscalls/threads.rs:88`), and everything else to `EINVAL`. The handler's doc comment (`detcore/src/syscalls/threads.rs:2459`) says the values are meant to be the raw `IOPRIO_CLASS_NONE` and the effective default of a `SCHED_OTHER` thread at nice 0. That matches what an unmodified host process sees (F1, F2, F3 measurements above). The classification table lists `ioprio_get` as determinized (`detcore/src/syscall_classification.rs:625`). This came from https://github.com/rrnewton/hermit/pull/881, the `TODO-HUMAN-REVIEW(PR-881)` tags at `detcore/src/lib.rs:2688` and `detcore/src/syscalls/threads.rs:390`.

Why the p3 plan chose `emulate`:

- `passthrough` leaks the host-inherited priority and the state of host processes, which are the sources listed above.
- `refuse` breaks `ionice -p`, and other tools that only query.
- `model`, a per-thread virtual priority, was left to the owner by the p3 plan; decision 7 chose it.

Where the emulation differs from Linux today:

- `who` is ignored, so `ESRCH` (F5) never happens, and a query about another guest thread gets the same fixed answer.
- An earlier `ioprio_set` has no effect, because the setter stores nothing (see the ioprio_set body). After the guest sets IDLE, F1 still returns 0, where Linux returns 24576, and a child does not inherit it.
- PGRP and USER always return 16388, even when a guest thread raised its own priority, or set IDLE, or runs at a nice value whose `(nice + 20) / 5` is not 4 (F6). Detcore's `setpriority` stores nothing either (`detcore/src/syscalls/misc.rs:449` returns `setpriority_result`, `detcore/src/syscalls/misc.rs:162`, which checks only `which`), so there is no virtual nice value to derive a level from.

All of these answers are host-independent, which is the property `--verify` checks.

Target (owner decision 7, with decision 6): `ioprio_get` reads the per-thread virtual I/O priority that the ioprio_set body adds to Detcore's per-thread state (next to `timer_slack_ns`, `detcore/src/tool_local.rs:1795`), which starts at 0, is copied at fork and clone (as at `detcore/src/lib.rs:1555`) and survives `execve`. Nothing is read from the host, so a priority inherited from outside Hermit still cannot leak in (T5). The order follows the kernel's `ioprio_get` syscall in `block/ioprio.c`: the `which` switch first, so an unknown `which` gives `EINVAL` (F4); then target resolution, where no match gives `ESRCH` (F5). `IOPRIO_WHO_PROCESS` resolves `who` with `thread_is_live` (`detcore/src/tool_global.rs:4032`), 0 meaning the caller, and returns that thread's raw recorded value (kernel `get_task_raw_ioprio()`), so 0 until a set and the set value afterwards, including in a child forked later (F1). `IOPRIO_WHO_PGRP` walks the guest processes in group `who` through Detcore's process-group table (`detcore/src/scheduler.rs:1067`), and `IOPRIO_WHO_USER` walks guest threads whose virtual real UID is `who`; each returns the numerically smallest effective value among the matches (kernel `ioprio_best()`), where a thread whose recorded class is `IOPRIO_CLASS_NONE` counts as the class its virtual scheduling policy gives (`SCHED_IDLE` gives IDLE, a real-time or deadline policy gives RT, anything else BE; kernel `task_nice_ioclass()`) at level `(nice + 20) / 5` (kernel `task_nice_ioprio()`) (F2, F3, F6). The policy and nice come from the per-thread scheduling record that the sched_setscheduler and setpriority bodies add under the same decision. Today `virtual_ioprio` ignores `who` and returns fixed answers (`detcore/src/syscalls/threads.rs:391`).

## Recommended tests

- T1 (covers F1, F4): `ioprio_get(IOPRIO_WHO_PROCESS, 0)` returns 0 under `hermit run --strict --verify` on ptrace, and an invalid `which` returns `EINVAL`.
  Existing candidates: `tests/c/scheduler_policy_queries.c:71`, which checks that `syscall(SYS_ioprio_get, 1, 0)` is 0, run by cell `c-programs/scheduler-policy-queries` (`tests/e2e/manifests/c-programs.yaml:7864`, verify ci on ptrace), and `scheduler_policy_queries_are_deterministic` at `hermit-cli/tests/scheduler_policy_queries.rs:30`, which runs that guest with `--backend=ptrace`. Neither checks `EINVAL`; the unit test `ioprio_query_reports_fixed_raw_and_effective_defaults` (`detcore/src/syscalls/threads.rs:2927`) does, but it calls `virtual_ioprio` directly, not through a guest.
- T2 (covers F2, F3, F6): PGRP and USER queries with `who = 0` return 16388 under Hermit. Repeat after `setpgid(0, 0)` and `setpriority(PRIO_PROCESS, 0, 10)`: the PGRP query must return best-effort level 6 (16390), as Linux does for a group whose only member is at nice 10; today Hermit returns 16388.
- T3 (covers F5): PROCESS, PGRP and USER queries for a `who` that does not exist inside the guest. Each must give `ESRCH` under Hermit, as on Linux; today Hermit returns its fixed answer.
- T4 (covers F1): set IDLE with `ioprio_set`, read it back, then fork and read it in the child. Both reads must return 24576, as on Linux; Hermit returns 0 today. A PGRP query in a group that also holds a member at the default priority still returns 16388, as measured natively.
- T5 (covers F1, F2): start Hermit itself under `ionice -c 3` on the host and check that the guest still reads 0 for PROCESS and 16388 for PGRP. This is the host-leak property the emulation exists for.
- T6 (covers F2, F6): after `setpgid(0, 0)` and an accepted `sched_setscheduler(0, SCHED_FIFO, {1})`, the PGRP query returns real-time class level 4 (8196); after an accepted switch to `SCHED_IDLE` it returns idle class level 4 (24580). Today Hermit returns 16388 for both.
  Related but not coverage: `ionice_query_is_deterministic_under_strict_verify` at `hermit-cli/tests/command_strict_verify.rs:528` runs `ionice -p 0` but is `#[ignore]`d.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/619: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (fixed host-independent answers: PROCESS 0, PGRP/USER 16388, other which EINVAL); linux_determinism nondeterministic -> conditional (host-inherited priority and host processes in PGRP/USER are the only variable inputs); 6 flag behaviours and 5 tests listed; open question for the owner on set-to-get round trip and ESRCH, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (reads the per-thread virtual I/O priority set by ioprio_set and inherited at fork; PGRP and USER answers derived from the guest's virtual policy and nice; ESRCH from the guest's own tables, after the which check); target_policy emulate -> model; new T6; removed all three open questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `ioprio_get`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ioprio_get&body=Discussion+for+the+%60ioprio_get%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fioprio_get.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ioprio_get%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: ioprio_get" (https://github.com/rrnewton/hermit/issues/619), legacy variant B.

### 2026-10-03 native probe (host kernel 7.1.3, unprivileged user)

Raw `syscall(252, ...)` from Python ctypes. `get(PROCESS, 0)` gave 0, `get(PGRP, 0)` and `get(USER, 0)` gave 16388, `get(0, 0)` gave `EINVAL`, PROCESS and PGRP with `who = 999999999` gave `ESRCH`, and USER with `who = 4000000` gave `ESRCH`. After `ioprio_set(PROCESS, 0, IDLE)`, `get(PROCESS, 0)` gave 24576 in the caller and in a child forked afterwards, while `get(PGRP, 0)` still gave 16388.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
