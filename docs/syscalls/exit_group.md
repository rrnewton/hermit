---
schema: syscall-pages/v1
syscall: exit_group
nr_x86_64: 231
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+exit_group&body=Discussion+for+the+%60exit_group%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fexit_group.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60exit_group%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 27
---

## Summary

`exit_group(status)` terminates every thread in the caller's thread group and never returns; glibc's `exit` and `_exit` both end with it. Detcore handles it (`detcore/src/lib.rs:2530`, `handle_exit_group` at `detcore/src/syscalls/threads.rs:1202`): the thread first waits for a scheduler-granted `ResourceID::Exit { group: true, .. }` turn, so the moment the process leaves the run set is a deterministic point in the schedule, then Detcore stages robust-futex owner-death wakes for the whole group and hands the call to the kernel with `tail_inject`.

Manual reference: Linux man-pages exit_group(2).

## Linux semantics

Only the low 8 bits of `status` reach the parent: `wait` reports `status & 0xff` as the exit code. Any thread may call it, including a thread that is not the group leader. The kernel marks the whole group as exiting and sends `SIGKILL` to every other thread; each thread then runs its own exit path. If a group exit or a fatal signal is already under way when a thread calls `exit_group`, that earlier exit wins and the caller's `status` is ignored.

Each exiting thread releases its resources. Its robust futex list is walked: every lock it still holds gets `FUTEX_OWNER_DIED` set and one waiter woken, so the next locker sees `EOWNERDEAD`. If its `clear_child_tid` address is set and another task still shares the address space, the kernel writes 0 there and does a futex wake. A `vfork` parent waiting on this process is released. When the last thread is gone, the parent receives the exit signal chosen at clone time (normally `SIGCHLD`) and can reap the exit status; children of the process are reparented to the nearest subreaper or to init, and children that asked for `PR_SET_PDEATHSIG` receive that signal. File descriptors close, which can release `flock` locks, wake pipe readers with EOF and send `SIGPIPE`/`EPIPE` to writers.

### Flags and options

- F1: `status`: only `status & 0xff` is reported to the parent (256 reads as 0, -1 as 255).
- F2: caller identity: the leader or any other thread may call it; every thread of the group terminates, including threads blocked in syscalls or spinning in user space.
- F3: concurrent exits: when another thread's `exit_group` or a fatal signal is already in progress, that exit and its status win.
- F4: robust futex lists: each exiting thread's held robust locks are marked `FUTEX_OWNER_DIED` and one waiter per lock is woken, which then gets `EOWNERDEAD`.
- F5: `clear_child_tid`: the kernel clears the word and futex-wakes it when another task still shares the address space.
- F6: parent and children: the parent receives the exit signal and a wait status; orphaned children are reparented, and `PR_SET_PDEATHSIG` children are signalled.
- F7: `vfork`: a `vfork` child that calls `exit_group` (through `_exit`) releases its suspended parent.

## Determinism analysis

`linux_determinism: conditional`, kept from the migration placeholder. For a single-threaded process with no robust locks and a parent that waits synchronously, the outcome is fixed by `status`. Otherwise the host decides several things: how far each sibling thread got before it was killed (and so what it wrote to files, pipes and shared memory), which of two concurrent exits wins, the order in which robust-futex waiters in other processes wake, and when the parent's `SIGCHLD` arrives relative to the parent's own progress. The comment at `detcore/src/scheduler.rs:4534` names that last source as the one behind `make -jN` and redis `--strict --verify` divergences.

### Nondeterminism sources

- How far sibling threads progress before the kernel kills them
- Which exit wins when threads exit, or a fatal signal arrives, concurrently
- When robust-futex and `clear_child_tid` waiters in other tasks wake
- When the parent's exit signal arrives, and the order of reaping when several children exit
- Reparenting order and `PR_SET_PDEATHSIG` delivery when the process has children

## Hermit design

Target policy: `model`. The kernel performs the termination, but Detcore decides when it happens and owns the events that other tasks observe:

- The call waits for a scheduler grant of `ResourceID::Exit { group: true, process, mm }` (`detcore/src/syscalls/threads.rs:1209`). The resource is documented as a go-ahead that is never released (`detcore/src/resources.rs:223`). Because the grant is a scheduler turn, sibling threads stop at a deterministic point of the schedule and two racing exits are ordered by the scheduler (F2, F3).
- When the scheduler grants that resource (`detcore/src/scheduler.rs:4540`), it reserves the exit boundary and, for a group exit of a process whose parent should get a synthesized child-exit signal, inserts a one-shot child-exit event at the next logical nanosecond (`detcore/src/scheduler.rs:4562`) instead of relying on the host-timed kernel `SIGCHLD` (F6).
- Robust futex lists of every thread in the group are read while the address space still exists, and the resulting wakes are held until the backend's exit callback confirms Linux finished the owner-word update (`stage_thread_group_robust_list_wakes`, `detcore/src/syscalls/threads.rs:1145`). This runs only when the backend runs the exit robust list, threads are sequentialized and the futex mode is precise (`detcore/src/syscalls/threads.rs:1150`) (F4).
- The call itself is sent with `tail_inject` (`detcore/src/syscalls/threads.rs:1219`), so the kernel applies the status mask (F1), the `clear_child_tid` wake (F5), `vfork` release (F7), reparenting and descriptor closing.

Why `model` and not the alternatives: forwarding the call with no scheduler turn would let the exit land at a host-chosen moment relative to other threads and would leave the parent's `SIGCHLD` and robust-futex wakes host-timed; `emulate` and `refuse` are not meaningful for process termination.

What remains: F5 and F7 are left to the kernel with no Detcore-side ordering that this reading found, and no test at this SHA names `clear_child_tid` wake-ups caused by `exit_group` or a `vfork` child's `_exit`. Whether a waiter on a `clear_child_tid` word in another process wakes at a deterministic logical time is not established; T4 would show it.

## Recommended tests

- T1 (covers F1, F2): the main thread calls `exit_group` while a sibling spins; repeat with a non-leader thread calling it, and with statuses 0, 1, 256, 257 and -1, checking the parent's `WEXITSTATUS` under `hermit run --verify` on ptrace.
  Existing candidates: `tests/rust/exit_group.rs` (main thread, status 0, sibling spinning), run by `default_cargo_exit_group` (`hermit-cli/tests/hermit_modes.rs:898`) and `record_rs_exit_group` (`hermit-cli/tests/record_replay.rs:2322`), both generated by macros. Non-leader callers and status masking are not tested.
- T2 (covers F3): two threads call `exit_group` with different statuses at the same moment, released by a barrier; separately, one thread calls `exit_group` while another receives a fatal signal. The reported status must be identical across `--verify` runs.
- T3 (covers F4): a thread holds a process-shared robust mutex when the main thread calls `exit_group`; the waiter in another process must get `EOWNERDEAD`, under strict verification.
  Existing candidate: `ptrace_wakes_after_guest_observed_fatal_signal_and_exit_group_cleanup` at `hermit-cli/tests/robust_futex_owner_death.rs:327`, which runs `tests/c/robust_futex_owner_lifecycle.c` mode `exit-group` (line 154) with `--backend=ptrace run --strict --verify --verify-strict` and also checks the stderr line `staged robust-list owner-death wake until physical exit`.
- T4 (covers F5): a task sharing the address space but outside the thread group (`clone(CLONE_VM)` without `CLONE_THREAD`) waits on the `clear_child_tid` word of a thread in the group that calls `exit_group`; the wait must end, and the observed order must be identical across runs.
- T5 (covers F6): a parent with three children that exit through `exit_group` at different points must reap them in the same order and receive the same number of `SIGCHLD` across runs; an orphaned grandchild must see the same `getppid()` after reparenting, and a `PR_SET_PDEATHSIG` child must receive its signal.
- T6 (covers F7): a `vfork` child calls `_exit(3)`; the parent resumes and reaps status 3.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (scheduler-granted Exit turn, synthesized child-exit SIGCHLD, staged robust-list wakes, then tail_inject); linux_determinism stays conditional; 7 flag behaviours and 6 tests listed; no coverage token because no test was run against this list, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T3 covered by hermit-cli/tests/robust_futex_owner_death.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `exit_group`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+exit_group&body=Discussion+for+the+%60exit_group%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fexit_group.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60exit_group%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `exit_group`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
