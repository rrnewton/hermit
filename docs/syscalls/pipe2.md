---
schema: syscall-pages/v1
syscall: pipe2
nr_x86_64: 293
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pipe2&body=Discussion+for+the+%60pipe2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpipe2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pipe2%60%2C+or+in+what+the+page+says%3F%0A
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

`pipe2(pipefd, flags)` creates an anonymous pipe and stores its read and write descriptors in `pipefd[0]` and `pipefd[1]`. Detcore handles it in `handle_pipe2`, which `pipe` also uses:

- it forwards the creation;
- it pins the pipe's capacity to 8192 bytes;
- it records both descriptors in its own descriptor model;
- it makes them physically nonblocking, while the guest sees the `O_NONBLOCK` state it asked for.

## Linux semantics

The kernel allocates one pipe inode on the internal pipefs mount and opens two separate open file descriptions on it, one read-only and one write-only. It installs them in the two lowest free descriptor slots, read end first, and writes both numbers to `pipefd` with a single copy.

Order of checks:

- `flags` is checked before anything is allocated.
- A failed copy to `pipefd` releases both descriptors and returns `EFAULT`, so no descriptor leaks.

Capacity:

- Normally 16 pages (65536 bytes on x86_64).
- When the creating user already holds more than `fs.pipe-user-pages-soft` pages of pipe buffers, the kernel silently creates 2 pages instead.
- An unprivileged user over `fs.pipe-user-pages-hard` gets `ENFILE`.

The capacity decides how much a writer can put in before it blocks, gets a short write, or gets `EAGAIN`. `F_GETPIPE_SZ` reports it.

`pipe(fds)` is the same call with `flags == 0`.

### Flags and options

- F1: `flags == 0`: both descriptors are blocking and not close-on-exec, exactly like `pipe`
- F2: `O_CLOEXEC`: `FD_CLOEXEC` is set on both descriptors
- F3: `O_NONBLOCK`: `O_NONBLOCK` is set on both open file descriptions. Reads of an empty pipe and writes to a full pipe return `EAGAIN` instead of blocking.
- F4: `O_DIRECT` (Linux 3.4+): packet mode. Each `write` of up to `PIPE_BUF` bytes is one packet, and a `read` smaller than the packet discards the rest of it.
  The number of packets that fit equals the number of capacity pages.
- F5: `O_NOTIFICATION_PIPE` (Linux 5.8+, same bit as `O_EXCL`): creates a kernel notification pipe. It returns `ENOPKG` when the kernel is built without `CONFIG_WATCH_QUEUE`.
- F6: any other bit in `flags`: `EINVAL`, checked before `pipefd`, so a bad pointer with bad flags is `EINVAL`
- F7: `pipefd` not writable while `flags` is valid: `EFAULT`, and no descriptor is left open
- F8: descriptor or file limits: `EMFILE` when `RLIMIT_NOFILE` leaves fewer than two free slots. `ENFILE` at the system-wide file limit, or when an unprivileged user is over `fs.pipe-user-pages-hard`.
- F9: descriptor allocation: the two lowest free descriptors, with `pipefd[0]` the read end and `pipefd[1]` the write end
- F10: capacity: 65536 bytes by default, or 8192 bytes when the creating user is over `fs.pipe-user-pages-soft`. Visible through `F_GETPIPE_SZ`, through when writes block or return `EAGAIN`, and through the packet count in F4.
- F11: identity: both descriptors name one pipefs inode. `fstat` reports `S_IFIFO` and a host-assigned `st_ino`, and `readlink("/proc/self/fd/N")` reports `pipe:[<ino>]`.

## Determinism analysis

`linux_determinism: conditional`. Most of the result follows from process-local state:

- the descriptor numbers (F9) follow from the descriptor table;
- the flags (F1 to F6) follow from the argument;
- the `EFAULT` result follows from the guest's own memory map.

Three outputs come from host-wide state, so `pipe2` repeats only when that state is fixed or hidden:

- **Capacity (F10).** It depends on how many pipe pages every process of the same host user holds. Another job on the host can push the user over `fs.pipe-user-pages-soft` between two runs. The guest then gets a 2-page pipe, and the same write that succeeded before now blocks or returns short.
- **Inode number (F11).** It is assigned from a host-global counter.
- **`ENFILE` (F8).** It depends on system-wide file counts and per-user pipe accounting. `EMFILE` depends only on the guest's own limit and table.

### Nondeterminism sources

- The host user's pipe-buffer accounting against `fs.pipe-user-pages-soft` and `fs.pipe-user-pages-hard` (F8, F10).
- The host-global pipefs inode counter and pipefs mount ID (F11).
- The system-wide open-file count (F8).
- `CONFIG_WATCH_QUEUE` in the host kernel (F5).

## Hermit design

Target policy: `model`. Detcore already models the parts of a pipe a guest can see.

Dispatch: the typed arms at `detcore/src/lib.rs:2536` (`pipe`) and `detcore/src/lib.rs:2537` (`pipe2`) both go to `handle_pipe2` at `detcore/src/syscalls/files.rs:3569`, which works in five steps.

**1. Physical nonblocking.** When threads are sequentialized, the handler adds `O_NONBLOCK` to the real call (`detcore/src/syscalls/files.rs:3584`; `use_nonblocking_sockets` at `detcore-model/src/config.rs:872`). The comment at `detcore/src/syscalls/files.rs:3574` explains why, and says this includes record and replay:

- a blocking read on an internal pipe takes the deterministic retry path instead of being treated as external blocking I/O;
- treating it as external blocking I/O deadlocks the sequentialized scheduler.

**2. No pre-call read of `pipefd`.** The comment at `detcore/src/syscalls/files.rs:3590` explains this. The kernel touches `pipefd` first, which keeps the F6/F7 precedence. The call is forwarded through `record_or_replay` at `detcore/src/syscalls/files.rs:3602`.

**3. Capacity pin.** After success, the handler sets `F_SETPIPE_SZ(8192)` on the read end (`detcore/src/syscalls/files.rs:3612`). One call sizes the whole pipe, because both ends share one pipe.

- The constant `DETERMINISTIC_PIPE_CAPACITY_BYTES` is at `detcore/src/syscalls/files.rs:256`. Its rationale, at `detcore/src/syscalls/files.rs:244`, is the soft-limit fallback in F10.
- Only an exact 8192 counts as success (`pipe_capacity_failure`, `detcore/src/syscalls/files.rs:295`).
- On failure the handler closes both new descriptors and stops the run with the policy-refusal exit (`detcore/src/syscalls/files.rs:3644`). It never invents an errno for the guest.
- The same constant is the ceiling for guest `F_SETPIPE_SZ`: a request above it gets `EPERM` (`detcore/src/syscalls/files.rs:3172`).
- `/proc/sys/fs/pipe-max-size` reports the same value (`sanitize_pipe_max_size`, `detcore/src/procfs.rs:2100`).

**4. Descriptor model.** Both descriptors are added with the guest's own `flags` and `FdType::Pipe` (`detcore/src/syscalls/files.rs:3647`), then marked physically nonblocking (`detcore/src/syscalls/files.rs:3652`). `F_GETFL` then reports the logical `O_NONBLOCK` bit, not the forced one (`detcore/src/syscalls/files.rs:3097`).

**5. Identity.** `readlink` of a `pipe:[N]` descriptor link is virtualized (`anonymous_proc_fd_identity`, `detcore/src/syscalls/namespace.rs:167`). Pipefs mount IDs in fdinfo are mapped to run-global values (`detcore/src/tool_global.rs:264`).

### Why `model` and not `passthrough` or `emulate`

- **Not `passthrough`.** It would expose the capacity fallback (F10), and the scheduler could not use its retry path on pipes.
- **Not `emulate`.** A pipe has to be a real kernel object, because the guest passes it to `read`, `write`, `splice`, `poll` and `execve`'d children.

So the model keeps the kernel object and fixes the host-dependent parts around it.

### Gaps and open points

- **Packet count under `O_DIRECT` (F4).** The pin gives 2 packet slots where a default host gives 16. This is deterministic, but a writer of many small packets blocks sooner than natively. No test covers `O_DIRECT` under Hermit.
- **`O_NOTIFICATION_PIPE` (F5).** This hypothesis was not tested. In the upstream kernel, `F_SETPIPE_SZ` on a notification pipe returns `EBUSY` (recalled from `pipe_set_size`, not checked here). On a host with `CONFIG_WATCH_QUEUE`, `pipe2(fds, O_NOTIFICATION_PIPE)` would then make the pin fail and stop the run at `detcore/src/syscalls/files.rs:3644`. The comment at `detcore/src/syscalls/files.rs:3632` says `EBUSY` "cannot hold here", and this case would contradict it. T4 measures it.
- **`F_SETFL` under record and replay.** `pipe2` forces physical nonblocking in record and replay, but `F_SETFL` forces it only outside record and replay (`detcore/src/syscalls/files.rs:3112`). A guest that clears `O_NONBLOCK` on a pipe while recording would make the pipe physically blocking again, which is the case the comment at `detcore/src/syscalls/files.rs:3574` says deadlocks. This belongs to `fcntl`'s tracking issue and is noted here because the two must agree.
- **`ENFILE` (F8).** Hermit does not hide host-wide `ENFILE`. It is reachable only under host pressure.

## Recommended tests

- T1 (covers F1, F2, F9): under `hermit run --verify` on ptrace, call `pipe2` with `0` and with `O_CLOEXEC`, then check:
  - `F_GETFD`;
  - that `pipefd[0]` is readable and `pipefd[1]` is writable;
  - that after closing descriptor 0 the next pipe's read end is 0.

  Existing candidates:
  - `tests/c/pipe2_flags.c` (cell `c-programs/pipe2-flags`, `tests/e2e/manifests/c-programs.yaml:13088`, verify ci on ptrace) checks `F_GETFD`/`F_GETFL` for flags 0, `O_CLOEXEC`, `O_NONBLOCK` and both together, through glibc `pipe2`.
  - The lowest-descriptor check exists in `tests/c/fixtures/stdio_inode_identity.c:289`, but that fixture is driven only by the KVM test `run_kvm_preserves_closed_standard_input` in `hermit-cli/tests/cli.rs`, not on ptrace.
- T2 (covers F3): `pipe2(fds, O_NONBLOCK)`, then:
  - `read` of the empty pipe returns `EAGAIN`;
  - filling the pipe ends in `EAGAIN` after exactly 8192 bytes;
  - `F_GETFL` reports `O_NONBLOCK`.

  Without `O_NONBLOCK`, `F_GETFL` must not report it, even though Detcore forces it physically. The flag readback is covered by `tests/c/pipe2_flags.c`; the `EAGAIN` behaviour is not.
- T3 (covers F4, F10): `F_GETPIPE_SZ` on both ends is 8192 on any host, and `/proc/sys/fs/pipe-max-size` reads 8192.
  Under `O_DIRECT`, three 100-byte writes to a nonblocking pipe give two packets and then `EAGAIN`, and a 50-byte read discards the rest of the packet.
  Existing candidates: `tests/c/pipe_capacity.c` (cell `c-programs/pipe-capacity`, `tests/e2e/manifests/c-programs.yaml:16062`) and `tests/c/pipe_capacity_pin.c` (cell `c-programs/pipe-capacity-pin`, `tests/e2e/manifests/c-programs.yaml:16132`, using glibc `pipe`). Both are verify ci on ptrace, and neither covers `O_DIRECT`.
- T4 (covers F5): `pipe2(fds, O_NOTIFICATION_PIPE)` returns what Linux returns on the same host: either `ENOPKG`, or success followed by `EXDEV` on `write`. The run must not stop with the policy-refusal exit. The test records which kernel configuration the CI host has.
- T5 (covers F6, F7): raw `syscall(SYS_pipe2, ...)` checks:
  - a bad pointer with valid flags, and a null pointer, return `EFAULT`;
  - a bad pointer with `-1` or `O_APPEND` returns `EINVAL`;
  - after each failure, the next `open` returns the same descriptor as before, so nothing leaked.

  Existing candidates: `tests/c/pipe2_errno_precedence.c` (cell `c-programs/pipe2-errno-precedence`, `tests/e2e/manifests/c-programs.yaml:5284`, verify ci on ptrace with guest argument `8192`) covers the errno precedence and the capacity, but not the leak check. `tests/c/errno_path_identity.c:128` passes flags `0x7fffffff`.
- T6 (covers F8): set `RLIMIT_NOFILE` so that one or zero slots are free. `pipe2` returns `EMFILE` and leaves the table unchanged. The result is identical across runs.
- T7 (covers F11): `fstat` on both ends reports `S_IFIFO` and the same `st_ino`, which matches `readlink("/proc/self/fd/N")` as `pipe:[<st_ino>]`. The value is identical across runs.
  A ptrace cell for `tests/c/fixtures/stdio_inode_identity.c` would cover this; its check is at `tests/c/fixtures/stdio_inode_identity.c:205`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · p3-samples · p3: linux_determinism conditional (capacity, pipefs inode and ENFILE depend on host-wide accounting); target_policy investigate -> model (Detcore pins capacity to 8192, forces physical nonblocking behind a logical F_GETFL and records both fds); 11 flag behaviours and 7 tests; O_NOTIFICATION_PIPE may stop the run through the capacity pin (untested hypothesis)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pipe2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pipe2&body=Discussion+for+the+%60pipe2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpipe2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pipe2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `pipe2`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Related unit tests at this SHA

- `the_pipe_ceiling_admits_exactly_the_pinned_capacity` (`detcore/src/syscalls/files.rs:4779`)
- `pipe_capacity_failure_classifies_the_errno` (`detcore/src/syscalls/files.rs:4845`)
- `pipe_capacity_failure_closes_both_created_descriptors` (`detcore/src/syscalls/files.rs:4876`)
- `pipe_max_size_reports_the_enforced_ceiling_not_the_host` (`detcore/src/procfs.rs:5546`)
- `recognizes_only_anonymous_pipe_and_socket_targets` (`detcore/src/syscalls/namespace.rs:608`)

These are unit tests of helpers, not ptrace runs. They do not count as coverage tokens.

### Stale comment

The comment at `detcore/src/syscalls/files.rs:3601` says "A C guest asserting that precedence directly is being added separately". That guest exists at this SHA as `tests/c/pipe2_errno_precedence.c`.
