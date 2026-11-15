---
schema: syscall-pages/v1
syscall: io_uring_setup
nr_x86_64: 425
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_uring_setup&body=Discussion+for+the+%60io_uring_setup%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_uring_setup.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_uring_setup%60%2C+or+in+what+the+page+says%3F%0A
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

`io_uring_setup(entries, params)` creates an io_uring instance: a file descriptor plus submission and completion rings that the process maps into its memory and drives through `io_uring_enter`. Hermit refuses it. Detcore returns `ENOSYS` for every call without reading the arguments (`detcore/src/lib.rs:2547`), so the guest sees a kernel without io_uring and has to fall back to other I/O interfaces.

## Linux semantics

The call takes a requested submission-ring size and a `struct io_uring_params`. The input fields are `flags`, `sq_thread_cpu`, `sq_thread_idle`, `wq_fd` and, with `IORING_SETUP_CQSIZE`, `cq_entries`; the `resv` words must be zero. On success it returns a new close-on-exec descriptor and fills the output fields:

- the actual `sq_entries` and `cq_entries`, rounded up to powers of two, with `cq_entries` defaulting to twice `sq_entries`;
- `features`, a bitmask of `IORING_FEAT_*` bits that depends on the running kernel;
- the `sq_off` and `cq_off` layouts.

The process then maps the rings from the descriptor with `mmap` at `IORING_OFF_SQ_RING`, `IORING_OFF_CQ_RING` and `IORING_OFF_SQES`. From then on, submissions and completions travel through that shared memory. The kernel reads submission entries and writes completion entries without any further system call from the process, and the work may run on kernel worker threads (io-wq) or, with `IORING_SETUP_SQPOLL`, on a kernel thread that polls the submission ring.

A kernel built without `CONFIG_IO_URING` returns `ENOSYS`. Since Linux 6.6 the `kernel.io_uring_disabled` sysctl can make the call fail with `EPERM`.

### Flags and options

- F1: `entries`: 0 gives `EINVAL`; a value above `IORING_MAX_ENTRIES` (32768) gives `EINVAL` unless `IORING_SETUP_CLAMP` is set; any other value is rounded up to a power of two.
- F2: the `params` pointer and its reserved fields: an unreadable or unwritable pointer gives `EFAULT`, a nonzero `resv` word gives `EINVAL`, and an unknown `flags` bit gives `EINVAL`.
- F3: `IORING_SETUP_IOPOLL`: busy-poll for completions of `O_DIRECT` I/O instead of waiting for interrupts.
- F4: `IORING_SETUP_SQPOLL`, with `sq_thread_idle`: a kernel thread polls the submission ring, so submissions need no `io_uring_enter`. Before Linux 5.11 this needed privilege.
- F5: `IORING_SETUP_SQ_AFF`, with `sq_thread_cpu`: pin the SQPOLL thread to one CPU.
- F6: `IORING_SETUP_CQSIZE`, with `cq_entries`: choose the completion-ring size.
- F7: `IORING_SETUP_CLAMP`: clamp an oversized `entries` or `cq_entries` to the maximum instead of failing.
- F8: `IORING_SETUP_ATTACH_WQ`, with `wq_fd`: share the worker-thread pool of an existing ring; a `wq_fd` that is not a ring fails.
- F9: `IORING_SETUP_R_DISABLED`: create the ring disabled until `io_uring_register` enables it.
- F10: `IORING_SETUP_SUBMIT_ALL`: keep submitting a batch after one entry fails.
- F11: `IORING_SETUP_COOP_TASKRUN` and `IORING_SETUP_TASKRUN_FLAG`: run completion work at the task's next kernel entry instead of interrupting it, and optionally flag pending work in the ring.
- F12: `IORING_SETUP_SINGLE_ISSUER` and `IORING_SETUP_DEFER_TASKRUN`: only one task may submit, and completion work runs only when that task waits in `io_uring_enter`.
- F13: `IORING_SETUP_SQE128` and `IORING_SETUP_CQE32`: double-size submission and completion entries, used by passthrough commands.
- F14: `IORING_SETUP_NO_MMAP`: the rings live in memory the process supplies through `sq_off.user_addr` and `cq_off.user_addr`, not in a mapping of the descriptor.
- F15: `IORING_SETUP_REGISTERED_FD_ONLY`: return a registered-ring index instead of installing a descriptor (requires `IORING_SETUP_NO_MMAP`).
- F16: `IORING_SETUP_NO_SQARRAY`: drop the submission index array, so entries are consumed in ring order.
- F17: `IORING_SETUP_HYBRID_IOPOLL` (Linux 6.13): sleep for part of the expected completion time before polling under `IORING_SETUP_IOPOLL`.
- F18: outputs: the returned descriptor number, `features`, `sq_entries`, `cq_entries`, `sq_off` and `cq_off`, and the ring mappings the process creates afterwards with `mmap`.
- F19: resource and policy errors: `EMFILE` or `ENFILE` when no descriptor is free, `ENOMEM` (on older kernels charged against `RLIMIT_MEMLOCK`), `EPERM` from `kernel.io_uring_disabled`, and `ENOSYS` from a kernel without `CONFIG_IO_URING`.

## Determinism analysis

`linux_determinism: nondeterministic`. The sources:

- **Availability and capability.** Whether the call succeeds at all depends on the host kernel's configuration, the `kernel.io_uring_disabled` sysctl and any seccomp policy around the process. The accepted flags (F3 to F17) and the `features` word (F18) depend on the kernel version.
- **What the ring enables.** After setup, the kernel writes completions into guest memory asynchronously. The order in which in-flight operations complete, how many completions are visible when the process reads the completion tail, and whether an operation runs inline or on an io-wq worker all depend on device and scheduling timing. With `IORING_SETUP_SQPOLL` (F4), even submission happens on a kernel thread at a time the process does not control. None of this traffic passes through a system call, so a ptrace-based tool can neither order it nor record it.
- **Descriptor number.** Like `open`, the returned descriptor is the lowest free one, which is deterministic for a given descriptor table.

The call repeats only in a narrow sense: the same kernel, configuration and parameters give the same output fields. The I/O that the ring carries does not repeat.

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_IO_URING`, returned before any argument is examined, so a valid call and a garbage-argument call get the same `ENOSYS`. `ENOSYS` stays the errno rather than the `EPERM` that `kernel.io_uring_disabled=2` gives: decision 6 keeps the errno the code already returns, and a kernel without io_uring refuses every caller, so no unprivileged success case needs an exception. Owner decision 9 makes `refuse` the long-term target; Detcore will not own a ring. The supportable fallback is epoll or blocking I/O, as `tests/c/io_uring_fallback.c:45-51` shows; libuv, for example, uses io_uring for file operations only when `io_uring_setup` succeeds and otherwise uses its thread pool (recalled, not re-checked in the libuv source for this body). Today's code already realizes the target (`detcore/src/lib.rs:2546-2549`), so no code change is needed.

What the code at f4055eb9cae4 does:

- Detcore intercepts the call in run mode. Without `passthru_opt` it subscribes to every system call (`detcore/src/lib.rs:1094`). With `passthru_opt` its explicit list still names `io_uring_setup`, `io_uring_enter` and `io_uring_register` (`detcore/src/lib.rs:1194` to `detcore/src/lib.rs:1196`).
- The dispatch arm returns `ENOSYS` for all three calls without reading any argument (`detcore/src/lib.rs:2547` to `detcore/src/lib.rs:2549`), under the comment "io_uring completion and memory-sharing semantics are not deterministic" (`detcore/src/lib.rs:2546`). The call never reaches the kernel, so no ring, descriptor or mapping is created, and every case in F1 to F19 becomes `ENOSYS`. The refusal does not depend on `--strict` or on `panic_on_unsupported_syscalls`.
- `classify_syscall` places the call in the Determinized group (`detcore/src/syscall_classification.rs:124`), which its header describes as syscalls with a "handler, deterministic replacement, or explicit refusal policy" (`detcore/src/syscall_classification.rs:55`).
- `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`) includes the call (`detcore/src/syscall_classification.rs:1341`). Its doc comment requires any backend path that bypasses the Detcore tool to enforce the same refusal under fail-closed policy (`detcore/src/syscall_classification.rs:1302`). The unit test `deterministic_refusal_aggregate_includes_fixed_error_families` (`detcore/src/syscall_classification.rs:2710`) asserts the membership (`detcore/src/syscall_classification.rs:2735`).
- The virtual-time cost model charges the call the `OPEN_CLOSE_NS` class (`detcore/src/syscall_time.rs:120`).

`ENOSYS` is what a kernel built without `CONFIG_IO_URING` returns, so a program that probes for io_uring already has to handle it as "io_uring is unavailable". The fixture `tests/c/io_uring_fallback.c` shows the pattern: it expects `ENOSYS` (`tests/c/io_uring_fallback.c:35`) and then falls back to epoll (`tests/c/io_uring_fallback.c:45` to `tests/c/io_uring_fallback.c:51`).

Why `refuse` and not the alternatives:

- `passthrough` would let the kernel write completions into guest memory asynchronously, outside Detcore's scheduler and invisible to record and replay. That is the reason given in the code comment.
- `model` would mean Detcore owns the rings. It would have to execute or order every submitted operation itself, across dozens of opcodes, write the completions deterministically, and refuse SQPOLL. That is an emulator for a second system-call interface, not a rewrite of a few result fields.
- `investigate` no longer fits, because the code has a settled behaviour with tests.

What remains before the code is known to fully realize `refuse`:

- **Argument coverage (F1 to F17, F19).** Both fixtures pass only `entries = 8` with zeroed parameters (`tests/c/io_uring_fallback.c:35`, `tests/c/io_uring_ring_determinism.c:49`). Because the arm reads no argument, by this reading of the code every other combination also gets `ENOSYS`. Not measured; T2 measures it.
- **The ring fixture accepts any failure.** `tests/c/io_uring_ring_determinism.c:50` treats any negative return as "unsupported", so it would still pass if Hermit returned a different errno. Only `tests/c/io_uring_fallback.c:19` checks for `ENOSYS` exactly.
- **Programs without a fallback.** A program that requires io_uring fails under Hermit. That is the cost of `refuse`, which owner decision 9 accepts.

`linux_determinism` changes from `conditional` to `nondeterministic` for the reasons above.

## Recommended tests

- T1 (covers F1, F18): `io_uring_setup(8, &params)` with zeroed `params` returns -1 with errno `ENOSYS`, leaves `params` unchanged (no `features`, no offsets), creates no new entry in `/proc/self/fd`, and the guest's fallback path runs. The output is identical across `--verify` runs.
  Existing candidate: `tests/c/io_uring_fallback.c:35` (cell `c-programs/io-uring-fallback`, `tests/e2e/manifests/c-programs.yaml:2315`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:2344`, expected stdout at `tests/e2e/manifests/c-programs.yaml:2361`; a custom ci cell also runs it three times under `--strict` on ptrace, `tests/e2e/manifests/c-programs.yaml:2386` to `tests/e2e/manifests/c-programs.yaml:2396`). It checks errno and the fallback, but not that `params` stays unchanged or that no descriptor appears.
  Weaker candidate: `tests/c/io_uring_ring_determinism.c:49` (cell `c-programs/io-uring-ring-determinism`, `tests/e2e/manifests/c-programs.yaml:2402`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:2432`; also the test `io_uring_ring_payload_is_deterministic` at `hermit-cli/tests/io_uring_ring_determinism.rs:108`, which runs `hermit run` with no backend flag). It accepts any failure.
- T2 (covers F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F19): call `io_uring_setup` with `entries` of 0, 1, 8 and 32769; with each `IORING_SETUP_*` bit alone, together with the companion field it uses (`sq_thread_idle`, `sq_thread_cpu`, `cq_entries`, `wq_fd`, `user_addr`); with a nonzero `resv` word, an unknown flag bit and a NULL `params`; and once with `RLIMIT_NOFILE` lowered so that no descriptor is free. Under Hermit every call returns `ENOSYS`, never a descriptor, `EINVAL`, `EFAULT` or `EMFILE`, and the output is identical across `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns ENOSYS for every call without reading arguments, so the call never reaches the kernel); linux_determinism conditional -> nondeterministic (kernel-dependent availability and features; the ring it creates is written asynchronously through shared memory); 19 flag behaviours and 2 tests listed; open question: is ENOSYS refusal permanent or is a Detcore-owned ring the long-term goal; existing tests pass only zeroed params, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_IO_URING, before any argument check, already realized at detcore/src/lib.rs:2546-2549); target_policy stays refuse; no new F/T ids; removed the open question on permanence and ENOSYS versus EPERM, and its heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `io_uring_setup`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+io_uring_setup&body=Discussion+for+the+%60io_uring_setup%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fio_uring_setup.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60io_uring_setup%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `io_uring_setup`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
