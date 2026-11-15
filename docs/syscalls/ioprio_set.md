---
schema: syscall-pages/v1
syscall: ioprio_set
nr_x86_64: 251
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/620
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ioprio_set&body=Discussion+for+the+%60ioprio_set%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fioprio_set.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ioprio_set%60%2C+or+in+what+the+page+says%3F%0A
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

`ioprio_set(which, who, ioprio)` sets the block-layer I/O scheduling class and level for a thread, a process group, or every process of a user. Detcore emulates it as an unconditional no-op success (`detcore/src/syscalls/threads.rs:2445`): nothing reaches the host kernel and nothing is remembered, so `ionice` runs under `--strict`. The emulation does not yet check its arguments the way Linux does, and a later `ioprio_get` does not see the value that was set.

Manual reference: Linux man-pages ioprio_set(2).

## Linux semantics

`ioprio` packs a class into bits 13-15, a level into bits 0-2, and, since Linux 6.5, a priority hint into bits 3-12 (`IOPRIO_HINT_*` in `include/uapi/linux/ioprio.h`). The kernel checks the value first and `which` second. The value check rejects class `IOPRIO_CLASS_INVALID` (7) and any undefined class, rejects `IOPRIO_CLASS_NONE` with a non-zero level, and requires `CAP_SYS_NICE` or `CAP_SYS_ADMIN` for `IOPRIO_CLASS_RT`. On the test host, class 5 gave `EINVAL` with a valid `which`, and `IOPRIO_CLASS_NONE` with level 1 gave `EINVAL`.

`who` is resolved after that. `IOPRIO_WHO_PROCESS` names one thread by TID (0 means the caller), `IOPRIO_WHO_PGRP` names a process group (0 means the caller's), and `IOPRIO_WHO_USER` names a UID (0 means the caller's real UID). An unknown target gives `ESRCH`. Changing a thread owned by another user without privilege gives `EPERM`. The new priority is stored in the target's I/O context. A child created by `fork` or `clone` inherits it, and `ioprio_get` reads it back. The value affects only how the block layer orders requests, never the data a read or write returns.

The PGRP and USER forms change every matching thread in the host PID namespace the caller can see. Without a private PID namespace, `IOPRIO_WHO_USER` with `who = 0` reaches every process the user owns on the machine.

### Flags and options

- F1: `which = IOPRIO_WHO_PROCESS` (1): set the priority of the thread whose TID is `who`; 0 means the calling thread
- F2: `which = IOPRIO_WHO_PGRP` (2): set every thread in process group `who`; 0 means the caller's group
- F3: `which = IOPRIO_WHO_USER` (3): set every thread whose real UID is `who`; 0 means the caller's real UID
- F4: any other `which`, with a valid `ioprio`: `EINVAL`
- F5: class `IOPRIO_CLASS_NONE` (0): level must be 0, otherwise `EINVAL`. The thread then follows its CPU nice value again.
- F6: class `IOPRIO_CLASS_RT` (1): levels 0-7; needs `CAP_SYS_NICE` or `CAP_SYS_ADMIN`, otherwise `EPERM`. Measured: `EPERM` for an unprivileged caller.
- F7: class `IOPRIO_CLASS_BE` (2): levels 0-7, 0 the highest
- F8: class `IOPRIO_CLASS_IDLE` (3): no level. Measured: succeeds unprivileged, and the caller's `ioprio_get(IOPRIO_WHO_PROCESS, 0)` then returns 24576.
- F9: class 4-7 (7 is `IOPRIO_CLASS_INVALID`): `EINVAL`. This check runs before `which` is examined.
- F10: priority hint in bits 3-12 (`IOPRIO_HINT_DEV_DURATION_LIMIT_1` to `_7`, Linux 6.5 and later), which the kernel validates with the value
- F11: `who` names no thread, group or user: `ESRCH`. Measured for PROCESS with `who = 999999999`.
- F12: the target belongs to another user and the caller lacks privilege: `EPERM`. Measured for PROCESS with `who = 1`.
- F13: inheritance: a child created after the call starts with the parent's priority. Measured: a forked child read 24576 after the parent set IDLE.

## Determinism analysis

`linux_determinism: conditional` (changed from the legacy `nondeterministic`). The return value is a pure function of the arguments, the caller's capabilities, and which threads, groups and users exist when the call runs. It repeats whenever those are the same. Two conditions break that:

- The PGRP and USER forms reach host processes that are not part of the guest, so a guest call can change the I/O priority of unrelated programs, and whether `who` exists (`ESRCH`) depends on the host process table.
- PROCESS with a non-zero `who` names a host TID, so the answer depends on host TID allocation.

The priority itself never changes the data a guest reads or writes, only host I/O timing, which Hermit does not expose to the guest.

### Nondeterminism sources

- Host processes matched by `IOPRIO_WHO_PGRP` or `IOPRIO_WHO_USER`, and whether they exist (`ESRCH` versus 0)
- Host TIDs passed as `who` for `IOPRIO_WHO_PROCESS`
- The caller's capabilities, which decide `EPERM` for the RT class (F6) and for other users' threads (F12)
- Host-inherited state: the priority the process tree had before Hermit started is visible through `ioprio_get`; see that body

## Hermit design

Target policy: `model` (owner decision 7, 2026-10-04); the p3 plan below was `emulate`, and the Target paragraph at the end of this section replaces it. The dispatch arm is at `detcore/src/lib.rs:2686`, and `handle_ioprio_set` (`detcore/src/syscalls/threads.rs:2445`) logs the request and returns `Ok(0)` (`detcore/src/syscalls/threads.rs:2456`) without injecting anything. Its doc comment (`detcore/src/syscalls/threads.rs:2438`) gives the reason: Detcore serializes guest threads onto one virtual CPU, so the block-layer class and level cannot change anything the guest computes. The classification table lists `ioprio_set` as determinized for the same reason (`detcore/src/syscall_classification.rs:588`, entry at `detcore/src/syscall_classification.rs:611`). The change came from https://github.com/rrnewton/hermit/pull/791, the `TODO-HUMAN-REVIEW(#791)` tag at `detcore/src/lib.rs:2685`.

Why the p3 plan chose `emulate`:

- `passthrough` would let a guest change the I/O priority of host processes through F2 and F3, and its `ESRCH` and `EPERM` answers would depend on the host process table.
- `refuse` breaks `ionice`, which is in the strict compatibility corpus.
- `model` would mean Detcore keeps a virtual priority per thread. The p3 plan left that to the owner; decision 7 chose it. The current code keeps none.

Where the emulation differs from Linux today:

- No argument checks. `EINVAL` for F4, F5, F9 and invalid hints (F10) never happens; every call returns 0.
- No privilege check. An RT request (F6) succeeds where an unprivileged Linux caller gets `EPERM`.
- No target lookup. `ESRCH` (F11) and the cross-user `EPERM` (F12) never happen, and `who` is ignored.
- No state. `ioprio_get` keeps returning the fixed defaults of `virtual_ioprio` (`detcore/src/syscalls/threads.rs:391`) after a set, so a guest that sets IDLE and reads it back sees 0 instead of 24576, and F13 inheritance is invisible.

Each difference is deterministic: the answer does not depend on the host. A program that sets a priority and never reads it back cannot tell the difference, which covers `ionice -c 3 cmd`.

Target (owner decision 7, with decision 6): each guest thread carries a virtual I/O priority in Detcore's per-thread state, next to the virtual timer slack (`detcore/src/tool_local.rs:1795`). It starts as the raw value 0 (`IOPRIO_CLASS_NONE`), is copied into every fork or clone child as the timer slack is (`detcore/src/lib.rs:1555`; kernel `copy_io()`), is shared between threads created with `CLONE_IO`, and survives `execve`. `ioprio_set` stores the value in each target's record; `ioprio_get` reads it back (F13). Nothing reaches the host or Detcore's scheduler, so the containment property of T2 is unchanged. The checks follow the kernel's order (ioprio_set(2); kernel `ioprio_set` syscall in `block/ioprio.c`): first `ioprio_check_cap()` on the value, so an undefined or `IOPRIO_CLASS_INVALID` class (F9), `IOPRIO_CLASS_NONE` with a non-zero level (F5) and an undefined hint (F10) give `EINVAL` whatever `which` is; then the `which` switch, where an unknown `which` gives `EINVAL` (F4). `IOPRIO_WHO_PROCESS` resolves `who` with `thread_is_live` (`detcore/src/tool_global.rs:4032`, used the same way at `detcore/src/syscalls/threads.rs:2423`), 0 meaning the caller; `IOPRIO_WHO_PGRP` matches guest processes through Detcore's process-group table (`detcore/src/scheduler.rs:1067`, mirrored by `handle_setpgid` at `detcore/src/syscalls/misc.rs:584`), 0 meaning the caller's group; `IOPRIO_WHO_USER` matches guest threads whose virtual real UID is `who`, 0 meaning the caller's. No match gives `ESRCH` (F11). For the group and user forms each matching thread is changed and the call returns the last failure or 0, as Linux does. Under the guest's launch credentials (owner decision 4: the guest holds full capabilities) and the privilege rule that `handle_sched_setattr` follows (`detcore/src/syscalls/threads.rs:2292`), an RT request (F6) and a request on another user's thread (F12) succeed and are recorded; the guest never sees the unprivileged `EPERM`. Today none of this exists: `handle_ioprio_set` returns 0 for every input (`detcore/src/syscalls/threads.rs:2456`).

## Recommended tests

- T1 (covers F1, F7, F8): under `hermit run --strict --verify` on ptrace, `ionice -c 3 cmd` and `ionice -c 2 -n 7 cmd` must succeed and both runs must match. Add a raw `syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ...)` for BE levels 0 and 7 and for IDLE, each returning 0.
  Existing candidate: the corpus row `ionice` at `tests/e2e/manifests/compat.yaml:183`, which runs `/usr/bin/ionice -c 3 /bin/echo ionice-ok` on the ptrace corpus lane (`tests/e2e/manifests/compat.yaml:17`).
- T2 (covers F2, F3): a guest calls `ioprio_set(IOPRIO_WHO_PGRP, 0, IDLE)` and `ioprio_set(IOPRIO_WHO_USER, 0, IDLE)`. Assert the return values under Hermit, and assert from the host that a process outside the guest owned by the same user still has its old priority. This is the containment property the emulation exists for.
- T3 (covers F4, F5, F9, F10): `which = 0` and `which = 4` with a valid value, `IOPRIO_CLASS_NONE` with level 1, classes 5 and 7, and an undefined hint. Each must give `EINVAL` under Hermit, as on Linux; today Hermit returns 0 for all.
- T4 (covers F6): an RT request at level 0 returns 0 under Hermit, because the guest holds full capabilities (owner decision 4), and `ioprio_get(IOPRIO_WHO_PROCESS, 0)` then returns 8192. The native unprivileged `EPERM` is the host's answer and is not expected.
- T5 (covers F11, F12): PROCESS with a TID that does not exist inside the guest gives `ESRCH`; PGRP with a group ID no guest process has, and USER with a UID no guest thread has, give `ESRCH`. PROCESS with the guest's TID 1 returns 0 and changes that thread's record, since the guest holds full capabilities (owner decision 4). Today Hermit returns 0 for all of them.
- T6 (covers F1, F8, F13): set IDLE, read it back with `ioprio_get(IOPRIO_WHO_PROCESS, 0)`, then fork and read it in the child. Both reads must return 24576, as on Linux; today Hermit returns 0 in both places. Add a thread created with `CLONE_IO` whose change is visible to its creator, and a thread created without it whose change is not.
- T7 (covers F4, F9, F11): check order. Class 5 with `which = 0` gives `EINVAL` (the value check runs first); a valid BE value with `which = 0` gives `EINVAL`; a valid BE value with PROCESS and a TID that does not exist gives `ESRCH`; and class 5 with PROCESS and a TID that does not exist gives `EINVAL`, not `ESRCH`.
  Related but not coverage: `ionice_query_is_deterministic_under_strict_verify` at `hermit-cli/tests/command_strict_verify.rs:528` only queries and is `#[ignore]`d; `rr_ioprio` at `hermit-cli/tests/rr_suite.rs:285` is `#[ignore]`d by its macro (`hermit-cli/tests/rr_suite.rs:233`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/620: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (Detcore returns 0 without touching the host, so a guest cannot change host processes' I/O priority); linux_determinism nondeterministic -> conditional (the answer depends only on arguments, capabilities and the host process table); 13 flag behaviours and 6 tests listed; open question for the owner on argument checks and set-to-get round trip, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7, 6 and 4 applied (per-thread virtual I/O priority read back by ioprio_get, inherited at fork, shared under CLONE_IO; Linux checks in kernel order, value before which, targets from the guest's own tables; RT and cross-user requests accepted under full launch capabilities); target_policy emulate -> model; new T7; removed both open questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `ioprio_set`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ioprio_set&body=Discussion+for+the+%60ioprio_set%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fioprio_set.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ioprio_set%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: ioprio_set" (https://github.com/rrnewton/hermit/issues/620), legacy variant B.

### 2026-10-03 native probe (host kernel 7.1.3, unprivileged user)

Raw `syscall(251, ...)` from Python ctypes. `set(PROCESS, 0, RT level 0)` gave `EPERM`. `set(PROCESS, 0, NONE level 1)` gave `EINVAL`. Class 5 gave `EINVAL`. `which = 4` with a valid value gave `EINVAL`. `set(PROCESS, 999999999, ...)` gave `ESRCH`. `set(PROCESS, 1, BE level 4)` gave `EPERM`. `set(PROCESS, 0, IDLE)` returned 0, after which `ioprio_get(PROCESS, 0)` returned 24576 in the caller and in a child forked afterwards.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
