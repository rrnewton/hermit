---
schema: syscall-pages/v1
syscall: pipe
nr_x86_64: 22
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pipe&body=Discussion+for+the+%60pipe%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpipe.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pipe%60%2C+or+in+what+the+page+says%3F%0A
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

`pipe(pipefd)` creates an anonymous pipe and stores its read and write descriptors in `pipefd[0]` and `pipefd[1]`. Detcore models it as `pipe2` with flags 0. The pipe is made physically nonblocking so that blocking I/O on it follows the deterministic retry schedule, its capacity is pinned to 8192 bytes, and both ends are registered as internal pipe fds (`detcore/src/lib.rs:2536`, `detcore/src/syscalls/files.rs:3569-3658` at hermit f4055eb9cae4).

## Linux semantics

`pipe` takes one argument, a pointer to two ints. On success the kernel writes the two lowest-numbered free descriptors there. Both are blocking, have no close-on-exec flag, and share one pipe buffer.

Errors:

- `EFAULT`: `pipefd` is not writable. The kernel creates nothing visible.
- `EMFILE`: the per-process descriptor limit (`RLIMIT_NOFILE`) is reached.
- `ENFILE`: the system-wide file limit is reached, or the per-user pipe hard limit is reached for an unprivileged user.

The pipe's capacity is 64 KiB by default (16 pages on x86-64). Once the creating user exceeds `/proc/sys/fs/pipe-user-pages-soft`, new pipes get only two pages. That host-global, per-user accounting decides how much a writer can push before it blocks, and the guest can read it with `F_GETPIPE_SZ`. `pipe` is `pipe2` with flags 0. On the glibc 2.34 host used for this analysis, `strace` shows glibc's `pipe()` issuing `SYS_pipe` (`pipe([3, 4]) = 0` for `bash -c 'echo hi | cat'`), so C programs that call `pipe()` reach this syscall. Other C libraries may call `pipe2` instead.

### Flags and options

- F1: success writes two descriptors, read end then write end, at the lowest free fd numbers; both blocking and without `FD_CLOEXEC`.
- F2: data written to `pipefd[1]` is read from `pipefd[0]` in FIFO order.
- F3: pipe capacity: the host default is 64 KiB, and two pages under `pipe-user-pages-soft` pressure; visible through `F_GETPIPE_SZ` and through when writers block.
- F4: an unwritable `pipefd` gives `EFAULT`.
- F5: `EMFILE` when fewer than two descriptors are free under `RLIMIT_NOFILE`.
- F6: `ENFILE` when the system-wide file limit or the per-user pipe hard limit is reached.
- F7: blocking status as seen by the guest: `F_GETFL` reports no `O_NONBLOCK`, and reads and writes block, as for any fresh pipe.

## Determinism analysis

The fd numbers repeat given the same descriptor table (F1), and `EMFILE` repeats given the same `RLIMIT_NOFILE` (F5). Three things depend on the host:

- The pipe's capacity depends on host-global per-user page accounting, which can change between runs (F3). That changes whether the same write completes at once or blocks.
- `ENFILE` depends on system-wide file usage (F6).
- How reads and writes on the pipe interleave depends on scheduling. That belongs to `read`, `write` and friends, not to `pipe` itself.

Hence `conditional`.

### Cross-host variation

- Host `RLIMIT_NOFILE` inherited by Hermit's guest: the kernel enforces that limit, not the virtual 1048576 that Detcore reports (`detcore/src/tool_local.rs:289`), so the descriptor count at which F5's `EMFILE` appears differs between hosts.

## Hermit design

The policy is `model`, as implemented at hermit f4055eb9cae4:

- **Dispatch:** `Syscall::Pipe` is converted to a `pipe2` call with flags 0 and handled by `handle_pipe2` (`detcore/src/lib.rs:2536`).
- **Nonblocking:** when `use_nonblocking_sockets()` holds (sequentialized threads, sockets not externalized: `detcore-model/src/config.rs:872-874`), `O_NONBLOCK` is ORed into the injected flags (`detcore/src/syscalls/files.rs:3584-3589`). The comment at `detcore/src/syscalls/files.rs:3574-3583` explains why. Pipes are container-internal, so a blocking read or write on them must follow the InternalIOPolling retry path rather than be treated as external blocking I/O, which deadlocks the sequentialized scheduler under record/replay. The physical `O_NONBLOCK` stays hidden because `F_GETFL` is virtualized (F7).
- **No pre-call read:** the handler deliberately reads nothing from `pipefd` before the call, so a bad pointer yields the kernel's `EFAULT` and the kernel's argument-validation order is preserved (`detcore/src/syscalls/files.rs:3590-3601`) (F4).
- **Record/replay:** the call goes through `record_or_replay` (`detcore/src/syscalls/files.rs:3602`). Errors return early, so `EMFILE` and `ENFILE` reach the guest unchanged (F5, F6).
- **Capacity pin:** on success, `F_SETPIPE_SZ(8192)` pins the capacity (`detcore/src/syscalls/files.rs:3605-3646`; the constant and its rationale are at `detcore/src/syscalls/files.rs:242-256`). Only an exact 8192 counts as success (`detcore/src/syscalls/files.rs:295-305`). If pinning fails, both new fds are closed and the run stops with the policy-refusal exit (`detcore/src/syscalls/files.rs:3644`) rather than continue with a host-dependent capacity (F3).
- **Registration:** both ends are registered as `FdType::Pipe` (`detcore/src/syscalls/files.rs:3647-3650`) and marked nonblocking in Detcore's fd model (`detcore/src/syscalls/files.rs:3651-3654`) (F1, F2).

Why not `passthrough`: the capacity pin and the internal nonblocking conversion are what make pipe I/O deterministic. Why not `emulate`: the kernel object must be real so that it is shared across `fork` and `exec`.

`ENFILE` stays host-dependent. Turning it into a fixed result would need a virtual system-wide file count, which nothing in Detcore models. It is a resource-exhaustion edge and not a reason for another policy.

## Recommended tests

- T1 (covers F1, F3): `pipe()` followed by `F_GETPIPE_SZ` on both ends reports 8192 on every run. Existing candidate: `tests/c/pipe_capacity_pin.c` (manifest cell `c-programs/pipe-capacity-pin`, `tests/e2e/manifests/c-programs.yaml:16132`, verify on ptrace).
- T2 (covers F1, F2): `pipe()` IPC between parent and child produces identical output across runs. Existing candidates: `tests/c/pipe_ipc.c` (cell `c-programs/pipe-ipc`, `tests/e2e/manifests/c-programs.yaml:15066`) and `tests/rust/pipe_basics.rs` (cargo target `rustbin_pipe_basics`, used by `record_rs_pipe_basics` in `hermit-cli/tests/record_replay.rs:2330`).
- T3 (covers F4): raw `SYS_pipe` with a `NULL` or read-only `pipefd` returns `EFAULT`, the guest survives, and no descriptor is leaked (the next `open` gets the same fd as natively).
- T4 (covers F5): with `RLIMIT_NOFILE` set to leave a single free fd, raw `SYS_pipe` returns `EMFILE` identically across runs.
- T5 (covers F7): `F_GETFL` on both ends of a raw `SYS_pipe` pipe shows no `O_NONBLOCK`, and a read on the empty pipe blocks until a second thread writes, identically across runs.
- T6 (covers none): on a disposable host where `/proc/sys/fs/pipe-user-pages-hard` has been lowered, an unprivileged guest that holds pipes up to the limit gets `ENFILE` from the next `pipe`. This needs a host sysctl, so it is manual-only. It no longer claims F6, which U1 records as a host-transient error.
- U1 (covers F6; host-transient): `ENFILE` depends on the host's system-wide file count and on the per-user pipe pages that every process of the same host user holds. The host call at `detcore/src/syscalls/files.rs:3602` returns the error to the guest unchanged today. F6 counts toward done only once Hermit hides this error or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (pipe routed as pipe2 flags 0; internal O_NONBLOCK hidden from the guest, 8192-byte capacity pin with fail-closed exit, no pre-call pipefd read); linux_determinism conditional confirmed (host pipe capacity, ENFILE); 7 flags, T1-T6 proposed; tracking_status stays todo.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (the kernel enforces the host RLIMIT_NOFILE, not the virtual one, so the EMFILE point of F5 differs between hosts); F6 ENFILE recorded as host-transient U1 and T6 no longer claims F6; no open questions
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/pipe_capacity_pin.c, T2 covered by tests/c/pipe_ipc.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pipe`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pipe&body=Discussion+for+the+%60pipe%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpipe.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pipe%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `pipe`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

At hermit f4055eb9cae4 no test file names `SYS_pipe` directly (`tests_naming: 0`). The C and Rust tests listed above reach it through libc `pipe()`. A raw-syscall test (T3-T5) would make that coverage independent of the C library.

The comment at `hermit-cli/tests/writev_determinism.rs:379-380` says that replaying dynamically allocated pipe fds "is currently blocked by the recorder's independent fd-numbering gap". That is a recorder issue that touches pipe fds; it is not a defect in `handle_pipe2`.
