---
schema: syscall-pages/v1
syscall: eventfd2
nr_x86_64: 290
linux_determinism: deterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+eventfd2&body=Discussion+for+the+%60eventfd2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Feventfd2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60eventfd2%60%2C+or+in+what+the+page+says%3F%0A
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

`eventfd2(initval, flags)` creates an event counter: a kernel object holding a 64-bit unsigned value, used through a new file descriptor. A `write` adds to the counter and a `read` takes from it. Detcore handles creation at `detcore/src/syscalls/files.rs:4184`. The kernel creates the object. Under the sequentialized scheduler, Detcore secretly makes the descriptor nonblocking, so that a later blocking `read` or `write` is retried by the scheduler instead of sleeping in the kernel. Detcore records the flags the guest asked for, and those are what the guest sees.

Manual reference: Linux man-pages eventfd(2).

## Linux semantics

The call returns the lowest free descriptor. The counter starts at `initval`, which is an `unsigned int`.

`read` needs a buffer of at least 8 bytes, or it fails with `EINVAL`.
- In counter mode it returns the whole value and resets the counter to 0.
- With `EFD_SEMAPHORE` it returns 1 and subtracts 1.
- If the counter is 0, `read` waits, or fails with `EAGAIN` when the descriptor is nonblocking.

`write` takes an 8-byte value and adds it.
- Writing `0xffffffffffffffff` is `EINVAL`.
- A write that would push the counter above `0xfffffffffffffffe` waits for a `read`, or fails with `EAGAIN` when nonblocking.

`poll`, `select` and `epoll` report the descriptor readable when the counter is above 0, and writable when at least 1 can be added without waiting.

The older `eventfd(initval)` (nr 284) has no flags argument and behaves like `eventfd2` with `flags = 0`.

### Flags and options

- F1: `initval`: the counter's starting value (0 to 2^32-1)
- F2: `EFD_CLOEXEC`: the new descriptor is close-on-exec
- F3: `EFD_NONBLOCK`: the description is `O_NONBLOCK`, so a `read` of a zero counter and an overflowing `write` fail with `EAGAIN` instead of waiting
- F4: `EFD_SEMAPHORE`: `read` returns 1 and decrements, instead of returning the whole value and resetting it to 0
- F5: without `EFD_NONBLOCK`: a `read` of a zero counter waits for a `write`, and an overflowing `write` waits for a `read`
- F6: any other bit in `flags`: `EINVAL`
- F7: resource errors: `EMFILE` (per-process limit), `ENFILE` (system-wide limit), `ENODEV` (the anonymous inode cannot be mounted), `ENOMEM`
- F8: `eventfd` (nr 284): no flags argument; the same as `eventfd2` with `flags = 0`

## Determinism analysis

`linux_determinism: deterministic`. The new descriptor number, the starting counter and the descriptor's flags depend only on the arguments and on the calling process's descriptor table. Hermit serializes the table's changes. The resource errors in F7 depend on host limits, as they do for every call that creates a descriptor.

The counter can also be a source of nondeterminism. Which thread's `read` sees which `write`, and when a waiting `read` wakes up, depends on thread scheduling. That comes from the later `read`, `write` and `epoll_wait` calls, not from `eventfd2`. Under Hermit's sequentialized scheduler the order is fixed by the schedule, as long as every reader and writer is a guest thread.

## Hermit design

Target policy: `model`. The kernel holds the counter. Detcore controls how threads wait on it, and it keeps the guest-visible descriptor state in its mirror.

- **Hidden nonblocking.** In `hermit run` with thread sequentialization on, `internally_nonblocking` is true (`detcore/src/syscalls/files.rs:4189`; `use_nonblocking_sockets` at `detcore-model/src/config.rs:872`). The handler then adds `EFD_NONBLOCK` to the call (`detcore/src/syscalls/files.rs:4192`) and afterwards marks the description physically nonblocking (`detcore/src/syscalls/files.rs:4207`, `detcore/src/syscalls/helpers.rs:695`). In record and replay this is off, and the descriptor is created exactly as asked.
- **Guest-visible flags.** The descriptor is registered with the guest's own `EFD_CLOEXEC|EFD_NONBLOCK` bits, taken from the original call, not the modified one (`detcore/src/syscalls/files.rs:4197` to `detcore/src/syscalls/files.rs:4205`; `DetFd::new` splits them at `detcore/src/fd.rs:204`). `fcntl(F_GETFL)` answers from that logical state (`detcore/src/syscalls/files.rs:3097`), so the injected `O_NONBLOCK` stays hidden (F2, F3). The doc comment at `detcore/src/syscalls/files.rs:4181` states this intent.
- **Waiting (F3, F5).** A `read` on an event counter always goes through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:1854`). A `write` takes that path when the descriptor is physically nonblocking (`detcore/src/syscalls/files.rs:2283`). There, `ioaction_based_on_fd_status` (`detcore/src/syscalls/helpers.rs:745`) chooses between three strategies:
  - The guest asked for blocking but the descriptor is physically nonblocking: Detcore converts the wait into scheduler-owned polling under `InternalIOPolling` (`detcore/src/syscalls/helpers.rs:778`, `detcore/src/syscalls/helpers.rs:260`).
  - The guest asked for `EFD_NONBLOCK`: one attempt, and `EAGAIN` is returned as is.
  - Record and replay, or a run without thread sequentialization: the call blocks in the kernel through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:250`).
- **Everything else** (F1, F4, F6, F7) is decided by the kernel through `record_or_replay`. `EFD_SEMAPHORE` reaches the kernel unchanged, and Detcore does not need to track it. If Detcore cannot read the new descriptor's metadata when metadata virtualization is on, `add_fd` closes the descriptor and returns the error, so no untracked descriptor is left behind (`detcore/src/syscalls/files.rs:927`).
- `eventfd` uses the same handler through a conversion that sets `flags = 0` (`detcore/src/lib.rs:2574`) (F8).

`passthrough` would let a blocking `read` sleep in the kernel where the scheduler cannot see it. `refuse` and `emulate` would break every event loop that uses an event counter. So `model` is what the code already does.

Target (owner decision 14): a blocking `read` or `write` on an event counter is a non-blocking probe plus a scheduler wait. Sequentialized `hermit run` already realizes it (`detcore/src/syscalls/files.rs:4189-4207`, `detcore/src/syscalls/helpers.rs:778`); record and replay do not, as the next item says.

What remains before the code fully realizes `model`:

- **Record and replay** create the descriptor as a blocking one and wait in the kernel through `record_or_replay_blocking`. The Recorder's subscription list (`hermit-cli/src/recorder.rs:384` to `hermit-cli/src/recorder.rs:474`) contains `read` and `write`, but neither `eventfd2` nor `eventfd`. On replay, creation therefore runs live again, which is harmless because creation is deterministic. No test listed here checks a producer and consumer of an event counter under record and replay; T5 does.
- **`/proc/self/fdinfo`** shows the kernel's real flags, including the injected `O_NONBLOCK`. Whether Detcore filters that is outside this body; T2 should check it.

## Recommended tests

- T1 (covers F1, F2, F4): counter mode and semaphore mode, under `hermit run --verify` on ptrace, both with `initval = 0` and with a non-zero `initval`. Check `F_GETFD` for `EFD_CLOEXEC`.
  Existing candidate: `tests/c/eventfd_probe.c` (cell `c-programs/eventfd-semantics`, `tests/e2e/manifests/c-programs.yaml:15196`, verify ci on ptrace). It covers counter mode and `EFD_SEMAPHORE`, but only with `initval = 0` (`tests/c/eventfd_probe.c:66`, `tests/c/eventfd_probe.c:80`), and it does not read `F_GETFD`.
- T2 (covers F3, F5): a blocking `read` in one thread must be woken by a `write` in another, with the same result on every `--verify` run. An `EFD_NONBLOCK` descriptor must give `EAGAIN` on a zero counter. `F_GETFL` and `/proc/self/fdinfo/<fd>` must show `O_NONBLOCK` only when the guest asked for it. A blocking `write` that would overflow must wait for a `read`.
  Existing partial coverage: `mixed_epoll_sources_are_deterministic` (`detcore/tests/misc/notification_fds.rs:322`, cargo test target `tests_misc`) uses an `EFD_NONBLOCK` event counter as one of four `epoll` sources (`detcore/tests/misc/notification_fds.rs:255`).
- T3 (covers F6, F7): `eventfd2(0, 0x1)` returns `EINVAL`. With `RLIMIT_NOFILE` lowered to the number of open descriptors, the call returns `EMFILE` and opens nothing.
- T4 (covers F8): a raw `syscall(SYS_eventfd, 3)` returns a working blocking counter that starts at 3 and is not close-on-exec.
- T5 (covers F3, F5): `hermit record` and then `hermit replay` of a two-thread producer and consumer over a blocking event counter. Replay must finish and give the same output.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel counter; Detcore injects hidden EFD_NONBLOCK in run mode and turns blocking reads and writes into scheduler-owned polling); linux_determinism deterministic; 8 flag behaviours and 5 tests listed; no coverage token, tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decision 14 (already realized in sequentialized run; record/replay gap kept); target_policy unchanged; no new F/T ids; no questions removed.

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `eventfd2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+eventfd2&body=Discussion+for+the+%60eventfd2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Feventfd2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60eventfd2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `eventfd2`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- None that need an owner decision. T2 checks whether `/proc/self/fdinfo/<fd>` shows the injected `O_NONBLOCK`; that is a question about procfs filtering, not about this handler.
