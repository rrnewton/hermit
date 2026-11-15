---
schema: syscall-pages/v1
syscall: clone3
nr_x86_64: 435
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clone3&body=Discussion+for+the+%60clone3%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclone3.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clone3%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 9
---

## Summary

Creates a new thread or process, like `clone`, but takes its arguments in a versioned `struct clone_args` in memory instead of in registers. That makes room for 64-bit flags, a separate exit signal, a pidfd output, chosen pids and a target cgroup. glibc 2.34 and later use it for `pthread_create` and try it first for `fork`. Hermit handles it with the same code as `clone`: the kernel creates the child with the guest's arguments, and Detcore registers the child, decides when parent and child run, and sets up the child's deterministic state.

Manual reference: [clone3(2)](https://man7.org/linux/man-pages/man2/clone3.2.html).

## Linux semantics

The raw signature is `clone3(cl_args, size)`. The kernel copies `size` bytes of `struct clone_args` from the caller and fills in the fields a smaller, older struct lacks with zeros. The fields are `flags`, `pidfd`, `child_tid`, `parent_tid`, `exit_signal`, `stack`, `stack_size`, `tls`, then `set_tid` and `set_tid_size` (from the 80-byte version), then `cgroup` (from the 88-byte version).

On success the parent gets the child's thread id, as seen in the parent's PID namespace, and the child gets 0. Unlike `clone`, the child does not continue on the parent's stack frame when `stack` is set: the kernel sets its stack pointer to the top of `stack` + `stack_size`. Which of parent and child runs first is up to the host scheduler. Failures from resource limits and capabilities depend on the host.

### Flags and options

- F1: `cl_args` and `size`: `size` below 64 (the first version of the struct) gives `EINVAL`; above a page gives `E2BIG`; a larger struct whose unknown trailing bytes are not zero gives `E2BIG`; an unreadable struct gives `EFAULT`. Versions are 64, 80 and 88 bytes.
- F2: `flags` is 64 bits. The exit-signal byte (`CSIGNAL`, except the `CLONE_NEWTIME` bit) and `CLONE_DETACHED` are not valid flags and give `EINVAL`, as do unknown bits.
- F3: `exit_signal`: a separate field, the signal sent to the parent when the child exits; it must be a valid signal number. It must be 0 with `CLONE_THREAD` or `CLONE_PARENT`, otherwise `EINVAL`.
- F4: `stack` and `stack_size`: the lowest address and the size of the child's stack; the kernel computes the top. Both 0 means the child continues on its copy of the caller's stack; `stack` without `stack_size`, or the reverse, gives `EINVAL`.
- F5: `CLONE_VM`: share the address space. Without it the child gets a copy-on-write copy.
- F6: `CLONE_FS`: share the root, current directory and umask.
- F7: `CLONE_FILES`: share the descriptor table.
- F8: `CLONE_SIGHAND`: share signal handlers. Requires `CLONE_VM`.
- F9: `CLONE_THREAD`: put the child in the caller's thread group. Requires `CLONE_SIGHAND`.
- F10: `CLONE_VFORK`: suspend the calling thread until the child calls `execve` or exits.
- F11: `CLONE_PARENT`: make the child a sibling of the caller.
- F12: `CLONE_SETTLS`: set the child's `fs` base from the `tls` field.
- F13: `CLONE_PARENT_SETTID`: store the child's thread id at `parent_tid`.
- F14: `CLONE_CHILD_SETTID` / `CLONE_CHILD_CLEARTID`: store the child's thread id at `child_tid` in the child's memory; at the child's exit, write 0 there and do a futex wake on it.
- F15: `CLONE_SYSVSEM`: share the System V semaphore undo list.
- F16: `CLONE_IO`: share the I/O scheduler context.
- F17: `CLONE_PIDFD`: store a pidfd for the child at the address in the `pidfd` field. Unlike `clone`, it can be combined with `CLONE_PARENT_SETTID`. With `CLONE_THREAD` it gives a thread pidfd from Linux 6.9 (see F28 for older kernels).
- F18: `CLONE_PTRACE` / `CLONE_UNTRACED`: make a traced caller's child traced as well, or stop a tracer from attaching to the child automatically. Hermit's target clears `CLONE_UNTRACED` before the call (F31).
- F19: namespace flags: `CLONE_NEWNS`, `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWUSER`, `CLONE_NEWPID`, `CLONE_NEWNET`, `CLONE_NEWCGROUP`, and `CLONE_NEWTIME`, which only `clone3` can pass because in `clone` its bit lies in the exit-signal byte. All but `CLONE_NEWUSER` need `CAP_SYS_ADMIN` in the caller's user namespace. Hermit's target refuses every one of them with `EPERM` (F30).
- F20: `CLONE_CLEAR_SIGHAND` (bit 32, Linux 5.5): reset every signal handler in the child to the default. With `CLONE_SIGHAND` it gives `EINVAL`.
- F21: `CLONE_INTO_CGROUP` (bit 33, Linux 5.7) with the `cgroup` field: start the child in the cgroup that descriptor names. A bad descriptor gives `EBADF`. Hermit's target refuses the flag with `EPERM` (F32).
- F22: `set_tid` and `set_tid_size` (Linux 5.5): choose the child's pid in each nested PID namespace, innermost first. Needs `CAP_SYS_ADMIN` or `CAP_CHECKPOINT_RESTORE` over each namespace (`EPERM`); more entries than nesting levels gives `EINVAL`; a pid in use gives `EEXIST`. Hermit's target refuses a non-empty `set_tid` with `EPERM` (F32).
- F23: invalid combinations give `EINVAL`: the ones `clone` rejects (`CLONE_SIGHAND` without `CLONE_VM`, `CLONE_THREAD` without `CLONE_SIGHAND`, `CLONE_NEWNS` with `CLONE_FS`, `CLONE_NEWUSER` or `CLONE_NEWPID` with `CLONE_THREAD`), plus F3 and F20.
- F24: resource and permission failures the guest decides: `EAGAIN` when the guest has lowered `RLIMIT_NPROC` below its own process count, and `EPERM`. A signal during creation makes the kernel restart the call.
- F25: the result: the child's thread id to the parent and 0 to the child. Afterwards both run concurrently, in an order the host scheduler chooses.
- F26: host-transient failures: `EAGAIN` when the host has no free pid (`kernel.pid_max`, or a cgroup `pids.max` shared with other processes) and `ENOMEM` from the host.
- F27: host-config failure: `ENOSPC` when a namespace flag hits a host namespace-count limit (`/proc/sys/user/max_*_namespaces`, for example `user.max_user_namespaces = 0`).
- F28: host-config failure: a field or flag newer than the host kernel gives `EINVAL` or `E2BIG`: `CLONE_PIDFD` with `CLONE_THREAD` before Linux 6.9, `CLONE_CLEAR_SIGHAND` and `set_tid` (80-byte struct) before 5.5, `CLONE_INTO_CGROUP` and the 88-byte struct before 5.7.
- F29: host-config failure: `EBUSY` or `EOPNOTSUPP` from `CLONE_INTO_CGROUP` when the host cgroup the descriptor names cannot take processes (it has domain controllers enabled, or is a threaded cgroup). Under the target the guest never sees these, because F32 refuses the flag first.
- F30: namespace flags refused (owner decision 15): a call with any `CLONE_NEW*` bit, including `CLONE_NEWTIME`, returns `EPERM` and creates no child. The argument and combination checks that Linux makes first (F1 to F4, F23) still give their own errors.
- F31: `CLONE_UNTRACED` stripped (owner decision 15): the call behaves as if the flag were absent. The child is traced and scheduled like any other child, and the guest's `clone_args` in memory are not changed.
- F32: `CLONE_INTO_CGROUP` and `set_tid` refused (owner decision 15): a call with `CLONE_INTO_CGROUP`, or with a non-zero `set_tid_size`, returns `EPERM` and creates no child, after the `EINVAL`, `E2BIG` and `EFAULT` checks Linux makes on the struct and its fields.

## Determinism analysis

`linux_determinism: conditional`, the same as `clone`. The thread id comes from the caller's PID namespace. In a fresh namespace it repeats when every process and thread is created in the same order; in the host namespace it depends on every other process on the machine. F22 can pin the id, but only with privileges over the namespace.

After the call, the order in which parent and child run is the host scheduler's choice, which is the main source of nondeterminism in programs that start threads or processes. Failures from resource limits (F24), F21's cgroup state and F19's capability checks depend on the host. The call repeats when the PID namespace is private, creation order is fixed by a deterministic scheduler, and the limits are not reached.

### Nondeterminism sources

- Thread id allocation, shared with every process in the PID namespace
- Which of parent and child the host scheduler runs first, and how they interleave
- Resource limits and host load: `RLIMIT_NPROC`, `pid_max`, memory, namespace counts
- Capabilities and cgroup state, for F19, F21 and F22
- The kernel version, for F17 with `CLONE_THREAD` and for which struct versions are accepted

### Cross-host variation

- Host kernel version: which struct sizes and flags exist (F28), and whether `CLONE_PIDFD` with `CLONE_THREAD` gives a thread pidfd or `EINVAL` (F17, F28).
- Host namespace-count sysctls (`/proc/sys/user/max_*_namespaces`): whether namespace flags give `ENOSPC` (F27).
- Host cgroup hierarchy: whether `CLONE_INTO_CGROUP` into a given cgroup succeeds (F29).

## Hermit design

Target policy: `model`, the same as `clone`. The kernel creates the child with the guest's own `clone_args`. Detcore registers the child, models the parts of process creation that affect determinism, and decides when parent and child run.

What the code does at f4055eb9:

- The arm at `detcore/src/lib.rs:2432` calls `handle_clone_family` (`detcore/src/syscalls/threads.rs:847`), the handler shared with `clone`, `fork` and `vfork`; the `clone` body describes it in detail.
- For `clone3` the flags come from the guest's `clone_args` (`detcore/src/syscalls/threads.rs:852`). In the pinned Reverie (`d766df20`), `CloneFamily::flags` reads the struct with `read_value` and returns empty flags if the read fails (`reverie-syscalls/src/syscalls/family.rs:215`-`221`). The flags type is 64 bits wide (`reverie-syscalls/src/args/clone.rs:21`), so bits 32 and 33 (F20, F21) are visible to Detcore.
- The exit signal is read from the `exit_signal` field, with 0 if the read fails (`detcore/src/syscalls/threads.rs:861`-`866`). The `child_tid` address is read from the struct (`reverie-syscalls/src/syscalls/family.rs:238`-`244`) and kept only with `CLONE_CHILD_CLEARTID` (`detcore/src/syscalls/threads.rs:868`, `child_tid_clear_address` at `detcore/src/tool_global.rs:3412`).
- A `CLONE_VFORK` caller gives up its scheduler turn before the call (`detcore/src/syscalls/threads.rs:907`-`910`). The original syscall is injected unchanged (`detcore/src/syscalls/threads.rs:924`), so the kernel validates `size` and applies every flag and field. A failed call cancels the vfork wait (`detcore/src/syscalls/threads.rs:926`-`936`).
- After a successful clone without `CLONE_THREAD`, Detcore forgets every known `flock` mode (`detcore/src/syscalls/threads.rs:963`-`967`). The kernel's thread id becomes the guest-visible id (`detcore/src/syscalls/threads.rs:983`, marked `TODO(T78538674), virtualized tid/pid`), which repeats because the guest runs in its own PID namespace (`default_container`, `hermit-cli/src/bin/hermit/container.rs:442`). The child is then registered (`detcore/src/syscalls/threads.rs:989`-`990`).

Why `model`: the kernel has to create the process, so `emulate` is not possible. `passthrough` would leave the post-clone race (F25) to the host. `refuse` would break `pthread_create` and `fork` in every program linked against glibc 2.34 or later, unless glibc's fallback to `clone` were relied on.

What remains before the code fully realizes `model`. These are hypotheses read from the code, not measurements:

- **Struct size.** Reverie's `CloneArgs` is the fixed 88-byte version (`reverie-syscalls/src/args/clone.rs:113`-`150`), and it is read whole whatever `size` says. A 64-byte struct placed at the very end of a mapping is valid for the kernel, but Detcore's read would fail, so Detcore would see empty flags and exit signal 0 while the kernel creates a thread or a vfork child. Detcore would then mis-model `CLONE_THREAD` and `CLONE_VFORK` (F1, F9, F10).
- **F17 pidfds.** A search of `detcore/src` finds no code for `CLONE_PIDFD`. The pidfd the kernel stores at `pidfd` is never added to Detcore's descriptor table, so `pidfd_send_signal` on it would fail with `EBADF` (`detcore/src/syscalls/files.rs:4388`-`4395`), unlike a pidfd from `pidfd_open`.
- **F20, F21, F22.** No code handles `CLONE_CLEAR_SIGHAND`, `CLONE_INTO_CGROUP` or `set_tid`. The kernel applies them. Detcore keeps no table of guest signal handlers of its own (`handle_rt_sigaction`, `detcore/src/syscalls/signal.rs:433`, forwards each change and query to the kernel), so F20 leaves nothing stale. F21 exposes host cgroup state, and F22 lets a privileged guest choose pids that Detcore's model does not expect. Owner decision 15 settles all three; see the Target paragraph below.
- **Kernel-owned flags.** As for `clone`, Detcore does not follow `CLONE_SETTLS`, `CLONE_PARENT_SETTID`, `CLONE_SYSVSEM`, `CLONE_IO`, `CLONE_PTRACE`, `CLONE_UNTRACED` or the namespace flags (F12, F13, F15, F16, F18, F19).
- **F18 `CLONE_UNTRACED`.** Owner decision 15 strips it; see the Target paragraph below.

Target (owner decisions 15 and 6): filter the arguments before the injection at `detcore/src/syscalls/threads.rs:924`, which today passes the guest's `clone_args` to the kernel unchanged. No code in `detcore/src` names any `CLONE_NEW*` bit, `CLONE_UNTRACED`, `CLONE_INTO_CGROUP` or `set_tid`. In Reverie `d766df20`, only the trap-only modes read `CLONE_UNTRACED` (`reverie-ptrace/src/task_trap_only.rs:1439`).

- Any `CLONE_NEW*` bit, `CLONE_NEWTIME` included, gives `EPERM` without injecting the call (F30, https://github.com/rrnewton/hermit/issues/3676). This closes the side door around the `unshare` and `setns` refusals. The Nix daemon is one program that falls back on this answer: with its default `sandbox-fallback = true` it builds without its namespace sandbox.
- `CLONE_INTO_CGROUP` and a non-empty `set_tid` give `EPERM` (F32). The decision says "refused" without naming the errno; `EPERM` is what Linux gives `set_tid` without `CAP_CHECKPOINT_RESTORE` over the namespace, and it keeps the clone family on one refusal errno. `set_tid` is used by checkpoint/restore tools such as CRIU, which cannot restore under Hermit anyway. systemd is the main user of `CLONE_INTO_CGROUP`; a privilege error from its spawn path is meant to make it retry without the flag, but that was not checked against systemd's source.
- `CLONE_UNTRACED` is cleared before injection (F31). The guest's struct may be read-only and must not change, so Detcore copies the struct, clears the bit in the copy, pushes the copy on the guest stack and injects a pointer to it, as `handle_rt_sigaction` already does for a sanitized `sigaction` (`signal.rs:480-490`).
- `CLONE_CLEAR_SIGHAND` is modelled. Today the kernel applies it, and because Detcore forwards every `rt_sigaction` query to the kernel, the child's handlers already read back as `SIG_DFL`. What the target adds is the `EINVAL` with `CLONE_SIGHAND` in kernel order (below), checked by T10.
- Order (owner decision 6, because `clone3` is modelled), from `kernel/fork.c`:
  - `copy_clone_args_from_user` first checks `size` (`E2BIG` above a page, `EINVAL` below 64) and copies the struct (`EFAULT`, or `E2BIG` for a non-zero tail). It then rejects a `set_tid_size` above the nesting limit or inconsistent with `set_tid`, an invalid `exit_signal`, and `CLONE_INTO_CGROUP` with a `cgroup` above `INT_MAX` or a struct shorter than 88 bytes, all with `EINVAL`. It copies the `set_tid` array (`EFAULT`).
  - `clone3_args_valid` rejects unknown flags, `CSIGNAL` bits, `CLONE_CLEAR_SIGHAND` with `CLONE_SIGHAND`, a non-zero `exit_signal` with `CLONE_THREAD` or `CLONE_PARENT`, and an invalid stack, all with `EINVAL`.
  - `copy_process` makes the flag-combination checks the `clone` body lists (F23). Only then do `copy_namespaces` (`EPERM` without a capability), `alloc_pid` (the `set_tid` checks) and `cgroup_can_fork` run.
  - The target gives every earlier error first and `EPERM` after. The `RLIMIT_NPROC` `EAGAIN` that Linux checks inside `copy_process` is not reproduced ahead of the refusal; resource limits belong to owner decision 5.
- The filter needs a read of exactly `size` bytes (at most 88), with absent fields taken as zero. Today Reverie reads a fixed 88-byte `CloneArgs` and treats a failed read as empty flags (`reverie-syscalls/src/syscalls/family.rs:215-221`). A valid 64-byte struct at the end of a mapping would then pass every `CLONE_NEW*` bit to the kernel unseen, which leaves the side door open (T12).

`linux_determinism` stays `conditional`: Hermit removes the scheduling and pid allocation sources, but host resource limits (F24) remain.

## Recommended tests

- T1 (covers F1, F2, F3, F25): raw `SYS_clone3` with each struct size (64, 80, 88) and `exit_signal = SIGCHLD`; the parent reaps the child and prints its status. Also `size` 63 (`EINVAL`), `size` above a page (`E2BIG`), a 96-byte struct with a nonzero tail (`E2BIG`), and a 64-byte struct placed at the end of a mapping, which must create the child. The output must match across `--verify` runs on ptrace.
- T2 (covers F4, F5, F25): `CLONE_VM` with a separate `stack` and `stack_size`; the child writes to shared memory and the parent waits and checks it. `stack` with `stack_size` 0 must give `EINVAL`.
- T3 (covers F6, F7, F15, F16): `CLONE_FS|CLONE_FILES|CLONE_SYSVSEM|CLONE_IO` and their absence. The child changes its directory and umask, opens a descriptor, and adjusts a semaphore with `SEM_UNDO`; the parent checks which changes it sees.
  Related: `tests/c/flock_exclusion.c:1189`-`1193` issues `clone3` with `CLONE_FILES` (scenario `clone3-files-process`), but its only harness test at this SHA, `dbt_process_clone_files_is_refused_before_copied_child_mutation` (`hermit-cli/tests/flock_exclusion.rs:718`), runs on DBT.
- T4 (covers F8, F9, F12, F13, F14): a thread created with raw `SYS_clone3` and glibc's thread flags (`CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM|CLONE_SETTLS|CLONE_PARENT_SETTID|CLONE_CHILD_CLEARTID`, `exit_signal` 0). Check `parent_tid`, the TLS base, and that `child_tid` is cleared and woken at exit.
  Related: `tests/c/pthread_lifecycle.c:26` calls `pthread_create` (cell `c-programs/pthread-lifecycle`, program at `tests/e2e/manifests/c-programs.yaml:13284`, verify ci on ptrace). Whether it reaches `clone3` depends on the glibc in the build root, which was not checked.
- T5 (covers F10): `clone3` with `CLONE_VFORK` and `exit_signal = SIGCHLD`: the parent must not run until the child exits or execs.
  Related: `start_vfork_form` at `tests/c/flock_exclusion.c:1254`-`1258` (scenario `clone3-vfork-upgrade`), driven only by the DBT test `dbt_clone_vfork_forms_fail_closed_before_copy` (`hermit-cli/tests/flock_exclusion.rs:699`).
- T6 (covers F11, F3): `CLONE_PARENT` with `exit_signal` 0: the caller's parent reaps the sibling. `CLONE_PARENT` or `CLONE_THREAD` with a nonzero `exit_signal` must give `EINVAL`.
- T7 (covers F17): `CLONE_PIDFD|CLONE_PARENT_SETTID`: both outputs are written; poll the pidfd until the child exits, signal it with `pidfd_send_signal`, and reap it with `waitid(P_PIDFD)`. This checks the F17 hypothesis above.
- T8 (covers F18, F31): a `CLONE_UNTRACED` child, with `exit_signal = SIGCHLD` and the struct in a read-only page, reads `getrandom` and `clock_gettime` and prints them. Under `--verify` the output must match, the values must be Hermit's virtual ones (the same as from a child without the flag), the child must stay under Hermit's scheduler, and the struct must be unchanged afterwards. Also check `CLONE_PTRACE`.
- T9 (covers F19, F22, F30, F32): raw `SYS_clone3` with `exit_signal = SIGCHLD` and each `CLONE_NEW*` flag alone, `CLONE_NEWTIME` included, and with `CLONE_NEWUSER|CLONE_NEWPID`; then a call with `set_tid` holding one free pid. Each must return -1 with `EPERM`, `wait` must then give `ECHILD`, and `readlink` of every `/proc/self/ns/*` entry must be unchanged, under `hermit run --verify` on ptrace and under `hermit record` and replay. Repeat once with `CLONE_VFORK|CLONE_VM|CLONE_NEWNS` to check that the parent is not left waiting.
- T10 (covers F20, F21, F32): `CLONE_CLEAR_SIGHAND` resets a handler the parent installed, and leaves an ignored signal ignored; with `CLONE_SIGHAND|CLONE_VM` it gives `EINVAL`. `CLONE_INTO_CGROUP` with a valid cgroup directory descriptor and with a bad one both give `EPERM`; with a `cgroup` above `INT_MAX`, or a 64-byte struct, it gives `EINVAL`.
- T11 (covers F23, F24): `CLONE_SIGHAND` alone, `CLONE_THREAD` without `CLONE_SIGHAND`, `CLONE_NEWNS|CLONE_FS` and a `CSIGNAL` bit in `flags` give `EINVAL`; with `RLIMIT_NPROC` lowered the call gives `EAGAIN`. A failed call must leave Detcore's state unchanged, including the known `flock` modes.
- T12 (covers F1, F23, F30, F32): the check order and the size-aware read. A 64-byte struct ending exactly at the end of a mapping, with `CLONE_NEWPID` and `exit_signal = SIGCHLD`, must give `EPERM` and create no child. `CLONE_NEWNS|CLONE_FS`, `CLONE_NEWUSER` with a `CSIGNAL` bit in `flags`, and `CLONE_NEWUTS` with `size` 63 must give `EINVAL`. A `set_tid_size` of 33 must give `EINVAL` and a `set_tid` pointing to unmapped memory `EFAULT`, both before the `EPERM`.
- U1 (covers F26; host-transient): pid exhaustion and host `ENOMEM` depend on host state shared with other processes. Detcore injects the call unchanged (`detcore/src/syscalls/threads.rs:924`), so these errors reach the guest today. F26 counts toward done only once Hermit hides them or reports them as an infrastructure error.
- U2 (covers F27; host-config): the namespace-count limits are host sysctls the CI host does not lower. Detcore injects the call unchanged (`detcore/src/syscalls/threads.rs:924`).
- U3 (covers F28; host-config): the CI host runs a kernel newer than 6.9, so it cannot produce the old-kernel `EINVAL` or `E2BIG`. Detcore injects the call unchanged (`detcore/src/syscalls/threads.rs:924`).
- U4 (covers F29; host-config): which host cgroups cannot take processes depends on the host's cgroup setup. Detcore injects the call unchanged (`detcore/src/syscalls/threads.rs:924`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (shared clone-family handler reads flags and exit_signal from clone_args and injects the call unchanged, detcore/src/syscalls/threads.rs:847); linux_determinism conditional; 25 flags, 11 tests; hypotheses: fixed 88-byte clone_args read, unregistered CLONE_PIDFD pidfd; open question: CLONE_UNTRACED (T8); no coverage recorded, so todo.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host kernel version, namespace-count sysctls, host cgroup hierarchy); F17, F21 and F24 narrowed; new F26 host pid exhaustion and ENOMEM (host-transient U1), F27 namespace-limit ENOSPC (host-config U2), F28 old-kernel EINVAL/E2BIG (host-config U3), F29 cgroup EBUSY/EOPNOTSUPP (host-config U4); no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 15 and 6 applied (CLONE_NEW* incl. NEWTIME, CLONE_INTO_CGROUP and set_tid get EPERM after Linux's struct and flag checks; CLONE_UNTRACED stripped through a stack copy of clone_args; CLONE_CLEAR_SIGHAND modelled; all missing at threads.rs:924; the filter needs a size-aware clone_args read); target_policy stays model; F18, F19, F21, F22 and F29 narrowed; new F30, F31, F32, T12; T8, T9 and T10 rewritten to the target; removed the CLONE_UNTRACED question and with it the Open questions heading


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `clone3`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clone3&body=Discussion+for+the+%60clone3%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclone3.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clone3%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `clone3`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

No test at this SHA runs a raw `clone3` on ptrace: the two `clone3` scenarios in `tests/c/flock_exclusion.c` are driven only by DBT tests, so T1 to T11 have no recorded coverage.
