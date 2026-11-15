---
schema: syscall-pages/v1
syscall: userfaultfd
nr_x86_64: 323
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+userfaultfd&body=Discussion+for+the+%60userfaultfd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fuserfaultfd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60userfaultfd%60%2C+or+in+what+the+page+says%3F%0A
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

`userfaultfd(2)` creates a descriptor through which a process handles page faults on chosen memory ranges in user space. Another thread reads fault events from the descriptor and resolves them with `ioctl` calls. Detcore lets the kernel create the descriptor and tracks it as a `Userfaultfd` entry. The protocol on the descriptor (`ioctl`, `read`, fault delivery) goes straight to the kernel, and nothing in Detcore models it. Owner decision 9 sets the target to `refuse`: the call should return a fixed `ENOSYS`, as on a kernel built without `CONFIG_USERFAULTFD`, so today's code has to change.

## Linux semantics

The call returns a new descriptor. The process first negotiates an API version and a feature set with `UFFDIO_API`, then registers address ranges with `UFFDIO_REGISTER`. When any thread touches a registered page that triggers the registered mode, the kernel suspends that thread inside the fault and queues a `struct uffd_msg`. A handler thread reads the message with `read`, `poll` or `epoll`, then resolves the fault with an operation such as `UFFDIO_COPY`, which wakes the faulting thread. Access is controlled by the `vm.unprivileged_userfaultfd` sysctl, which defaults to 0 since Linux 5.11. Linux 6.1 also added `/dev/userfaultfd` as another way to create the descriptor; that path does not use this syscall.

### Flags and options

- F1: `O_CLOEXEC`: the descriptor is close-on-exec.
- F2: `O_NONBLOCK`: `read` on an empty event queue returns `EAGAIN` instead of blocking.
- F3: `UFFD_USER_MODE_ONLY` (Linux 5.11): only faults from user mode are handled. A kernel-mode access to a registered range, such as a syscall copying into it, fails with `EFAULT` instead of queuing an event.
- F4: access and errors:
  - `EPERM` when `vm.unprivileged_userfaultfd` is 0, the caller lacks `CAP_SYS_PTRACE`, and it did not pass `UFFD_USER_MODE_ONLY`;
  - `EINVAL` for unknown flag bits;
  - `EMFILE` when the caller's descriptor limit is reached (`ENFILE` and `ENOMEM` are F11).
- F5: `UFFDIO_API` handshake: the caller names an API version and requested features. The kernel returns the features and `ioctl` set it supports, which depend on kernel version and configuration. A second handshake gives `EINVAL`.
- F6: `UFFDIO_REGISTER` and `UFFDIO_UNREGISTER`: a range is registered for missing-page faults (`MISSING`), write-protect faults (`WP`) or minor faults (`MINOR`). The kernel returns the resolution operations allowed on the range.
- F7: fault events: the faulting thread blocks in the kernel until the fault is resolved. The `uffd_msg` read from the descriptor carries the fault address and flags, and the faulting thread's ID when `UFFD_FEATURE_THREAD_ID` is negotiated.
- F8: resolution operations: `UFFDIO_COPY`, `UFFDIO_ZEROPAGE`, `UFFDIO_CONTINUE`, `UFFDIO_WRITEPROTECT`, `UFFDIO_MOVE`, `UFFDIO_POISON` and `UFFDIO_WAKE`, each with a `DONTWAKE` mode where it applies. Partial completion is reported in the argument structure, and `EAGAIN` is returned when the mapping changed underneath.
- F9: non-cooperative events, enabled by `UFFD_FEATURE_EVENT_FORK`, `EVENT_REMAP`, `EVENT_REMOVE` and `EVENT_UNMAP`. With `EVENT_FORK`, a `fork` gives the child its own userfaultfd and delivers its new descriptor number in a message on the parent's descriptor.
- F10: `UFFD_FEATURE_SIGBUS`: faults raise `SIGBUS` in the faulting thread instead of queuing an event.
- F11: host resource errors: `ENFILE` when the host's system-wide file table is full, and `ENOMEM` when the kernel cannot allocate the context.
- F12: host kernel support: a kernel built without `CONFIG_USERFAULTFD` gives `ENOSYS`, and a kernel before 5.11 treats `UFFD_USER_MODE_ONLY` as an unknown bit and gives `EINVAL`.

## Determinism analysis

Creation repeats for a fixed host: its result depends only on the flags, the `vm.unprivileged_userfaultfd` sysctl, the caller's capabilities and the descriptor table. The negotiated feature set (F5) depends on the host kernel. The event stream does not repeat:

- A fault is raised by a memory access, not a syscall, and the usual design has a handler thread separate from the faulting threads. Which thread faults first, and the order in which the handler reads the events, depend on thread scheduling.
- The messages contain raw fault addresses, which move with address-space layout randomization.
- With `UFFD_FEATURE_THREAD_ID`, the messages also carry host thread IDs.

`linux_determinism` is `conditional`: creation and the handshake repeat for a fixed host. The event order repeats only when one thread at a time can fault, in a fixed order.

### Cross-host variation

- Sysctl `vm.unprivileged_userfaultfd` and whether the caller holds `CAP_SYS_PTRACE` in the initial user namespace: decide whether creation without `UFFD_USER_MODE_ONLY` succeeds or gives `EPERM` (F4). the measurement host has the sysctl at 0.
- Kernel version and configuration: decide the feature and `ioctl` masks returned by the handshake (F5), whether `UFFD_USER_MODE_ONLY` exists, and whether the call exists at all (F12).

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel built without `CONFIG_USERFAULTFD` (F12), returned before any argument is examined, so every flag combination, valid or not, gets the same `ENOSYS` and no descriptor is created. Linux's `userfaultfd` checks in this order: the `vm.unprivileged_userfaultfd`/`CAP_SYS_PTRACE` gate, which a `UFFD_USER_MODE_ONLY` caller passes (`EPERM`, F4); then unknown flag bits (`EINVAL`, F4, F12); then context allocation and descriptor installation (`ENOMEM`, `EMFILE`, `ENFILE`, F4, F11). The fixed `ENOSYS` comes before all of these. `ENOSYS` rather than `EPERM`: with `EPERM`, decision 6's exception would apply, because Linux 5.11 and later allow a `UFFD_USER_MODE_ONLY` caller without privilege; a kernel without userfaultfd refuses every caller, so no exception arises. The supportable fallback is the one callers already need on a default host, where `vm.unprivileged_userfaultfd` is 0 and an unprivileged call without `UFFD_USER_MODE_ONLY` fails: garbage collectors and live-migration code (QEMU postcopy, CRIU lazy pages) report the feature as unavailable and use their ordinary paths (the general pattern; no specific caller's source was re-checked for this body). Owner decision 9 rules out modelling the fault protocol, so the hang hypothesis below no longer needs measuring. Code change needed: today the call is forwarded (`detcore/src/syscalls/files.rs:4508`) and the descriptor is tracked (`files.rs:4509-4515`); the target replaces the dispatch at `detcore/src/lib.rs:2596` with a fixed `ENOSYS` and adds `userfaultfd` to a refusal predicate in `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`) so that `code_facts.refused` becomes true, which L14 requires. That also removes the `UFFD_USER_MODE_ONLY`-as-`O_WRONLY` mapping at `files.rs:4512`. Linux 6.1's `/dev/userfaultfd` creates the same descriptor through `open` and `ioctl`, not this syscall, so this refusal does not close that path; see the open question. Until the code changes, the bullets below describe today's code, not the target:

- Dispatch: `detcore/src/lib.rs:2596` runs `handle_userfaultfd` (`detcore/src/syscalls/files.rs:4502-4517`). `lib.rs:1193` subscribes the syscall.
- F1, F2, F4: `files.rs:4508` runs the call through `record_or_replay`, so the result and the errors are the kernel's.
- F3: `files.rs:4509-4515` registers the descriptor with `add_fd` as `FdType::Userfaultfd`, passing the raw flags through `OFlag::from_bits_truncate`. `O_CLOEXEC` and `O_NONBLOCK` keep their meaning. `UFFD_USER_MODE_ONLY` is `1`, which is the bit for `O_WRONLY`, so the model records a write-only status flag. This is the same mistake as `memfd_create`'s `MFD_CLOEXEC` mapping, but harmless as far as this review found, because `F_GETFL` takes its result from the kernel (`files.rs:3097-3108`).
- F5, F6, F8: `handle_ioctl` (`files.rs:3190-3244`) has no case for userfaultfd requests. They fall to `_ => (None, None)` (`files.rs:3208`) and run in the kernel at `files.rs:3232`.
- F7, F9: `read` on a `Userfaultfd` descriptor passes straight to the kernel (`files.rs:1862-1864`). It does not go through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/helpers.rs:202`), which handles reads that may block on signalfd, eventfd, timerfd and inotify descriptors (`files.rs:1854-1860`). The message bytes, including fault addresses and host thread IDs, reach the guest unchanged.
- F10: `SIGBUS` delivery for a fault is generated by the kernel; Detcore has no code specific to userfaultfd for it.
- Hypothesis from reading the code, not measured: Detcore lets one guest thread run at a time.
  - A handler thread that blocks in `read` while holding that turn would wait for a fault that no other thread can raise.
  - A thread suspended inside a page fault never reaches a syscall stop at which the scheduler could switch away from it.
  - Either case could hang the run, unless the scheduler preempts a thread blocked in the kernel. This review did not examine whether it does.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F11, F12): under `hermit run --verify` on ptrace, `syscall(SYS_userfaultfd, flags)` with `flags` 0, `O_CLOEXEC`, `O_NONBLOCK`, `UFFD_USER_MODE_ONLY`, all three together, and an unknown bit (`0x80000000`) each returns -1 with `errno == ENOSYS`, never a descriptor, `EPERM` or `EINVAL`, and the lowest free descriptor number is the same before and after.
- T2 (covers F5, F6): since no descriptor can be created, a guest that follows the usual sequence (`userfaultfd`, then `UFFDIO_API`, then `UFFDIO_REGISTER` of an anonymous mapping) stops at the first step with `ENOSYS`, and its fallback path, which touches the mapping directly, runs to completion with identical output across runs.
- T3 (covers F7, F8): a program with a handler thread and two worker threads that would resolve faults with `UFFDIO_COPY` or `UFFDIO_ZEROPAGE` gets `ENOSYS` from `userfaultfd`, starts no handler, and its workers touch the pages directly; the run finishes and its output matches across runs.
- T4 (covers F3, F8): `userfaultfd(UFFD_USER_MODE_ONLY | O_CLOEXEC)` returns `ENOSYS`, and a `read(2)` into the unpopulated page the program would have registered succeeds normally instead of giving `EFAULT`.
- T5 (covers F9): a parent that would enable `UFFD_FEATURE_EVENT_FORK` gets `ENOSYS`, then forks; the child inherits no userfaultfd descriptor (its descriptor table matches a run without the call) and its page touches complete normally.
- T6 (covers F10): a program that would use `UFFD_FEATURE_SIGBUS` gets `ENOSYS`; with a `SIGBUS` handler installed, touching the page it would have registered raises no `SIGBUS`.
- U1 (covers F11; host-transient): `ENFILE` and `ENOMEM` come from host state, and the kernel's result reaches the guest unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:4508`). This counts toward done only once Hermit hides the error or reports it as an infrastructure error; under the refuse target the call is no longer forwarded and T1 covers F11.
- U2 (covers F12; host-config): the CI host runs Linux 7.1.3 with `CONFIG_USERFAULTFD=y`, so neither the `ENOSYS` nor the pre-5.11 `EINVAL` can be produced there. Hermit forwards the kernel's result unchanged (`detcore/src/syscalls/files.rs:4508`); under the refuse target every host gives the guest `ENOSYS`, which T1 asserts.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy stays investigate (creation is passed to the kernel and tracked as a Userfaultfd descriptor; ioctl, read and fault delivery reach the kernel unmodelled); linux_determinism stays conditional; 10 flag behaviours and 6 tests listed, no test names the syscall; open question for the owner: model the fault protocol under the one-thread-at-a-time scheduler, or refuse the call; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (vm.unprivileged_userfaultfd and CAP_SYS_PTRACE, kernel version and config); F4 narrowed to EPERM, EINVAL and EMFILE; new F11 ENFILE/ENOMEM (host-transient U1) and F12 no CONFIG_USERFAULTFD or pre-5.11 kernel (host-config U2); no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as without CONFIG_USERFAULTFD, before any argument check; needs a code change at detcore/src/lib.rs:2596 and a refusal predicate for L14); target_policy investigate -> refuse; no new F/T ids, T1-T6 now assert the refusal and T1 also covers F11 and F12; cross_host_variant stays true for today's forwarding code; removed the open questions on model versus refuse and on the O_WRONLY mapping; added an open question on /dev/userfaultfd

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `userfaultfd`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+userfaultfd&body=Discussion+for+the+%60userfaultfd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fuserfaultfd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60userfaultfd%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `userfaultfd`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

No test or e2e manifest cell at hermit f4055eb9cae4 names `userfaultfd`.

### Open questions

- Can a guest open `/dev/userfaultfd` (Linux 6.1+) inside Hermit's container and obtain the same descriptor through `ioctl(USERFAULTFD_IOC_NEW)`? If it can, refusing the syscall leaves the protocol reachable; whether the guest's `/dev` contains that node was not checked for this body.
