---
schema: syscall-pages/v1
syscall: fork
nr_x86_64: 57
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fork&body=Discussion+for+the+%60fork%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffork.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fork%60%2C+or+in+what+the+page+says%3F%0A
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

Creates a child process that is a copy of the caller. It behaves like `clone` with only `SIGCHLD` as the exit signal. On x86_64, glibc's `fork()` uses `clone`, so this syscall is reached only through a raw `syscall(SYS_fork)` or an older or static runtime. Detcore handles it in the same code as `clone`.

Manual reference: [fork(2)](https://man7.org/linux/man-pages/man2/fork.2.html).

## Linux semantics

`fork()` takes no arguments. The parent gets the child's pid, as seen in the parent's PID namespace, and the child gets 0. The child has one thread, a copy of the calling thread, and resumes at the instruction after the syscall. When it exits, the parent receives `SIGCHLD`.

The child gets a copy-on-write copy of the address space and a copy of the descriptor table; the copied descriptors refer to the same open file descriptions as the parent's. Some state is not inherited. Which of parent and child runs first is up to the host scheduler.

Because `fork` has no arguments, the items below list behaviours that tests must cover rather than flags.

### Flags and options

- F1: the result: the child's pid to the parent and 0 to the child. The child's parent is the caller, and it sends `SIGCHLD` when it exits.
- F2: what the child copies: the address space (copy-on-write), the descriptor table (sharing the open file descriptions, including their offsets and `flock` locks), signal dispositions and mask, current directory, umask, resource limits and credentials.
- F3: what the child does not inherit: pending signals, POSIX timers, `fcntl` record locks, memory locks, timer slack set by `prctl`, and resource usage counters (which restart at 0).
- F4: memory mappings that do not follow the copy rule: `MAP_SHARED` mappings stay shared, `MADV_DONTFORK` ranges are absent in the child, and `MADV_WIPEONFORK` ranges read as zero in the child.
- F5: a multithreaded caller: only the calling thread is copied. Locks held by other threads stay held in the child.
- F6: failures: `EAGAIN` (`RLIMIT_NPROC` or no free pid) and `ENOMEM`. A signal that arrives during creation makes the kernel restart the call.
- F7: after the call, parent and child run concurrently, in an order the host scheduler chooses.

## Determinism analysis

`linux_determinism: conditional`, for the same reasons as `clone`. The child's pid is allocated in the caller's PID namespace. In a fresh namespace it repeats when processes are created in the same order, but in the host namespace it depends on every other process on the machine. The post-fork order of parent and child (F7) is the host scheduler's choice. Failures (F6) depend on host limits. The call repeats when the PID namespace is private, the creation order is fixed by a deterministic scheduler, and the limits are not reached.

### Nondeterminism sources

- Pid allocation, shared with every process in the PID namespace
- Which of parent and child the host scheduler runs first, and how they interleave
- Resource limits and host load: `RLIMIT_NPROC`, `pid_max`, memory

## Hermit design

Target policy: `model`, in the same handler as `clone`. The kernel creates the child. Detcore registers it, gives it a copy of the parent's deterministic state, and decides when parent and child run.

What the code does at f4055eb9:

- The arm at `detcore/src/lib.rs:2433` calls `handle_clone_family` (`detcore/src/syscalls/threads.rs:847`). For `fork` the exit signal is `SIGCHLD` (`detcore/src/syscalls/threads.rs:855`), and none of the `CLONE_*` sharing flags are set.
- The syscall is injected unchanged (`detcore/src/syscalls/threads.rs:924`). On ptrace the parent does not wait for the child: `backend_serializes_fork_children` is false (`hermit-cli/src/lib.rs:4252`), so `parent_blocks_for_child` is false for `fork` (`detcore/src/syscalls/threads.rs:870`).
- After a successful fork, Detcore forgets every known `flock` mode (`detcore/src/syscalls/threads.rs:963`), because the child shares the open file descriptions (F2). A failed fork leaves them alone.
- The kernel's pid is used directly as the guest-visible id (`detcore/src/syscalls/threads.rs:983`). It repeats because the guest has its own PID namespace (`default_container`, `hermit-cli/src/bin/hermit/container.rs:442`) and the scheduler fixes the creation order.
- The child's Detcore state matches F2 and F3 (`init_thread_state`, `detcore/src/lib.rs:1490`):
  - memory metadata is copied (`detcore/src/lib.rs:1525`)
  - descriptor metadata is copied for the child (`detcore/src/lib.rs:1544`)
  - POSIX timers start empty (`detcore/src/lib.rs:1561`)
  - resource limits are a snapshot (`detcore/src/lib.rs:1568`)
  - process CPU time starts at 0 (`detcore/src/lib.rs:1579`)
  - random number streams come from the creation pedigree (`detcore/src/lib.rs:1600`)
- Unit test: `common_child_rng_identity_ignores_backend_tid_for_every_clone_shape` (`detcore/src/lib.rs:3566`) includes a `fork` shape.

Why `model`: the kernel has to create the process. `passthrough` would leave the post-fork race (F7) to the host, and `refuse` would break programs that call it.

What remains before the code fully realizes `model`. These are hypotheses read from the code, not measurements:

- **No test reaches this syscall.** No test under `tests/`, `hermit-cli/tests` or `detcore/tests` calls `SYS_fork`. Every program that calls glibc `fork()` goes through `clone`, so the `clone` body's tests do not cover this arm.
- **F4 and F5** are left to the kernel. Detcore copies its memory metadata as a whole, and no code looks at `MADV_DONTFORK` or `MADV_WIPEONFORK` ranges for the child. Whether this matters for Detcore's own bookkeeping has not been tested.
- **Flock state after fork.** Forgetting every lock mode on each successful fork makes later blocking `flock` calls on inherited descriptors fail with `ENOLCK`; the flock issue body tracks this.

`linux_determinism` stays `conditional`: Hermit removes the scheduling and pid allocation sources, but host resource limits (F6) remain.

## Recommended tests

- T1 (covers F1, F7): with `syscall(SYS_fork)`, parent and child each write their `getpid`, `getppid` and the fork result to a shared pipe; the parent reaps the child and checks `SIGCHLD`. The output, including its order, must match across `hermit run --verify` runs on ptrace.
- T2 (covers F2, F4): after `SYS_fork`, the child writes to a private page, a `MAP_SHARED` page, a `MADV_WIPEONFORK` page and a `MADV_DONTFORK` range, and moves the offset of an inherited descriptor. Parent and child check what they see.
- T3 (covers F2, F3): a POSIX timer created before the fork is absent in the child (`timer_gettime` gives `EINVAL`), a pending signal is not inherited, an `fcntl` record lock is not inherited, and a `flock` lock is shared. Record what a blocking `flock` in the child returns under Hermit.
- T4 (covers F5): a process with two threads calls `SYS_fork`. The child must have one thread (`/proc/self/task`) and must run to exit under Hermit.
- T5 (covers F6): with `RLIMIT_NPROC` lowered to the current process count, `SYS_fork` must give `EAGAIN`, and a `flock` lock held before the failed call must still be known to Detcore afterwards.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (shared clone-family handler detcore/src/syscalls/threads.rs:847: kernel forks, Detcore copies child state and orders parent and child); linux_determinism conditional; 7 behaviour items, 5 tests; no test calls SYS_fork at this SHA (glibc fork uses clone); no coverage recorded, so todo.

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fork`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fork&body=Discussion+for+the+%60fork%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffork.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fork%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `fork`: the 2026-07-24 classification campaign filed issues only for syscalls that had no Detcore handler, and Detcore already handled this one.
