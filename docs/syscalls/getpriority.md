---
schema: syscall-pages/v1
syscall: getpriority
nr_x86_64: 140
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/607
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpriority&body=Discussion+for+the+%60getpriority%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpriority.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpriority%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`getpriority(which, who)` reports the scheduling priority (nice value) of a process, a process group or a user's processes. Detcore never asks the host. For every valid `which` it returns the constant raw value 20, which means nice 0, and it ignores `who` (`detcore/src/syscalls/misc.rs:150`). An unknown `which` gives `EINVAL`. This body proposes tracking a virtual nice value, so that the result reflects the guest's own `setpriority` calls and `ESRCH` is reported for a target that does not exist.

## Linux semantics

The kernel looks up every task that `which`/`who` selects and returns the highest priority among them, which is the lowest nice value. Nice is per thread. It is inherited across `fork`/`clone` and kept across `execve`, and it changes through `setpriority` and `sched_setattr`.

The raw syscall returns `20 - nice`, a value from 1 to 40, so that the result is never negative. glibc's `getpriority()` converts it back to a nice value from -20 to 19. glibc's `nice()` calls `setpriority` and then returns the value read back with `getpriority`.

Reading needs no permission. Only tasks visible in the caller's PID namespace are considered.

### Flags and options

- F1: `PRIO_PROCESS` with `who > 0`: the thread or process whose ID is `who`
- F2: `PRIO_PROCESS` with `who == 0`: the calling thread
- F3: `PRIO_PGRP` with `who > 0`: every member of process group `who`. The result is the highest priority among them.
- F4: `PRIO_PGRP` with `who == 0`: the caller's process group
- F5: `PRIO_USER` with `who > 0`: every visible process whose real uid is `who`. The result is the highest priority among them.
- F6: `PRIO_USER` with `who == 0`: the caller's real uid
- F7: no task matches `which`/`who`: `ESRCH`
- F8: `which` is not one of the three selectors: `EINVAL`, checked before `who`
- F9: return encoding: the raw value is `20 - nice` (1 to 40). It reflects nice changes made by `setpriority`/`sched_setattr` and the value inherited at `fork`.

## Determinism analysis

`linux_determinism: conditional`; the legacy value was `nondeterministic`. Nothing in the call races: it reads nice values that only explicit calls change. The result is fixed once three inputs are fixed:

- the nice value the program was started with, which is inherited from whoever launched it;
- the set of visible processes that `PRIO_PGRP`/`PRIO_USER` match;
- changes made by processes outside the guest, for example a host `renice`.

With those fixed, the answer depends only on the guest's own `setpriority` calls and its `fork` tree, which run in a deterministic order under Hermit. The legacy note, "observes host scheduler nice state and target membership", names exactly these inputs. They are inputs that can be held fixed, not sources of timing variation, so `conditional` describes them more accurately than `nondeterministic`.

## Hermit design

Target (owner decision 7, 2026-10-04): `model`. The `setpriority` value is modelled per thread and reported by `getpriority`, with no effect on Hermit's own scheduling. Detcore keeps a virtual nice value for each guest thread, starting at 0 whatever the host's launch nice is, and answers `getpriority` from it, in the kernel's order (`getpriority` in `kernel/sys.c`): `EINVAL` for an unknown `which` first (F8, as today at `detcore/src/syscalls/misc.rs:150`), then the target lookup, `ESRCH` when nothing matches:

- it is inherited across `clone` and kept across `execve`;
- it is updated by `setpriority` (and by `sched_setattr`'s nice field);
- `PRIO_PGRP` and `PRIO_USER` take the highest priority over the guest threads Detcore tracks;
- `ESRCH` is returned when no guest task matches.

Nice stays inert for scheduling (`detcore/src/syscalls/misc.rs:411`); only the reported value changes.

Resource limits (owner decision 5, 2026-10-04: limits become real and deterministic): the only limit that affects what `getpriority` reports is `RLIMIT_NICE`, through which nice values `setpriority` may set. Under decision 5 that permission check uses the guest's reported `RLIMIT_NICE` (today `RLIM64_INFINITY`, `detcore/src/tool_local.rs:278`-`292`), and Detcore enforces it itself, because neither call reaches the host (`detcore/src/syscalls/misc.rs:150`, `detcore/src/syscalls/misc.rs:162`). No host limit can disagree with the reported one here, so there is no `cause=host-rlimit` infrastructure error and no divergence flag for this syscall. The rule itself is tracked in the `setpriority` body.

Target, narrowed by the decision-5 refinement (owner, 2026-10-04: "I'm also fine with this resource limit proposal"; standing least-common-denominator policy): `RLIMIT_NICE` is reported as one fixed value with soft equal to hard, and it stays `RLIM_INFINITY`, so every nice value `setpriority` accepts today stays permitted and `getpriority` can report any raw value from 1 to 40. A raise of the hard limit is `EPERM`, which at `RLIM_INFINITY` only matters after the guest has lowered it. A lowering is recorded and Detcore enforces it in `setpriority`; the host is not involved, because the host nice is never changed.

The code today is an `emulate` policy:

- `getpriority_result` (`detcore/src/syscalls/misc.rs:150`) returns `Ok(20)` for any valid `which` and ignores `who`, after the `which` check in `is_valid_prio_which` (`detcore/src/syscalls/misc.rs:137`).
- `handle_getpriority` (`detcore/src/syscalls/misc.rs:419`) is reached from the typed arm at `detcore/src/lib.rs:2525`.
- `setpriority` is accepted as a no-op that returns 0 (`setpriority_result`, `detcore/src/syscalls/misc.rs:162`; `handle_setpriority`, `detcore/src/syscalls/misc.rs:449`).

This is deterministic, and it hides the host launch nice, which is correct. It differs from Linux in three ways a program can see:

- **No read-back.** A program that sets its own nice value and reads it back sees 0. `nice -n 5 nice` prints 0 instead of 5, and Python's `os.nice(5)` returns 0, because glibc's `nice()` reads the value back with `getpriority`. This is F9.
- **No `ESRCH`.** `getpriority(PRIO_PROCESS, <pid that does not exist>)` returns 20 instead of failing. This is F1, F3, F5 and F7.
- **No aggregation.** `PRIO_PGRP`/`PRIO_USER` cannot report the highest priority of members whose nice values differ. This is F3 and F5.

Why `model` over keeping `emulate`: both are deterministic, so the choice is about how faithful the result is. The model is small: one integer per thread, plus a lookup over the guest threads Detcore already tracks. PIDs under ptrace come from the container's PID namespace (`getpid` is passed through at `detcore/src/syscall_classification.rs:698`), so the lookup maps `who` onto those threads. It removes every difference listed above without letting the host's nice value back in.

This contradicts what is pinned today, and owner decision 7 settles it in favour of `model`. The tests below require `setpriority` to be observable, and the existing guest `tests/c/getpriority_identity.c` asserts the opposite: its step (3) checks that the value read back is still 20 after `setpriority` (`tests/c/getpriority_identity.c:84`). That step changes with the model, and so do the unit tests `getpriority_reports_default_nice_for_every_target` (`detcore/src/syscalls/misc.rs:1042`) and `setpriority_accepts_any_change_for_valid_target` (`detcore/src/syscalls/misc.rs:1053`). `get_and_set_priority_reject_unknown_which_with_einval` (`detcore/src/syscalls/misc.rs:1062`) stays valid. The `TODO-HUMAN-REVIEW(#806)` comments on the handlers (`detcore/src/syscalls/misc.rs:144`, `detcore/src/syscalls/misc.rs:408`; https://github.com/rrnewton/hermit/pull/806) are answered by the same decision.

## Recommended tests

- T1 (covers F2, F8, F9): under `hermit run --verify` on ptrace, a raw `syscall(SYS_getpriority, PRIO_PROCESS, 0)` returns 20 at startup, whatever nice the host launched Hermit with. Run the test once under `nice -n 10` to show the host value does not reach the guest. `which = 999` returns `EINVAL`.
  Existing candidate: `tests/c/getpriority_identity.c` (cell `c-programs/getpriority-identity`, `tests/e2e/manifests/c-programs.yaml:14660`, verify ci on ptrace). It covers the startup value and `EINVAL`, but not the launch under `nice`.
- T2 (covers F1, F7): `PRIO_PROCESS` with the guest's own PID, with a child's PID, and with a thread ID returns the target's value. A PID that does not exist in the guest returns `ESRCH`.
- T3 (covers F9, F1, F2): `setpriority(PRIO_PROCESS, 0, 5)` followed by `getpriority` returns 15, which is nice 5. A forked child inherits it, and it survives `execve`. Two threads keep separate values.
  Today this would fail, because Hermit returns 20; see Hermit design.
- T4 (covers F3, F4): put two processes in one process group with nice 3 and nice 7. `PRIO_PGRP` with the group ID, and with `who = 0`, returns 17, the highest priority. A group ID that does not exist returns `ESRCH`.
- T5 (covers F5, F6): `PRIO_USER` with `who = 0`, and with the guest's virtual uid 0, returns the highest priority over guest processes only. A uid that no guest process has returns `ESRCH`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/607: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · p3-samples · p3: linux_determinism nondeterministic -> conditional (no race; the inputs are launch nice, visible task membership and outside renice); target_policy model (a virtual nice value per thread, giving read-back after setpriority and ESRCH), replacing today's constant-20 emulation; needs owner confirmation because tests/c/getpriority_identity.c pins the opposite; 9 flag behaviours and 5 tests
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5 (owner decision 5): target described (the RLIMIT_NICE permission check uses the guest's reported limit, enforced by Detcore since neither call reaches the host, so no host-rlimit infrastructure error applies); target_policy stays model; no new F/T ids; removed the RLIMIT_NICE permission question; added the unconfirmed standing-policy proposal as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 7 applied (per-thread nice modelled, reported by getpriority, no scheduling effect; check order EINVAL then ESRCH per kernel getpriority); target_policy stays model; no new F/T ids; removed the model-vs-emulate question and the unit-test note (folded into Hermit design)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d5r (owner approved the decision-5 refinement): target narrowed (RLIMIT_NICE fixed with soft equal to hard at RLIM_INFINITY; a hard raise is EPERM; a lowering is recorded and enforced by Detcore; no host involvement); target_policy stays model; no new F/T ids; removed the standing-policy proposal and the empty Open questions heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getpriority`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpriority&body=Discussion+for+the+%60getpriority%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpriority.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpriority%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getpriority" (https://github.com/rrnewton/hermit/issues/607), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
