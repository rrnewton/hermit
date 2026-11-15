---
schema: syscall-pages/v1
syscall: vfork
nr_x86_64: 58
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+vfork&body=Discussion+for+the+%60vfork%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fvfork.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60vfork%60%2C+or+in+what+the+page+says%3F%0A
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

Creates a child process that borrows the caller's memory, and suspends the calling thread until the child calls `execve` or exits. It behaves like `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`. glibc's `vfork()` on x86_64 uses this syscall, and shells and `posix_spawn`-style launchers rely on it. Detcore forwards it as `vfork`, so the kernel enforces the suspension, and stops scheduling other guest threads until the child has registered.

Manual reference: [vfork(2)](https://man7.org/linux/man-pages/man2/vfork.2.html).

## Linux semantics

`vfork()` takes no arguments. The parent gets the child's pid, as seen in the parent's PID namespace, and the child gets 0. The child runs in the parent's address space, on the parent's stack, and may only call `execve` or `_exit`. Anything else, including returning from the calling function, is undefined behaviour.

The calling thread sleeps in the kernel until the child execs successfully or exits; only a fatal signal ends that wait early. Other threads of the parent keep running. The child gets a copy of the descriptor table, so it shares the open file descriptions but not the table itself.

Because `vfork` has no arguments, the items below list behaviours that tests must cover rather than flags.

### Flags and options

- F1: the result: the child's pid to the parent and 0 to the child. The child's parent is the caller, and it sends `SIGCHLD` when it exits.
- F2: the calling thread is suspended until the child's `execve` succeeds or the child exits, normally or by a signal. A failed `execve` does not release it.
- F3: shared memory: the child runs on the parent's address space and stack, so its writes are visible to the parent when it resumes.
- F4: state copied as in `fork`: the descriptor table (sharing the open file descriptions and their `flock` locks), signal dispositions, current directory, umask, credentials and resource limits.
- F5: a multithreaded caller: only the calling thread is suspended. Other threads keep running, on the memory the child is also using.
- F6: signals sent to the suspended parent: a fatal signal ends the wait. Other signals wait until the child releases the parent, and are then delivered.
- F7: failures: `EAGAIN` (`RLIMIT_NPROC` or no free pid) and `ENOMEM`. A signal that arrives during creation makes the kernel restart the call.
- F8: after the child releases the parent, both run concurrently, in an order the host scheduler chooses.

## Determinism analysis

`linux_determinism: conditional`. While the parent thread is suspended, the order between parent and child is fixed by the kernel, so the window in which most of the race happens after `fork` does not exist here. What remains:

- the child's pid, allocated in the caller's PID namespace; it repeats in a fresh namespace when processes are created in the same order
- the interleaving of the parent's other threads with the child (F5)
- the order of parent and exec'd child once the parent is released (F8)
- failures caused by host limits (F7)

The call repeats when the PID namespace is private, the scheduler fixes the order of all guest threads, and the limits are not reached.

### Nondeterminism sources

- Pid allocation, shared with every process in the PID namespace
- The host scheduler: the parent's other threads during the borrow, and parent against child after the release
- When a signal reaches the suspended parent
- Resource limits and host load

## Hermit design

Target policy: `model`. The kernel creates the child and enforces the suspension. Detcore keeps the rest of the guest from running until the child has registered, and gives the child its scheduler turn while the parent waits.

What the code does at f4055eb9:

- The arm at `detcore/src/lib.rs:2438` calls `handle_clone_family` (`detcore/src/syscalls/threads.rs:847`). The comment at `detcore/src/lib.rs:2435` explains why `vfork` is forwarded as `vfork` and not rewritten to `fork`: the kernel then enforces the parent-blocking contract while the child registers and runs to exec or exit.
- `CLONE_VFORK` makes `parent_blocks_for_child` true (`detcore/src/syscalls/threads.rs:870`). The parent stores a `PendingVfork` record (`detcore/src/syscalls/threads.rs:893`) that the child inherits (`detcore/src/lib.rs:1598`). When the child starts, it uses that record to register itself (`create_vfork_child_thread`, `detcore/src/lib.rs:1681`); the parent does not register it.
- Before entering the kernel, the parent gives up its turn with `ResourceID::BlockingVfork` (`detcore/src/syscalls/threads.rs:913`). The scheduler then admits no other guest turn until the child has registered or the call has failed (doc comment at `detcore/src/resources.rs:259`; barrier set at `detcore/src/scheduler.rs:4415`).
- When the kernel releases the parent, it asks to be re-admitted with `ResourceID::BlockedExternalContinue` (`detcore/src/syscalls/threads.rs:945`). If the call failed, it uses `ResourceID::VforkFailed` (`detcore/src/syscalls/threads.rs:935`), which cancels the barrier and returns the original error.
- With chaos scheduling, the child's priority comes from the parent's chaos random stream (`detcore/src/syscalls/threads.rs:882`), and the parent advances that stream afterwards (`detcore/src/syscalls/threads.rs:972`).
- After a successful `vfork`, Detcore forgets every known `flock` mode (`detcore/src/syscalls/threads.rs:963`), because the child shares the open file descriptions (F4).
- The child shares Detcore's memory metadata (`detcore/src/lib.rs:1525`) and gets a copy of the descriptor metadata (`detcore/src/lib.rs:1544`). Its random number streams come from the creation pedigree (`detcore/src/lib.rs:1600`). The unit test `common_child_rng_identity_ignores_backend_tid_for_every_clone_shape` (`detcore/src/lib.rs:3566`) includes a `vfork` shape.

Why `model`: rewriting `vfork` to `fork` would be `emulate`, and the comment at `detcore/src/lib.rs:2435` rejects it, because programs depend on the parent waiting and on the shared memory (F2, F3). `passthrough` would let the scheduler run other guest threads while the child is half-registered. `refuse` would break shells and process launchers.

What remains before the code fully realizes `model`. These are hypotheses read from the code, not measurements:

- **F5 other threads.** Linux lets the parent's other threads run during the borrow. Detcore blocks every other guest turn only until the child registers; after that they are scheduled normally. No test checks a multithreaded parent.
- **F6 signals.** A signal to the parent while it is suspended has not been tested under Hermit.
- **Flock state after vfork.** Forgetting every lock mode on each successful `vfork` makes later blocking `flock` calls on inherited descriptors fail with `ENOLCK`; the flock issue body tracks this. The `vfork` cases in `hermit-cli/tests/flock_exclusion.rs` (for example `dbt_vfork_child_flock_fails_closed_without_deadlock` at `hermit-cli/tests/flock_exclusion.rs:648`) run on DBT, not ptrace.

`linux_determinism` stays `conditional`: Hermit removes the scheduling and pid allocation sources, but host resource limits (F7) remain.

## Recommended tests

- T1 (covers F1, F2, F8): `vfork()` followed by `execve` of a program that prints its pid. The parent reaps the child and checks `SIGCHLD`. The output must match across `hermit run --verify` runs on ptrace.
  Existing candidates: `tests/c/vforkExec.c:23` (cell `c-programs/vforkexec`, program at `tests/e2e/manifests/c-programs.yaml:10676`, verify ci on ptrace), and `vfork_parent_resumes_after_child_exec` at `detcore/tests/misc/vfork.rs:48`, in the `tests_misc` target.
- T2 (covers F2, F3): the `vfork` child writes a value to a parent stack variable and calls `_exit`. The parent must see the value, and must not have run before the child exited. Also check that a failed `execve` does not release the parent until `_exit`.
  Related: `clone_vfork_parent_waits_for_child_exit` at `detcore/tests/misc/vfork.rs:20` checks the same property for `clone(CLONE_VM|CLONE_VFORK)`.
- T3 (covers F4): the child changes an inherited descriptor's offset and closes another one before `execve`. The parent must see the new offset and must still have the closed descriptor.
  Related: `start_vfork_form` at `tests/c/flock_exclusion.c:1248` calls `vfork()`, `clone(CLONE_VFORK)` and `clone3(CLONE_VFORK)`.
- T4 (covers F5): a parent with a second thread that writes a counter in a loop calls `vfork`; the child execs. The run must finish and its output must match across `--verify` runs.
- T5 (covers F6): send `SIGUSR1` to the parent while the child sleeps before `_exit`. The handler must run only after the parent resumes. Send `SIGKILL` in a second run; the parent must die and the child must finish.
- T6 (covers F7): with `RLIMIT_NPROC` lowered to the current process count, `vfork` must give `EAGAIN` and the parent must continue: the `VforkFailed` path must not leave the scheduler waiting.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (forwarded as vfork so the kernel suspends the parent; Detcore's BlockingVfork barrier holds other turns until the child registers, detcore/src/syscalls/threads.rs:913); linux_determinism conditional; 8 behaviour items, 6 tests; multithreaded parent and signals during the borrow untested; no coverage recorded, so todo.

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `vfork`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+vfork&body=Discussion+for+the+%60vfork%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fvfork.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60vfork%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `vfork`: the 2026-07-24 classification campaign filed issues only for syscalls that had no Detcore handler, and Detcore already handled this one.
