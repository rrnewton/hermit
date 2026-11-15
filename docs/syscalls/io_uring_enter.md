---
schema: syscall-pages/v1
syscall: io_uring_enter
nr_x86_64: 426
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_uring_enter&body=Discussion+for+the+%60io_uring_enter%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_uring_enter.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_uring_enter%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`io_uring_enter(fd, to_submit, min_complete, flags, arg, argsz)` tells the kernel to consume submission entries from an io_uring ring and can wait for completions. Hermit refuses it. Detcore returns `ENOSYS` for every call without reading the arguments (`detcore/src/lib.rs:2547`). Because `io_uring_setup` is refused by the same arm, a guest cannot create a ring under Hermit, so this refusal mostly answers probes and calls on descriptors that are not rings.

## Linux semantics

`fd` must be an io_uring descriptor, or, with `IORING_ENTER_REGISTERED_RING`, an index into the task's registered-ring table. The kernel consumes up to `to_submit` entries from the submission ring, starts the operation each entry names, and returns the number of entries it consumed. With `IORING_ENTER_GETEVENTS` it then waits until at least `min_complete` completions are in the completion ring, a signal arrives, or the optional timeout expires. `arg` and `argsz` carry either a signal mask installed for the wait, as with `ppoll`, or, with `IORING_ENTER_EXT_ARG`, a `struct io_uring_getevents_arg` holding the mask, a minimum wait and a timeout.

The operations are multiplexed through the submission entries, not the arguments. Each entry names an `IORING_OP_*` opcode that does the work of a read, write, accept, open, timeout and so on, plus per-entry `IOSQE_*` flags. Completions arrive in the shared completion ring in an order that depends on device and scheduling timing. Some operations run on kernel worker threads (io-wq), and with `IORING_SETUP_SQPOLL` a kernel thread consumes entries without any `io_uring_enter` call.

A kernel built without `CONFIG_IO_URING` returns `ENOSYS`.

### Flags and options

- F1: `fd`: not open gives `EBADF`; open but not an io_uring gives `EOPNOTSUPP`; a ring created with `IORING_SETUP_R_DISABLED` and not yet enabled gives `EBADFD`.
- F2: `to_submit`: the number of entries to consume. The return value can be smaller, and submission can fail with `EAGAIN` or `EBUSY` when resources run out or the completion ring has overflowed.
- F3: `IORING_ENTER_GETEVENTS` with `min_complete`: wait for that many completions; a signal interrupts the wait with `EINTR`.
- F4: `IORING_ENTER_SQ_WAKEUP`: wake a sleeping SQPOLL thread.
- F5: `IORING_ENTER_SQ_WAIT`: under SQPOLL, wait until the submission ring has room.
- F6: `arg` without `IORING_ENTER_EXT_ARG`: a signal mask installed for the wait; an `argsz` that is not the kernel's signal-set size gives `EINVAL`.
- F7: `IORING_ENTER_EXT_ARG`: `arg` points to `struct io_uring_getevents_arg` (signal mask, its size, `min_wait_usec` from Linux 6.12, and a timeout); an expired timeout gives `ETIME`, a wrong `argsz` gives `EINVAL`.
- F8: `IORING_ENTER_ABS_TIMER` (Linux 6.12): the timeout is an absolute time on the ring's clock.
- F9: `IORING_ENTER_EXT_ARG_REG` (Linux 6.13): `arg` is an offset into a registered wait-argument region instead of a pointer.
- F10: `IORING_ENTER_REGISTERED_RING`: `fd` is a registered-ring index, not a descriptor.
- F11: `IORING_ENTER_NO_IOWAIT` (recent kernels): do not account the wait as I/O wait.
- F12: an unknown `flags` bit gives `EINVAL`; on a ring created with `IORING_SETUP_SINGLE_ISSUER`, a call from a task other than the issuer gives `EEXIST`.
- F13: file I/O opcodes: `READ`, `WRITE`, `READV`, `WRITEV`, `READ_FIXED`, `WRITE_FIXED`, `READ_MULTISHOT`, `FSYNC`, `SYNC_FILE_RANGE`, `FALLOCATE`, `FADVISE`, `MADVISE`, `SPLICE`, `TEE`, `FTRUNCATE`.
- F14: network opcodes: `ACCEPT`, `CONNECT`, `SEND`, `RECV`, `SENDMSG`, `RECVMSG`, `SEND_ZC`, `SENDMSG_ZC`, `SHUTDOWN`, `SOCKET`, and on recent kernels `BIND` and `LISTEN`.
- F15: polling, timeout and cancellation opcodes: `POLL_ADD`, `POLL_REMOVE`, `TIMEOUT`, `TIMEOUT_REMOVE`, `LINK_TIMEOUT`, `ASYNC_CANCEL`, `EPOLL_CTL`, `NOP`.
- F16: file-system opcodes: `OPENAT`, `OPENAT2`, `CLOSE`, `STATX`, `RENAMEAT`, `UNLINKAT`, `MKDIRAT`, `SYMLINKAT`, `LINKAT`, `SETXATTR`, `FSETXATTR`, `GETXATTR`, `FGETXATTR`.
- F17: other opcodes: `PROVIDE_BUFFERS`, `REMOVE_BUFFERS`, `FILES_UPDATE`, `MSG_RING`, `URING_CMD`, `WAITID`, `FUTEX_WAIT`, `FUTEX_WAKE`, `FUTEX_WAITV`, `FIXED_FD_INSTALL`.
- F18: per-entry flags: `IOSQE_FIXED_FILE`, `IOSQE_IO_DRAIN`, `IOSQE_IO_LINK`, `IOSQE_IO_HARDLINK`, `IOSQE_ASYNC`, `IOSQE_BUFFER_SELECT`, `IOSQE_CQE_SKIP_SUCCESS`.
- F19: outputs: the return value (entries consumed) and the completion entries (`user_data`, `res`, and `flags` such as `IORING_CQE_F_MORE` for multishot operations) that the kernel writes into the ring.

## Determinism analysis

`linux_determinism: nondeterministic`. The sources:

- **Completion order and count.** Which in-flight operations finish first, and how many completions are visible when the call returns, depend on devices, the network and kernel scheduling. An operation may complete inline or on an io-wq worker.
- **Shared memory outside system calls.** The kernel writes completion entries (F19) into guest memory and, under SQPOLL, reads submission entries without any system call, so a ptrace-based tool cannot order or record that traffic.
- **Real time and signals.** Timeouts (F7, F8, and `TIMEOUT` and `LINK_TIMEOUT` in F15) are measured on host clocks, and a signal ends a wait (F3) at a time that depends on its sender.
- **Host kernel.** Which flags and opcodes exist depends on the kernel version, and whether io_uring is available at all depends on configuration.

Error returns for a bad `fd`, unknown flags or a wrong `argsz` (F1, F6, F7, F12) do repeat. Once a ring carries real I/O, the results do not.

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_IO_URING`, returned before any argument is examined, so a valid call and a garbage-argument call get the same `ENOSYS`. `ENOSYS` stays the errno rather than the `EPERM` that `kernel.io_uring_disabled=2` gives: decision 6 keeps the errno the code already returns, and a kernel without io_uring refuses every caller, so no unprivileged success case needs an exception. Owner decision 9 makes `refuse` the long-term target; Detcore will not own a ring. The supportable fallback is epoll or blocking I/O, as `tests/c/io_uring_fallback.c:45-51` shows; libuv, for example, uses io_uring for file operations only when `io_uring_setup` succeeds and otherwise uses its thread pool (recalled, not re-checked in the libuv source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2546-2549`), so no code change is needed.

What the code at f4055eb9cae4 does:

- Detcore intercepts the call in run mode. Without `passthru_opt` it subscribes to every system call (`detcore/src/lib.rs:1094`). With `passthru_opt` its explicit list still names `io_uring_enter` (`detcore/src/lib.rs:1195`).
- The dispatch arm shared with `io_uring_setup` and `io_uring_register` returns `ENOSYS` without reading any argument (`detcore/src/lib.rs:2547` to `detcore/src/lib.rs:2549`), under the comment "io_uring completion and memory-sharing semantics are not deterministic" (`detcore/src/lib.rs:2546`). The call never reaches the kernel, so no entry is consumed, no wait happens, and every case in F1 to F19 becomes `ENOSYS`. The refusal does not depend on `--strict` or on `panic_on_unsupported_syscalls`.
- Because `io_uring_setup` gets the same answer, a guest cannot create a ring under Hermit, so none of the opcodes in F13 to F17 can run.
- `classify_syscall` places the call in the Determinized group (`detcore/src/syscall_classification.rs:122`), which its header describes as syscalls with a "handler, deterministic replacement, or explicit refusal policy" (`detcore/src/syscall_classification.rs:55`).
- `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`) includes the call (`detcore/src/syscall_classification.rs:1342`). Its doc comment requires any backend path that bypasses the Detcore tool to enforce the same refusal under fail-closed policy (`detcore/src/syscall_classification.rs:1302`). The unit test `deterministic_refusal_aggregate_includes_fixed_error_families` (`detcore/src/syscall_classification.rs:2710`) asserts the membership (`detcore/src/syscall_classification.rs:2736`).
- The virtual-time cost model charges the call the `OPEN_CLOSE_NS` class (`detcore/src/syscall_time.rs:121`).

The answer differs from Linux for an invalid descriptor: `io_uring_enter(-1, ...)` gets `ENOSYS` where Linux gives `EBADF` (F1). That is deliberate and consistent. A kernel without `CONFIG_IO_URING` also returns `ENOSYS` for every call, and the fixture `tests/c/io_uring_fallback.c:39` expects exactly that.

Why `refuse` and not the alternatives:

- `passthrough` would let the kernel consume submissions and write completions into guest memory asynchronously, outside Detcore's scheduler and invisible to record and replay.
- `model` would mean Detcore executes or orders every opcode in F13 to F17 itself and writes the completion ring deterministically. That is an emulator for a second system-call interface.
- `investigate` no longer fits, because the code has a settled behaviour with a test.

What remains before the code is known to fully realize `refuse`:

- **Argument coverage (F2 to F12).** The only test call passes `fd = -1` with every other argument zero (`tests/c/io_uring_fallback.c:39`). Because the arm reads no argument, by this reading of the code every flag also gets `ENOSYS` immediately; in particular `IORING_ENTER_GETEVENTS` with `min_complete` greater than 0 returns at once instead of waiting. Not measured; T2 measures it.
- **The ring fixture never reaches this call under Hermit.** `tests/c/io_uring_ring_determinism.c` calls `io_uring_enter` (`tests/c/io_uring_ring_determinism.c:117`) only after a successful setup, and returns early when setup fails (`tests/c/io_uring_ring_determinism.c:50`). It exercises `IORING_OP_WRITE` (`tests/c/io_uring_ring_determinism.c:108`) only on a host run without Hermit.
- **Ring descriptors from outside the guest.** If a guest could inherit an io_uring descriptor from the process that launched Hermit, `io_uring_enter` on it would still get `ENOSYS`. The io_uring arm does not cover `mmap` of such a descriptor, and whether another handler does was not checked for this body; if the guest can map an inherited SQPOLL ring, entries written there would run with no system call. Not measured; see the open question.

`linux_determinism` changes from `conditional` to `nondeterministic` for the reasons above.

## Recommended tests

- T1 (covers F1, F19): `io_uring_enter(-1, 0, 0, 0, NULL, 0)` and the same call on an ordinary descriptor (a pipe) return -1 with errno `ENOSYS`, not `EBADF` or `EOPNOTSUPP`, and the guest's fallback path runs. The output is identical across `--verify` runs.
  Existing candidate: `tests/c/io_uring_fallback.c:39` covers the `-1` case (cell `c-programs/io-uring-fallback`, `tests/e2e/manifests/c-programs.yaml:2315`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:2344`, expected stdout at `tests/e2e/manifests/c-programs.yaml:2361`; a custom ci cell also runs it three times under `--strict` on ptrace, `tests/e2e/manifests/c-programs.yaml:2386` to `tests/e2e/manifests/c-programs.yaml:2396`). It does not try an ordinary descriptor.
- T2 (covers F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12): on `-1` and on a pipe descriptor, call `io_uring_enter` with `to_submit` of 1, with `IORING_ENTER_GETEVENTS` and `min_complete` of 1 and no timeout, with each other `IORING_ENTER_*` bit alone, with an unknown flag bit, with a signal mask of the right and the wrong `argsz`, and with an `IORING_ENTER_EXT_ARG` structure carrying a timeout. Under Hermit every call returns `ENOSYS` at once, none blocks, and the output is identical across `--verify` runs.
- T3 (covers F13, F14, F15, F16, F17, F18): extend the ring program so that, when setup succeeds on the host, it submits one entry from each opcode family with each `IOSQE_*` flag; under Hermit it checks that `io_uring_setup` failed with `ENOSYS` exactly, not just any error, so that no entry of any family can run. The output is identical across `--verify` runs.
  Existing base: `tests/c/io_uring_ring_determinism.c:49` (cell `c-programs/io-uring-ring-determinism`, `tests/e2e/manifests/c-programs.yaml:2402`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:2432`). Today it submits only `IORING_OP_WRITE` and accepts any setup failure.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns ENOSYS for every call without reading arguments; no ring can be created under Hermit, so no opcode runs); linux_determinism conditional -> nondeterministic (completion order, shared-memory traffic outside system calls, host timeouts); 19 flag behaviours and 3 tests listed; open questions: is ENOSYS refusal permanent, and can a guest inherit a ring descriptor from outside; the only test passes fd -1 with zero arguments, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_IO_URING, before any argument check, already realized at detcore/src/lib.rs:2546-2549); target_policy stays refuse; no new F/T ids; removed the open question on permanence; kept the open question on an inherited ring descriptor

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_uring_enter`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_uring_enter&body=Discussion+for+the+%60io_uring_enter%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_uring_enter.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_uring_enter%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `io_uring_enter`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Can a guest inherit an io_uring descriptor from the process that launched Hermit? If it can, should Hermit close it or refuse `mmap` of it, since an inherited SQPOLL ring that the guest maps could carry I/O that no system call reveals?
