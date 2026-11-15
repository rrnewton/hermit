---
schema: syscall-pages/v1
syscall: exit
nr_x86_64: 60
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+exit&body=Discussion+for+the+%60exit%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fexit.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60exit%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 53
---

## Summary

`exit(2)` terminates the calling thread only (glibc's `pthread_exit` and a thread function's return use it; `_exit(3)` uses `exit_group`). Hermit models the exit as a scheduler-ordered boundary, replays the robust-list and `CLONE_CHILD_CLEARTID` wakes against its own futex model, and then lets the kernel perform the exit.

## Linux semantics

The call never returns. The kernel tears down the calling task: it releases the robust futex list, clears and wakes the `clear_child_tid` word, releases a `vfork` parent, and drops the thread from its group. Only when the last thread of a thread group exits does the process terminate: it becomes a zombie with exit status `(status & 0xff) << 8`, its children are reparented to the nearest subreaper or init, and the parent receives the child's exit signal (normally `SIGCHLD`) and becomes able to reap it with `wait4`/`waitid`. If the thread-group leader exits while other threads still run, the leader stays a zombie and the parent cannot reap the process until the last thread exits.

### Flags and options

- F1: `status`: only the low 8 bits are kept; a waiter reads them through `WEXITSTATUS`.
- F2: the caller is not the last thread of its group: only that thread ends; the process, its other threads and its parent see no exit status or `SIGCHLD`.
- F3: the caller is the last thread of its group: the process becomes a zombie, its children are reparented, the parent gets the exit signal and wait readiness.
- F4: the thread-group leader exits first: the leader is held as a zombie until the last thread exits, and only then is the process reapable.
- F5: `CLONE_CHILD_CLEARTID` / `set_tid_address`: the kernel writes 0 to the registered word and wakes one futex waiter on it (`pthread_join`).
- F6: robust futex list: for every robust mutex the thread still holds, the kernel sets `FUTEX_OWNER_DIED` and wakes one waiter, which then gets `EOWNERDEAD`.
- F7: a `vfork` child that exits releases its parent, which resumes.

## Determinism analysis

For the exiting thread the call is deterministic: it never returns, and the status a waiter later reads is the low byte of the argument. What other tasks observe depends on host scheduling. When the parent's `SIGCHLD` arrives relative to its own work, the order in which several exiting threads or children become visible to `wait4`, `pthread_join` or robust-mutex waiters, and which waiter a single futex wake reaches are all decided by host timing on Linux. A program whose output depends on which thread or child finished first therefore differs from run to run. `linux_determinism` is `conditional`: deterministic for the caller, scheduling-dependent for every observer.

## Hermit design

`target_policy` is `model`: Detcore fixes the moment of the exit in its schedule and replays the exit's wake-ups in its own futex model. The kernel still frees the thread. Code at hermit f4055eb9cae4:

- The dispatch arm is `detcore/src/lib.rs:2531`, which calls `handle_exit` at `detcore/src/syscalls/threads.rs:1181-1199`.
- `threads.rs:1187-1195` requests `ResourceID::Exit { group: false, process, mm }`. The scheduler grants it at `detcore/src/scheduler.rs:4540-4565` after `reserve_exit_boundary` (`detcore/src/scheduler/signal_control.rs:801`). That grant is the deterministic moment the thread leaves the run set.
- `threads.rs:1196` calls `run_robust_list_owner_death` (`threads.rs:1095-1139`). It walks the guest's robust list and wakes the modeled waiters (F6). It runs only with thread sequentialization and the precise futex model (`threads.rs:1096-1101`). Ptrace leaves the atomic `FUTEX_OWNER_DIED` store to Linux (doc comment at `threads.rs:1078-1091`). In the polling and external futex modes, waiters park in the host kernel, and the kernel's own robust-list walk wakes them.
- `threads.rs:1198` hands the call to the kernel with `tail_inject` and skips the posthook. F1, F3's reparenting and F7 are therefore the kernel's.
- F5: when the scheduler removes the thread, `logically_kill_thread` (`detcore/src/scheduler.rs:2289`) wakes one modeled waiter on the `clear_child_tid` word at `scheduler.rs:2330-2334`, through `wake_futex_child_cleartid` (`scheduler.rs:2895-2905`).
- F3 `SIGCHLD`: the scheduler synthesizes a one-shot child-exit `SIGCHLD` at a fixed logical time, but only for `group: true` grants (`scheduler.rs:4551-4563`), which come from `exit_group` (`threads.rs:1202-1220`). When the last thread leaves through plain `exit`, the parent receives the kernel's host-timed `SIGCHLD`. The scheduler then defers it until no other guest work is runnable (`scheduler.rs:4577-4599`). That ordering is deterministic only as long as the deferral rule holds; this is a reading of the code, not a measured result.

## Recommended tests

- T1 (covers F1, F3): a process with one thread calls raw `SYS_exit` with a status above 255; the parent's `wait4` must read the low byte, and two runs under `hermit run --verify` on ptrace must agree.
  Existing candidate: `tests/c/simple/hello_nostdlib.c` (cell at `tests/e2e/manifests/c-programs.yaml:2173`, verify ci on ptrace) ends with raw `SYS_exit`. It does not check the status a parent reads.
- T2 (covers F2, F3): `fork`, then both parent and child leave by raw `SYS_exit` after writing; output order must be the same in every run.
  Existing candidate: `tests/c/simple/racewrite_nostdlib.c` (cell at `tests/e2e/manifests/c-programs.yaml:6714`, verify ci on ptrace).
- T3 (covers F2, F5): several threads leave by `pthread_exit` in a racy order while the main thread joins them; join order and output must match across runs, and no join may hang.
- T4 (covers F4): the main thread calls `pthread_exit` while a second thread keeps running and then exits with a known status; the parent's `wait4` must not return before the second thread exits and must read that status.
- T5 (covers F6): a thread exits while holding a `PTHREAD_MUTEX_ROBUST` mutex with a waiter parked; the waiter must get `EOWNERDEAD`, with identical runs.
  Existing candidates: `robust_futex_owner_death_wakes_the_waiter_at_l2` at `hermit-cli/tests/robust_futex_owner_death.rs:196` and `detcore_wakes_the_modeled_waiter_before_linux_finishes_exit` at `hermit-cli/tests/robust_futex_owner_death.rs:250`, both with `--backend=ptrace`.
- T6 (covers F3): the last thread of a child leaves by plain `exit` while the parent has a `SIGCHLD` handler and other runnable threads; the handler's position in the parent's output must be the same in every run.
- T7 (covers F7): a `vfork` child calls raw `SYS_exit` without `exec`; the parent must resume and reap it with the child's status.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (scheduler-ordered Exit grant, modeled robust-list and CLEARTID wakes, kernel performs the exit); linux_determinism stays conditional; 7 flag behaviours and 7 tests listed; open question on the kernel SIGCHLD when a process ends through plain exit; no coverage token yet because no test has been run against this list, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/simple/racewrite_nostdlib.c, T5 covered by hermit-cli/tests/robust_futex_owner_death.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `exit`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+exit&body=Discussion+for+the+%60exit%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fexit.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60exit%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `exit`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- When a process's last thread leaves through `exit` (not `exit_group`), should the scheduler also synthesize the deterministic child-exit `SIGCHLD`? Today `detcore/src/scheduler.rs:4551-4563` does so only for `group: true`, so this case relies on the deferral of the kernel's `SIGCHLD` (`scheduler.rs:4577-4599`). T6 should show whether that is enough. Single-threaded programs rarely exit this way, but a `pthread_exit` from the last thread does.
- No test pins the robust-list behaviour of `exit` in the polling futex mode, where Detcore skips its own walk (`detcore/src/syscalls/threads.rs:1096-1101`) and relies on the host kernel's. The same guard applies to `exit_group`'s staged walk (`threads.rs:1150-1155`).
