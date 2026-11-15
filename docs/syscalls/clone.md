---
schema: syscall-pages/v1
syscall: clone
nr_x86_64: 56
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clone&body=Discussion+for+the+%60clone%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclone.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clone%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 31
---

## Summary

Creates a new thread or process. The flags choose what the child shares with its creator: memory, descriptors, signal handlers, namespaces. On x86_64, glibc's `fork()` is implemented with this syscall, not with `fork` (nr 57). The kernel creates the child, and Detcore registers it, decides when parent and child run, and sets up the child's deterministic state.

Manual reference: [clone(2)](https://man7.org/linux/man-pages/man2/clone.2.html).

## Linux semantics

On x86_64 the raw argument order is `clone(flags, stack, parent_tid, child_tid, tls)`. Only the low 32 bits of `flags` are used. The low byte is the exit signal (`CSIGNAL`) and the rest are `CLONE_*` bits; flags above bit 31, such as `CLONE_CLEAR_SIGHAND` and `CLONE_INTO_CGROUP`, exist only in `clone3`.

On success the parent gets the child's thread id, as seen in the parent's PID namespace, and the child gets 0. The child resumes at the instruction after the syscall, on `stack` if one was given and otherwise on a copy of the parent's stack. Thread ids come from the PID namespace's allocator, which hands out the next free number.

Which of parent and child runs first after the call is up to the host scheduler. The call itself can fail because of resource limits (`RLIMIT_NPROC`, `pid_max`, memory, namespace counts) or because of capabilities, and these depend on the host.

### Flags and options

- F1: exit signal (low byte): the signal sent to the parent when the child exits. 0 sends none, and the parent must then wait with `__WALL`. It is ignored with `CLONE_THREAD`.
- F2: `CLONE_VM`: share the address space. Without it the child gets a copy-on-write copy.
- F3: `CLONE_FS`: share the root, current directory and umask.
- F4: `CLONE_FILES`: share the descriptor table. Without it the child gets a copy that refers to the same open file descriptions.
- F5: `CLONE_SIGHAND`: share signal handlers. Requires `CLONE_VM`.
- F6: `CLONE_THREAD`: put the child in the caller's thread group. Requires `CLONE_SIGHAND`. The child cannot be waited for, and no exit signal is sent.
- F7: `CLONE_VFORK`: suspend the calling thread until the child calls `execve` or exits.
- F8: `CLONE_PARENT`: make the child a sibling of the caller: its parent, and the receiver of its exit signal, is the caller's parent.
- F9: `CLONE_SETTLS`: set the child's thread-local storage base (`fs` on x86_64) from `tls`.
- F10: `CLONE_PARENT_SETTID`: store the child's thread id at `parent_tid` before the call returns.
- F11: `CLONE_CHILD_SETTID` / `CLONE_CHILD_CLEARTID`: store the child's thread id at `child_tid` in the child's memory; at the child's exit, write 0 there and do a futex wake on it.
- F12: `CLONE_SYSVSEM`: share the System V semaphore undo list.
- F13: `CLONE_IO`: share the I/O context used by the I/O scheduler.
- F14: `CLONE_PIDFD`: return a pidfd for the child through `parent_tid`. On `clone` it cannot be combined with `CLONE_PARENT_SETTID`, and `CLONE_DETACHED` with it gives `EINVAL`; `CLONE_DETACHED` alone is ignored.
- F15: `CLONE_PTRACE` / `CLONE_UNTRACED`: `CLONE_PTRACE` makes a traced caller's child traced as well. `CLONE_UNTRACED` stops a tracer from attaching to the child automatically. Hermit's target clears `CLONE_UNTRACED` before the call (F22).
- F16: namespace flags: `CLONE_NEWNS`, `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWUSER`, `CLONE_NEWPID`, `CLONE_NEWNET`, `CLONE_NEWCGROUP`. All but `CLONE_NEWUSER` need `CAP_SYS_ADMIN` in the caller's user namespace. A child in a new PID namespace sees itself as pid 1. Hermit's target refuses every one of them with `EPERM` (F21).
- F17: `stack`: the child's stack pointer. 0 means the child continues on its copy of the caller's stack, which is only safe without `CLONE_VM`.
- F18: invalid combinations give `EINVAL`: `CLONE_SIGHAND` without `CLONE_VM`, `CLONE_THREAD` without `CLONE_SIGHAND`, `CLONE_NEWNS` with `CLONE_FS`, `CLONE_NEWUSER` or `CLONE_NEWPID` with `CLONE_THREAD`, and `CLONE_PIDFD` with `CLONE_PARENT_SETTID`.
- F19: resource and permission failures: `EAGAIN` (`RLIMIT_NPROC` or no free pid), `ENOMEM`, `ENOSPC` (a namespace limit), `EPERM` (a namespace flag without the capability). A signal that arrives during creation makes the kernel restart the call.
- F20: the result: the child's thread id to the parent and 0 to the child. Afterwards both run concurrently, in an order the host scheduler chooses.
- F21: namespace flags refused (owner decision 15): a call with any `CLONE_NEW*` bit returns `EPERM` and creates no child. The `EINVAL` combinations of F18 that Linux checks first still give `EINVAL`.
- F22: `CLONE_UNTRACED` stripped (owner decision 15): the call behaves as if the flag were absent. The child is traced and scheduled like any other child, and the flag has no other visible effect.

## Determinism analysis

`linux_determinism: conditional`. The values `clone` produces are the child's thread id and the copies or shares the flags ask for. The thread id is allocated in the caller's PID namespace. In a fresh namespace it repeats when every process and thread is created in the same order, but in the host namespace it depends on every other process on the machine.

After the call, the order in which parent and child run is the host scheduler's choice. That is the main source of nondeterminism in programs that fork. Failures from resource limits (F19) depend on host limits and load. The call repeats when the PID namespace is private, the creation order is fixed by a deterministic scheduler, and the limits are not reached.

### Nondeterminism sources

- Thread id allocation, shared with every process in the PID namespace
- Which of parent and child the host scheduler runs first, and how they interleave
- Resource limits and host load: `RLIMIT_NPROC`, `pid_max`, memory, namespace counts
- Capabilities, for namespace flags

## Hermit design

Target policy: `model`. The kernel creates the child with the guest's own flags. Detcore registers the child, models the parts of process creation that affect determinism, and decides when parent and child run.

What the code does at f4055eb9:

- The arm at `detcore/src/lib.rs:2431` calls `handle_clone_family` (`detcore/src/syscalls/threads.rs:847`), which also handles `clone3`, `fork` and `vfork`.
- It reads the flags (`detcore/src/syscalls/threads.rs:852`) and takes the exit signal from the low byte (`detcore/src/syscalls/threads.rs:858`). It keeps the `child_tid` address only if `CLONE_CHILD_CLEARTID` is set (`child_tid_clear_address`, `detcore/src/tool_global.rs:3412`), so the exit-time wake is registered and a plain `CLONE_CHILD_SETTID` is left to the kernel.
- A `CLONE_VFORK` caller gives up its scheduler turn before the call, through `ResourceID::BlockingVfork` (`detcore/src/syscalls/threads.rs:910`), and is re-admitted when the child execs or exits. A failed call cancels that wait through `ResourceID::VforkFailed` (`detcore/src/syscalls/threads.rs:935`).
- The original syscall is injected unchanged (`detcore/src/syscalls/threads.rs:924`), so the kernel applies every flag. The return value is not recorded; it comes from the kernel in every mode.
- After a successful clone without `CLONE_THREAD`, Detcore forgets every known `flock` mode (`detcore/src/syscalls/threads.rs:963`), because another process can now change the inherited open file descriptions.
- The kernel's thread id is used directly as the guest-visible id (`detcore/src/syscalls/threads.rs:983`, marked `TODO(T78538674), virtualized tid/pid`). This repeats because the guest runs in its own PID namespace (`default_container`, `hermit-cli/src/bin/hermit/container.rs:442`) and the scheduler fixes the creation order.
- The child is registered in the thread tree with its exit signal and its `CLONE_PARENT` setting (`detcore/src/tool_global.rs:2073`). Its Detcore state is built in `init_thread_state` (`detcore/src/lib.rs:1490`):
  - logical time is cloned (`detcore/src/lib.rs:1505`)
  - the memory map id and memory metadata are shared under `CLONE_VM` and copied otherwise (`detcore/src/lib.rs:1520`)
  - descriptor metadata is shared under `CLONE_FILES` and copied otherwise (`detcore/src/lib.rs:1544`)
  - POSIX timers, resource limits and process CPU time are shared under `CLONE_THREAD` and fresh or copied otherwise (`detcore/src/lib.rs:1561`)
  - the child's random number streams are derived from its creation pedigree, never from the host thread id (`detcore/src/lib.rs:1600`)
- Unit tests: `common_child_rng_identity_ignores_backend_tid_for_every_clone_shape` (`detcore/src/lib.rs:3566`), `serialized_siblings_receive_distinct_child_rng_streams` (`detcore/src/lib.rs:3605`) and `cloned_threads_and_processes_keep_absolute_time_without_inheriting_work` (`detcore/src/lib.rs:3675`).

Why `model`: the kernel has to create the process, so `emulate` is not possible. `passthrough` would leave the post-clone race (F20) to the host. `refuse` would break every program that starts a thread or a process.

What remains before the code fully realizes `model`. These are hypotheses read from the code, not measurements:

- **Kernel-owned flags.** Detcore does not look at `CLONE_SETTLS`, `CLONE_PARENT_SETTID`, `CLONE_SYSVSEM`, `CLONE_IO`, `CLONE_PIDFD`, `CLONE_PTRACE`, `CLONE_UNTRACED` or the namespace flags (F9, F10, F12 to F16); a search of `detcore/src` finds no code for them. The kernel applies them faithfully, but Detcore's model does not follow their effects.
- **F14 pidfds.** The pidfd is created by the kernel inside `clone`, so Detcore's descriptor table may not contain it. Later calls on it could then go down an untracked-descriptor path.
- **F16 new PID namespace.** Detcore keys the child by the id returned to the parent, while the child sees itself as pid 1. Whether Detcore stays consistent when that child calls `getpid` or `gettid` has not been tested.
- **F15 `CLONE_UNTRACED`.** Owner decision 15 strips it; see the Target paragraph below.
- **Flock state after fork.** Forgetting every lock mode on each successful process clone makes later blocking `flock` calls on inherited descriptors fail with `ENOLCK`; the flock issue body tracks this.

Target (owner decisions 15 and 6): filter the flags before the injection at `detcore/src/syscalls/threads.rs:924`, which today passes the guest's flags to the kernel unchanged. No code in `detcore/src` names any `CLONE_NEW*` bit or `CLONE_UNTRACED`. In Reverie `d766df20`, only the trap-only modes read `CLONE_UNTRACED` (`reverie-ptrace/src/task_trap_only.rs:1439`).

- Any `CLONE_NEW*` bit (`CLONE_NEWNS`, `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWUSER`, `CLONE_NEWPID`, `CLONE_NEWNET`, `CLONE_NEWCGROUP`) gives `EPERM` without injecting the call (F21, https://github.com/rrnewton/hermit/issues/3676). This closes the side door around the `unshare` and `setns` refusals (`detcore/src/lib.rs:2215-2219`). `EPERM` is what Linux gives a caller without `CAP_SYS_ADMIN` for every kind but the user namespace, and it is what a host that forbids user namespaces gives for that one. The Nix daemon is one program that falls back: with its default `sandbox-fallback = true` it builds without its namespace sandbox. A `CLONE_VFORK` caller must get the refusal before it gives up its scheduler turn (`threads.rs:907-921`).
- Order (owner decision 6, because `clone` is modelled): `kernel_clone` (`kernel/fork.c`) first rejects `CLONE_PIDFD` with `CLONE_PARENT_SETTID`. `copy_process` then checks the flag combinations of F18, which include the namespace ones (`CLONE_NEWNS` or `CLONE_NEWUSER` with `CLONE_FS`, `CLONE_NEWUSER` or `CLONE_NEWPID` with `CLONE_THREAD`). Only after those does `copy_namespaces` (`kernel/nsproxy.c`) return `EPERM` for a missing capability. The target gives those `EINVAL` answers first and `EPERM` after. Linux checks the `RLIMIT_NPROC` `EAGAIN` between the two, and the target does not reproduce it ahead of the refusal; resource limits belong to owner decision 5.
- `CLONE_UNTRACED` is cleared from the flags register before injection, so the tracer attaches to the child automatically, as for every other child (F22).
- `CLONE_CLEAR_SIGHAND`, `CLONE_INTO_CGROUP` and `set_tid` exist only in `clone3` (bits above 31). The `clone3` body covers them.

`linux_determinism` stays `conditional`: Hermit removes the scheduling and pid allocation sources, but host resource limits (F19) remain.

## Recommended tests

- T1 (covers F2, F17, F20): `clone(CLONE_VM|SIGCHLD)` with a separate stack. The child writes to shared memory, and the parent waits and checks it, under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/clone.c:31` (cell `c-programs/clone`, program at `tests/e2e/manifests/c-programs.yaml:594`, verify ci on ptrace).
- T2 (covers F3, F4, F12, F13): raw `SYS_clone` with and without `CLONE_FS|CLONE_FILES|CLONE_SYSVSEM|CLONE_IO`. The child changes its directory and umask, opens and closes a descriptor, and adjusts a semaphore with `SEM_UNDO`. The parent checks which changes it sees.
  Related: `tests/c/flock_exclusion.c:1186` uses `CLONE_FILES|SIGCHLD`, but the only harness test for that form at this SHA, `dbt_process_clone_files_is_refused_before_copied_child_mutation` (`hermit-cli/tests/flock_exclusion.rs:718`), runs on DBT.
- T3 (covers F5, F6, F9, F10, F11): create a thread with raw `SYS_clone` and the flags glibc uses for threads: `CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM|CLONE_SETTLS|CLONE_PARENT_SETTID|CLONE_CHILD_CLEARTID`. Check that `parent_tid` holds the id, the TLS base is set, and `child_tid` is cleared and woken at exit. Raw `SYS_clone` is needed because glibc's `pthread_create` may use `clone3`.
- T4 (covers F7): `SYS_clone` with `CLONE_VM|CLONE_VFORK|SIGCHLD`: the parent must not run until the child has exited or exec'd.
  Existing candidate: `clone_vfork_parent_waits_for_child_exit` at `detcore/tests/misc/vfork.rs:20`, in the `tests_misc` target. Related: `start_vfork_form` at `tests/c/flock_exclusion.c:1248`.
- T5 (covers F1, F8): `clone(CLONE_PARENT|SIGCHLD)`: the caller's parent can reap the sibling. Also exit signal 0 (reaped only with `__WALL`) and `SIGUSR1` (delivered to the parent).
  Existing candidate: `raw_clone(CLONE_PARENT | SIGCHLD)` at `tests/c/terminal_child_selectors.c:216`, run on ptrace by `ptrace_terminal_child_selectors_follow_linux_contract` (`hermit-cli/tests/terminal_child_selectors.rs:68`).
- T6 (covers F14): `CLONE_PIDFD`: poll the pidfd until the child exits, signal through `pidfd_send_signal`, reap with `waitid(P_PIDFD)`. `CLONE_PIDFD|CLONE_PARENT_SETTID` must give `EINVAL`.
- T7 (covers F15, F22): a `CLONE_UNTRACED|SIGCHLD` child reads `getrandom` and `clock_gettime` and prints them. Under `--verify` the output must match, the values must be Hermit's virtual ones (the same as from a child without the flag), and the child must stay under Hermit's scheduler. Also check `CLONE_PTRACE`.
- T8 (covers F16, F21): raw `SYS_clone` with `SIGCHLD` and each `CLONE_NEW*` flag alone, and with `CLONE_NEWUSER|CLONE_NEWPID`. Each must return -1 with `EPERM`, `wait` must then give `ECHILD`, and `readlink` of every `/proc/self/ns/*` entry must be unchanged, under `hermit run --verify` on ptrace and under `hermit record` and replay. Repeat once with `CLONE_VFORK|CLONE_VM|CLONE_NEWNS` to check that the parent is not left waiting.
- T9 (covers F18, F19): `SYS_clone` with `CLONE_SIGHAND` alone, with `CLONE_THREAD` without `CLONE_SIGHAND`, and with `CLONE_NEWNS|CLONE_FS` must give `EINVAL`. With `RLIMIT_NPROC` lowered, the call must give `EAGAIN`. A failed call must leave Detcore's state unchanged.
  Existing candidate: `syscall(SYS_clone, CLONE_SIGHAND, ...)` at `tests/c/flock_exclusion.c:615`, run on ptrace by `failed_process_clone_preserves_known_flock_state` (`hermit-cli/tests/flock_exclusion.rs:480`).
- T10 (covers F11, F20): a process tree built with glibc `fork()`, which calls `clone` with `CLONE_CHILD_SETTID|CLONE_CHILD_CLEARTID|SIGCHLD`. Parent and children interleave output, and it must match across `--verify` runs.
  Existing candidate: `tests/e2e/determinism-stress/fork_tree.c:68` (program at `tests/e2e/manifests/determinism-stress-c.yaml:14`, verify ci on ptrace).
- T11 (covers F18, F21): the check order. `CLONE_NEWNS|CLONE_FS|SIGCHLD` and `CLONE_NEWPID|CLONE_THREAD|CLONE_SIGHAND|CLONE_VM` must give `EINVAL`, as on Linux, while `CLONE_NEWPID|SIGCHLD` gives `EPERM`; no child is created in any case.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel creates the child with the guest's flags; Detcore registers it, blocks CLONE_VFORK parents, derives child state and RNG from the pedigree, detcore/src/syscalls/threads.rs:847); linux_determinism conditional; 20 flags, 10 tests; open question: can a CLONE_UNTRACED child escape supervision (T7); no coverage recorded, so todo.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 15 and 6 applied (CLONE_NEW* gets EPERM after Linux's copy_process EINVAL checks, CLONE_UNTRACED stripped, both missing at threads.rs:924); target_policy stays model; F15 and F16 narrowed; new F21, F22, T11; T7 and T8 rewritten to the target; removed the CLONE_UNTRACED question and with it the Open questions heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `clone`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clone&body=Discussion+for+the+%60clone%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclone.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clone%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `clone`: the 2026-07-24 classification campaign filed issues only for syscalls that had no Detcore handler, and Detcore already handled this one.
