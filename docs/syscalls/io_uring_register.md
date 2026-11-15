---
schema: syscall-pages/v1
syscall: io_uring_register
nr_x86_64: 427
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_uring_register&body=Discussion+for+the+%60io_uring_register%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_uring_register.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_uring_register%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

Registers resources with an io_uring instance or changes its settings: fixed buffers, fixed files, an eventfd, worker limits, provided-buffer rings and more, chosen by an opcode. Hermit refuses it: Detcore returns a fixed `ENOSYS` for every opcode and every argument, without asking the host. It refuses `io_uring_setup` and `io_uring_enter` the same way, so a guest sees a kernel without io_uring and uses its fallback path.

Manual reference: [io_uring_register(2)](https://man7.org/linux/man-pages/man2/io_uring_register.2.html).

## Linux semantics

The raw signature is `io_uring_register(fd, opcode, arg, nr_args)`. `fd` is a ring descriptor returned by `io_uring_setup`. `opcode` picks the operation, and `arg` and `nr_args` mean different things for each opcode. The call changes state inside the ring: registered buffers are pinned and charged to `RLIMIT_MEMLOCK`, registered files are held by reference, and later submissions refer to them by index. Some opcodes return data: `IORING_REGISTER_PROBE` fills in the opcodes this kernel supports, and `IORING_REGISTER_IOWQ_MAX_WORKERS` writes back the previous limits.

A kernel built without `CONFIG_IO_URING` returns `ENOSYS` for all three io_uring syscalls. That is the kernel Hermit presents.

### Flags and options

- F1: `fd`: the ring descriptor. With the `IORING_REGISTER_USE_REGISTERED_RING` bit (bit 31) set in `opcode`, `fd` is instead an index into the ring descriptors registered with `IORING_REGISTER_RING_FDS`. A bad descriptor gives `EBADF`, and a descriptor that is not a ring gives `EOPNOTSUPP`.
- F2: fixed buffers: `IORING_REGISTER_BUFFERS` (0), `IORING_UNREGISTER_BUFFERS` (1), `IORING_REGISTER_BUFFERS2` (15), `IORING_REGISTER_BUFFERS_UPDATE` (16) and `IORING_REGISTER_CLONE_BUFFERS` (30). They pin user memory, charged to `RLIMIT_MEMLOCK`; registering twice gives `EBUSY`.
- F3: fixed files: `IORING_REGISTER_FILES` (2), `IORING_UNREGISTER_FILES` (3), `IORING_REGISTER_FILES_UPDATE` (6), `IORING_REGISTER_FILES2` (13), `IORING_REGISTER_FILES_UPDATE2` (14) and `IORING_REGISTER_FILE_ALLOC_RANGE` (25).
- F4: completion eventfd: `IORING_REGISTER_EVENTFD` (4), `IORING_UNREGISTER_EVENTFD` (5) and `IORING_REGISTER_EVENTFD_ASYNC` (7). The kernel signals the eventfd when completions are posted.
- F5: `IORING_REGISTER_PROBE` (8): reports which request opcodes this kernel supports. The answer depends on the kernel version.
- F6: `IORING_REGISTER_PERSONALITY` (9) and `IORING_UNREGISTER_PERSONALITY` (10): save the caller's current credentials and return an id that later requests can run under.
- F7: `IORING_REGISTER_RESTRICTIONS` (11) and `IORING_REGISTER_ENABLE_RINGS` (12): restrict which opcodes a ring created with `IORING_SETUP_R_DISABLED` accepts, then enable it.
- F8: io-wq worker control: `IORING_REGISTER_IOWQ_AFF` (17), `IORING_UNREGISTER_IOWQ_AFF` (18) and `IORING_REGISTER_IOWQ_MAX_WORKERS` (19). These set the CPU affinity and the bounded and unbounded worker limits of the kernel worker pool; the last one writes the previous limits back to `arg`.
- F9: `IORING_REGISTER_RING_FDS` (20) and `IORING_UNREGISTER_RING_FDS` (21): register the ring descriptor itself and return an index for use with the F1 bit.
- F10: provided-buffer rings: `IORING_REGISTER_PBUF_RING` (22), `IORING_UNREGISTER_PBUF_RING` (23) and `IORING_REGISTER_PBUF_STATUS` (26). The status opcode reports the ring's current head, which moves as requests consume buffers.
- F11: `IORING_REGISTER_SYNC_CANCEL` (24): cancel matching in-flight requests and wait, with an optional timeout, until they are gone.
- F12: `IORING_REGISTER_NAPI` (27) and `IORING_UNREGISTER_NAPI` (28): network busy-poll settings for the ring.
- F13: `IORING_REGISTER_CLOCK` (29): choose `CLOCK_MONOTONIC` or `CLOCK_BOOTTIME` for timed waits.
- F14: `IORING_REGISTER_SEND_MSG_RING` (31): post a completion to another ring without a source ring.
- F15: newer memory opcodes: `IORING_REGISTER_ZCRX_IFQ` (32, zero-copy receive from a network queue), `IORING_REGISTER_RESIZE_RINGS` (33) and `IORING_REGISTER_MEM_REGION` (34).
- F16: argument errors: an unknown opcode or a wrong `nr_args` gives `EINVAL`; an unreadable `arg` gives `EFAULT`; pinning or allocation failure gives `ENOMEM` or `EAGAIN`; some opcodes need privileges (`EPERM`). A kernel without io_uring gives `ENOSYS` for any arguments.

## Determinism analysis

`linux_determinism: conditional`. Most opcodes give the same result for the same ring state and the same kernel. Three things break that. F5's answer depends on the kernel version. F10's status and F11's cancellation depend on how far the kernel has got with requests that are still in flight, which depends on kernel worker threads and device timing. F2 and F8 depend on host limits (`RLIMIT_MEMLOCK`, CPU count). The call repeats when the kernel version, the limits and the ring's in-flight state are the same, and the last of these cannot be controlled from user space, because the ring is shared memory that the kernel fills in from its own threads.

### Nondeterminism sources

- Kernel version (F5, and which opcodes exist at all)
- In-flight request progress in kernel worker threads (F10, F11, and every completion the registration later enables)
- Host limits: `RLIMIT_MEMLOCK` for F2, CPU count for F8

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_IO_URING`, returned before any argument is examined, so a valid call and a garbage-argument call get the same `ENOSYS`. `ENOSYS` stays the errno rather than the `EPERM` that `kernel.io_uring_disabled=2` gives: decision 6 keeps the errno the code already returns, and a kernel without io_uring refuses every caller, so no unprivileged success case needs an exception. Owner decision 9 makes `refuse` the long-term target; Detcore will not own a ring. The supportable fallback is epoll or blocking I/O, as `tests/c/io_uring_fallback.c:45-51` shows; libuv, for example, uses io_uring for file operations only when `io_uring_setup` succeeds and otherwise uses its thread pool (recalled, not re-checked in the libuv source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2546-2549`), so no code change is needed.

Detcore presents a kernel built without io_uring: `io_uring_register` fails with `ENOSYS`, and so do `io_uring_setup` and `io_uring_enter`. Programs written for io_uring check for this and fall back to `epoll` or blocking I/O.

What the code does at f4055eb9:

- The dispatch arm returns `Err(Error::Errno(Errno::ENOSYS))` for all three io_uring syscalls (`detcore/src/lib.rs:2547`-`2549`), after the comment "io_uring completion and memory-sharing semantics are not deterministic" (`detcore/src/lib.rs:2546`). It reads none of the arguments and never forwards the call, so every opcode, flag and argument (F1 to F16) gets the same answer.
- `io_uring_register` is classified `Determinized` (`detcore/src/syscall_classification.rs:123`) and is a member of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`, list entry at line 1343). It is not in `is_strict_only_deterministic_refusal_syscall` (`detcore/src/syscall_classification.rs:1311`), so the refusal does not depend on fail-closed policy. The doc comment at `detcore/src/syscall_classification.rs:1293`-`1303` requires backends that run guest syscalls outside the Detcore dispatcher to enforce the same refusal whenever fail-closed policy is active; the ptrace arm itself refuses unconditionally.
- The syscall stays in the record/replay subscription (`detcore/src/lib.rs:1196`), so a recording reaches the same arm and returns the same `ENOSYS`; nothing needs to be recorded.
- Unit test: `deterministic_refusal_aggregate_includes_fixed_error_families` (`detcore/src/syscall_classification.rs:2710`) lists it at line 2737.

Why `refuse`: `model` would mean simulating the ring, its shared memory and its kernel worker threads, and the F10 and F11 results depend on kernel progress that Detcore cannot see. `passthrough` would expose that progress and the kernel version (F5) to the guest. `ENOSYS` is a real Linux answer, it needs no host state, and it is the same in every mode. Since `io_uring_setup` is refused first, a guest never holds a ring descriptor, so no working program reaches this call with a valid ring.

Nothing remains for the code to realize `refuse`; what is missing is the test coverage below.

## Recommended tests

- T1 (covers F1, F16): `syscall(SYS_io_uring_register, -1, 0, NULL, 0)` returns `-1` with `ENOSYS` under `hermit run --verify` on ptrace, after which the program uses its `epoll` fallback.
  Existing candidate: `tests/c/io_uring_fallback.c:43` (helper `expect_enosys` at line 18; cell `c-programs/io-uring-fallback`, program at `tests/e2e/manifests/c-programs.yaml:2325`, verify ci on ptrace). It exercises only opcode 0 on descriptor -1.
- T2 (covers F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15): for every opcode 0 to 34, with and without the `IORING_REGISTER_USE_REGISTERED_RING` bit, on descriptor -1 and on a valid non-ring descriptor such as a pipe, the call returns `ENOSYS`. The output must match across `--verify` runs, and the pipe must be unchanged afterwards.
- T3 (covers F1, F16): an unknown opcode (35 and `0x7fffffff`), a bad `arg` pointer and a huge `nr_args` all return `ENOSYS`, not `EINVAL` or `EFAULT`, which shows Detcore never reads the arguments. Run it under `hermit record` and `hermit replay` as well as `run --verify`; every mode must return `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (fixed ENOSYS for every opcode in every mode, never forwarded, detcore/src/lib.rs:2547-2549); linux_determinism conditional; 16 flags, 3 tests; T1 has an existing candidate (tests/c/io_uring_fallback.c:43) but no coverage recorded, so todo.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_IO_URING, before any argument check, already realized at detcore/src/lib.rs:2546-2549); target_policy stays refuse; no new F/T ids; no open questions to remove
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/io_uring_fallback.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_uring_register`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_uring_register&body=Discussion+for+the+%60io_uring_register%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_uring_register.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_uring_register%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `io_uring_register`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

Opcode numbers come from the host's `/usr/include/linux/io_uring.h` (opcodes 0 to 34, plus the `IORING_REGISTER_USE_REGISTERED_RING` bit). A later kernel can add opcodes; under Hermit they also return `ENOSYS`, because the arm ignores the opcode.

The same `ENOSYS` refusal is copied for AIO and POSIX message queues (`detcore/src/lib.rs:2231`, "mirrors the io_uring refusal").
