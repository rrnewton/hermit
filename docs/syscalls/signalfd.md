---
schema: syscall-pages/v1
syscall: signalfd
nr_x86_64: 282
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+signalfd&body=Discussion+for+the+%60signalfd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsignalfd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60signalfd%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`signalfd(fd, mask, sizemask)` is the legacy, flagless form of `signalfd4`: with `fd` = -1 it creates a descriptor from which the caller reads pending signals in `mask` as records, and with an existing signalfd it replaces that descriptor's mask. Detcore turns the legacy call into `signalfd4` with flags 0 (`detcore/src/lib.rs:2576`), passes it to the kernel through `record_or_replay`, and registers the result as a signalfd (`detcore/src/syscalls/files.rs:4213`). Signal delivery itself is left to the kernel, and a blocking read waits outside the deterministic scheduler.

Manual reference: Linux man-pages signalfd(2).

## Linux semantics

A signalfd does not change how signals are delivered. A signal in the mask is reported through the descriptor only while it is pending, which in practice means the program has blocked it with `sigprocmask` or `pthread_sigmask`; otherwise the default action or a handler takes it first. A `read` dequeues pending signals and returns one 128-byte `struct signalfd_siginfo` per signal, as many as fit in the buffer. It returns the signals pending for the reading thread and for its process, so which thread reads decides which thread-directed signals it can see.

The legacy syscall takes no flags argument. The new descriptor is therefore blocking and not close-on-exec; `SFD_NONBLOCK` and `SFD_CLOEXEC` exist only on `signalfd4`. Passing an existing signalfd as `fd` replaces its mask and returns the same descriptor; the descriptor's status flags do not change.

After `fork`, the child's copy of the descriptor reads the child's own pending signals, not the parent's. After `execve`, the descriptor stays open (the legacy call cannot set close-on-exec) and keeps its mask, and the blocked-signal mask is also preserved, so the new program can keep reading.

### Flags and options

- F1: `fd` = -1: create a new signalfd and return the lowest free descriptor number.
- F2: `fd` is an existing signalfd: replace its mask with `mask` and return `fd`. The descriptor's status flags are not changed.
- F3: `fd` is open but is not a signalfd: `EINVAL`. `fd` is neither -1 nor open: `EBADF`.
- F4: `sizemask` must equal the kernel's signal-set size, 8 on x86_64; any other value gives `EINVAL` (host probe: 4 and 16 both fail).
- F5: `mask` is not readable: `EFAULT`.
- F6: `SIGKILL` and `SIGSTOP` in `mask` are silently ignored.
- F7: no flags argument: the legacy call behaves as `signalfd4(fd, mask, 8, 0)`, so a new descriptor is blocking and survives `execve`. `SFD_NONBLOCK` and `SFD_CLOEXEC` are not reachable through this syscall; `fcntl` can set `O_NONBLOCK` and `FD_CLOEXEC` afterwards.
- F8: `read` on the descriptor: returns whole 128-byte `signalfd_siginfo` records for signals in the mask that are pending for the reading thread or its process, and dequeues them. A buffer shorter than 128 bytes gives `EINVAL`. With nothing pending, `read` blocks, or gives `EAGAIN` once `O_NONBLOCK` is set.
- F9: record contents: `ssi_signo`, `ssi_code`, the sender's `ssi_pid` and `ssi_uid`, and fields that depend on the signal, such as `ssi_status`, `ssi_utime` and `ssi_stime` for `SIGCHLD`, or `ssi_int` and `ssi_ptr` for a queued real-time signal.
- F10: readiness: `poll`, `select` and `epoll` report readable while a signal in the mask is pending for the polling thread or its process.
- F11: inheritance: a `fork` child's copy reads the child's own signals; after `execve` the descriptor and its mask are kept.
- F12: errors on creation: `EMFILE` when the descriptor table is full under `RLIMIT_NOFILE`.
- F13: host-transient errors on creation: `ENFILE` at the system-wide file limit and `ENOMEM` when the host cannot allocate.
- F14: host-config error on creation: `ENODEV` when the host kernel has no internal anonymous-inode mount.

## Determinism analysis

`linux_determinism: conditional`. Creation and mask updates are deterministic given the descriptor table and the arguments. What a later `read` returns depends on when signals arrive. That is deterministic only when every signal is sent by the program itself at a fixed point, for example `raise` or `kill` on itself, and no outside process, timer or child exit sends one.

### Nondeterminism sources

- Signals sent from outside the program or from other processes, whose arrival time depends on the host
- Signals generated by timers (`alarm`, `setitimer`, `timer_create`) and by child exits, whose timing depends on scheduling
- Which thread reads first when several threads read the same signalfd, and whether a process-directed signal is reported to a reader or handled elsewhere
- Record fields that carry host values: `ssi_utime` and `ssi_stime` for `SIGCHLD` are host CPU time (the host trace under Additional information shows `si_utime=1` on a `SIGCHLD`), and `ssi_pid` and `ssi_uid` name the sender
- The time at which a blocking `read` returns

### Cross-host variation

- Host `RLIMIT_NOFILE` inherited by Hermit's guest: the kernel enforces that limit, not the virtual 1048576 that Detcore reports (`detcore/src/tool_local.rs:289`), so the descriptor count at which F12's `EMFILE` appears differs between hosts.
- Host kernel build: whether the internal anonymous-inode mount exists, which decides F14's `ENODEV`.

## Hermit design

Current behaviour (hermit 239ca43d3092): signalfd records are reconciled against Detcore's signal model instead of passing through bare. A read runs as a nonblockable descriptor call and each returned record is put through the child-exit ledger and the discarded-at-send filtering (`read_signalfd`, `detcore/src/syscalls/files.rs:10804`); a read whose records were all dropped duplicates reads again or answers 0, and a faulted copy-out that would silently drop a record is refused by name rather than losing it. Where the mask covers a signal with ledger state, reads are taken one record per kernel read so the reconciliation order is the kernel's dequeue order. This supersedes the summary claim below that signal delivery and blocking reads are simply left to the kernel outside the scheduler.

Target policy: `model`. The kernel keeps the descriptor, its mask and the signal queue, as it does today. Detcore must own the decisions that make a read depend on timing: when a blocking reader wakes, and which signals are pending at each point of the schedule. A full emulation would have to reimplement signal queueing for every signal source, and refusing would break programs such as systemd and event loops that consume signals through a signalfd.

What the code does at this SHA:

- The legacy call reaches the `signalfd4` handler (`detcore/src/lib.rs:2576`). The conversion in the pinned reverie (`reverie-syscalls/src/syscalls.rs:3071` at reverie d766df20b780) sets the flags argument to 0 (`reverie-syscalls/src/syscalls.rs:3074`), which matches F7.
- `handle_signalfd4` sends the call to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:4218`) and registers the returned descriptor with the guest's flags (`detcore/src/syscalls/files.rs:4219`). The kernel's own argument checks give F3 to F6 their Linux results.
- A `read` on a signalfd goes through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:1854`). Unlike an eventfd, a signalfd is never made nonblocking in the kernel behind the guest's back. For a descriptor that is nonblocking in both views, `ioaction_based_on_fd_status` runs the read once (`detcore/src/syscalls/helpers.rs:775`), which is how the existing deterministic tests use it. For a blocking descriptor, it chooses the blocking strategy (`detcore/src/syscalls/helpers.rs:772`), and the read runs through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:250`). That path takes the thread out of the run queue, and it rejoins at a time the scheduler does not choose (`detcore/src/syscalls/helpers.rs:155`). The doc comment of `record_or_replay_blocking` says it is only valid in record/replay or in `hermit run` without sequentialized threads (`detcore/src/syscalls/helpers.rs:108`), but a blocking signalfd read takes it in sequentialized `hermit run` too.
- The fdinfo sanitizer passes the `sigmask` line through unchanged (`detcore/src/procfs.rs:2827`), which is guest state.
- Detcore does not read or rewrite the `signalfd_siginfo` records the kernel returns; nothing under `detcore/src` refers to that structure at this SHA.

What remains before the code fully realizes `model`:

- **Blocking reads.** A blocking `read` must park in the deterministic scheduler and wake at a scheduled point once a signal in the mask is pending, instead of taking the external blocking path. Making the descriptor nonblocking in the kernel and retrying, as the eventfd handler does (`detcore/src/syscalls/files.rs:4189`), is one way; it also needs F_GETFL to keep reporting the guest's view.
- **Target (owner decision 14):** a blocking `read` is a non-blocking probe plus a scheduler wait. The handler creates the kernel descriptor with `SFD_NONBLOCK` added under the condition `handle_eventfd2` uses (`use_nonblocking_sockets()` and not record/replay), registers the guest's own flags, and calls `maybe_set_nonblocking_fd`, as `handle_eventfd2` does (`detcore/src/syscalls/files.rs:4189-4207`); `F_SETFL` adds `FdType::Signalfd` to its forced list (`detcore/src/syscalls/files.rs:3113`). `ioaction_based_on_fd_status` then picks `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:776-778`) instead of the blocking path it picks today (`detcore/src/syscalls/helpers.rs:772`, then `detcore/src/syscalls/helpers.rs:250`). A mask update through the legacy call must keep the existing record (F2 below), or it would drop the internal flag from Detcore's view. Not `refuse`: blocking signalfd reads are the normal use of the descriptor.
- **Mask updates (F2).** The handler registers the returned descriptor whether the call created it or only changed its mask. `add_fd` builds a new open-file record with a new id (`detcore/src/tool_local.rs:759`) and replaces the old entry (`detcore/src/tool_local.rs:747`). The legacy call always passes flags 0, so after `signalfd(fd, ...)` on a descriptor created as nonblocking, Detcore's record would say blocking while the kernel's file is still nonblocking. `fcntl(F_GETFL)` reports Detcore's view (`detcore/src/syscalls/files.rs:3099`), and a `dup` copy made earlier keeps the old record. This is read from the code, not measured. The header of `tests/c/signalfd_create.c` (line 12) records that mask updates diverge across backends and keeps them out of that test. T3 checks it.
- **Host values in records (F9).** `ssi_utime` and `ssi_stime` carry host CPU time. If Hermit virtualizes CPU time elsewhere, these fields should match it.

## Recommended tests

- T1 (covers F1, F7, F11): create a signalfd with `syscall(SYS_signalfd, -1, &mask, 8)` and check that the descriptor is the lowest free number, that `F_GETFD` shows no close-on-exec and `F_GETFL` no `O_NONBLOCK`, and that a `fork` child's copy reads a signal the child raised while the parent's pending set is unchanged. Run under `hermit run --verify` on ptrace.
  Existing candidate for creation through `signalfd4`: `tests/c/signalfd_create.c` (cell `c-programs/signalfd-create`, `tests/e2e/manifests/c-programs.yaml:13656`, verify ci on ptrace). It creates two descriptors with `SFD_NONBLOCK | SFD_CLOEXEC` through glibc, so it never issues syscall 282 and never reads.
- T2 (covers F3, F4, F5, F6, F12): with raw `syscall(SYS_signalfd, ...)`, check `EINVAL` for a regular file descriptor and for `sizemask` 4 and 16, `EBADF` for a closed descriptor, `EFAULT` for a null mask, that a mask containing `SIGKILL` and `SIGSTOP` succeeds, and `EMFILE` with `RLIMIT_NOFILE` lowered to the number of open descriptors.
- T3 (covers F2, F7): create a signalfd with `signalfd4(-1, {SIGUSR1}, 8, SFD_NONBLOCK)`, `dup` it, then call `syscall(SYS_signalfd, fd, {SIGUSR2}, 8)`. Check the call returns `fd`, that `F_GETFL` still shows `O_NONBLOCK` on both descriptors, that a raised `SIGUSR2` is read from both, and that a raised `SIGUSR1` is not. Compare with a native run.
- T4 (covers F8, F9): block `SIGUSR1` and `SIGUSR2`, raise both, and read once with a 256-byte buffer, then once with a 64-byte buffer. Check two records come back in signal-number order with `ssi_pid` equal to `getpid()`, and that the short read gives `EINVAL`.
  Existing candidate: `signalfd_delivery_is_deterministic` at `detcore/tests/misc/notification_fds.rs:312`, which raises `SIGUSR1` and reads one record from an `SFD_NONBLOCK` signalfd five times under Detcore and compares the output.
- T5 (covers F8): one thread blocks in `read` on a blocking signalfd while another thread does fixed work and then sends the signal with `tgkill`. Under `hermit run --verify` the output, including any virtual timestamps printed after the read, must match on every run. With the current external blocking path this is expected to show nondeterminism.
- T6 (covers F10): put a signalfd in an epoll set with other ready descriptors and check the event order across runs.
  Existing candidates: `mixed_epoll_sources_are_deterministic` at `detcore/tests/misc/notification_fds.rs:322`; `mixed_fd_readiness_is_deterministic` at `hermit-cli/tests/epoll_determinism.rs:387`, which runs the `mixed` scenario of `tests/c/epoll_determinism.c` (signalfd created at `tests/c/epoll_determinism.c:236`); and `tests/c/event_delivery_ordering.c` (cell `c-programs/event-delivery-ordering`, `tests/e2e/manifests/c-programs.yaml:17040`, verify ci on ptrace with `bitwise_parity`), whose signalfd leg starts at `tests/c/event_delivery_ordering.c:134`.
- T7 (covers F9): reap a child through a signalfd's `SIGCHLD` record and print `ssi_status`, `ssi_utime` and `ssi_stime`. Check whether the CPU-time fields are identical across runs under `hermit run --verify`.
- U1 (covers F13; host-transient): `ENFILE` and `ENOMEM` depend on host-wide state shared with other processes. The host call at `detcore/src/syscalls/files.rs:4218` returns the error to the guest unchanged today. F13 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F14; host-config): `ENODEV` needs a host kernel without the anonymous-inode mount, which the CI host cannot provide. Hermit forwards the kernel's answer unchanged (`detcore/src/syscalls/files.rs:4218`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel keeps the mask and signal queue; Detcore converts the legacy call to signalfd4 with flags 0 and registers the descriptor, but a blocking read still uses the external blocking path and a mask update replaces Detcore's record of the descriptor); linux_determinism stays conditional; 12 flag behaviours and 7 tests listed; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (the kernel enforces the host RLIMIT_NOFILE, not the virtual one, so the EMFILE point of F12 differs between hosts; host kernel build for ENODEV); F12 split into F12 (EMFILE), F13 (ENFILE, ENOMEM; U1 host-transient) and F14 (ENODEV; U2 host-config); open question untouched (pending clocks decision)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decision 14 (blocking read becomes probe plus scheduler wait through an internal SFD_NONBLOCK, as for eventfd); target_policy unchanged; no new F/T ids; no questions removed (the ssi_utime/ssi_stime CPU-time question is not answered by decision 13).


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated for signalfd record reconciliation against the signal model; code facts also refreshed at conversion.

## Discussion

[Start a discussion about `signalfd`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+signalfd&body=Discussion+for+the+%60signalfd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsignalfd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60signalfd%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `signalfd`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

History: notification-descriptor determinism was documented and tested in https://github.com/rrnewton/hermit/pull/549 (commit 3c49d197b4, "Document and verify notification fd determinism").

### Open questions

- Should `ssi_utime` and `ssi_stime` in `SIGCHLD` records be rewritten to Hermit's virtual CPU time, or accepted as host data? That depends on the policy for CPU-time values elsewhere, which this body does not decide.

### 2026-10-03 host strace evidence

Test host: Linux 7.1.3-0_fbk0_0_g295ad0959f34 x86_64; strace 6.12. A Python ctypes script issued raw `syscall(282, ...)` calls under strace (excerpt):

```text
signalfd(-1, [USR1], 8)         = 5
signalfd(-1, 0x7f0e9bc27120, 4) = -1 EINVAL (Invalid argument)
signalfd(-1, 0x7f0e9bc27120, 16) = -1 EINVAL (Invalid argument)
signalfd(0, [USR1], 8)          = -1 EINVAL (Invalid argument)
signalfd(-1, NULL, 8)           = -1 EFAULT (Bad address)
--- SIGCHLD {si_signo=SIGCHLD, si_code=CLD_EXITED, si_pid=3356695, si_uid=212630, si_status=0, si_utime=1 /* 0.01 s */, si_stime=0} ---
```

Descriptor 0 in the fourth line was the script's standard input, not a signalfd. The last line shows a `SIGCHLD` carrying host CPU time.
