---
schema: syscall-pages/v1
syscall: signalfd4
nr_x86_64: 289
linux_determinism: deterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+signalfd4&body=Discussion+for+the+%60signalfd4%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsignalfd4.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60signalfd4%60%2C+or+in+what+the+page+says%3F%0A
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

`signalfd4(fd, mask, sizemask, flags)` either creates a descriptor from which the caller can `read` its pending signals in `mask`, or, when `fd` is an existing signalfd, replaces that descriptor's mask. Detcore handles the call at `detcore/src/syscalls/files.rs:4213`. The kernel does the work through `record_or_replay`, and Detcore then registers the returned descriptor in its mirror as a new signalfd. Creation is handled correctly. Updating the mask of an existing signalfd is not: Detcore overwrites its record of the descriptor as if it were new.

Manual reference: Linux man-pages signalfd(2).

## Linux semantics

With `fd == -1`, the kernel creates a new signalfd and returns the lowest free descriptor. When `fd` is an existing signalfd, the kernel replaces its mask and returns `fd`. In that case `flags` is ignored: the description keeps the close-on-exec and nonblocking state it was created with.

`SIGKILL` and `SIGSTOP` are removed from the mask without an error. `sizemask` must equal the kernel's `sigset_t` size, 8 bytes on x86_64; glibc's `signalfd` wrapper passes that value.

Checks happen in this order: `flags` (`EINVAL`), then `sizemask` (`EINVAL`), then copying `mask` (`EFAULT`), then `fd`. For `fd`, a closed descriptor is `EBADF` and an open descriptor that is not a signalfd is `EINVAL`.

Reading from the descriptor:
- A `read` returns one or more 128-byte `signalfd_siginfo` records. Each record consumes a pending signal that is in the mask, either one sent to the calling thread or one sent to the whole process.
- With no such signal pending, the `read` waits, or fails with `EAGAIN` when the descriptor is nonblocking.
- To keep the signals from being delivered the normal way, the caller must block them with `sigprocmask`.

The older `signalfd(fd, mask, sizemask)` (nr 282) has no flags argument and acts like `signalfd4` with `flags = 0`.

### Flags and options

- F1: `fd == -1`: create a new signalfd with `mask` and return the lowest free descriptor
- F2: `fd` is an existing signalfd: replace its mask and return `fd`. `flags` is ignored, and the description keeps its close-on-exec and `O_NONBLOCK` state.
- F3: `fd` is open but not a signalfd: `EINVAL`. `fd` is not open: `EBADF`.
- F4: `SFD_CLOEXEC`: the new descriptor is close-on-exec (creation only)
- F5: `SFD_NONBLOCK`: the new description is `O_NONBLOCK`, so a `read` with no matching pending signal fails with `EAGAIN` (creation only)
- F6: `sizemask` other than 8: `EINVAL`
- F7: `SIGKILL` or `SIGSTOP` in `mask`: dropped without an error
- F8: `mask` points to unreadable memory: `EFAULT`
- F9: any other bit in `flags`: `EINVAL`
- F10: resource errors: `EMFILE`, `ENFILE`, `ENODEV`, `ENOMEM`
- F11: `signalfd` (nr 282): no flags argument; the same as `signalfd4` with `flags = 0`
- F12: reading: each 128-byte `signalfd_siginfo` record consumes one pending signal in the mask, sent either to the thread or to the process. Without `SFD_NONBLOCK`, a `read` waits until such a signal is pending.

## Determinism analysis

`linux_determinism: deterministic`. The result of `signalfd4` itself depends only on its arguments and on the calling process's descriptor table: which descriptor is created, or which mask is installed, and which errno comes back. Hermit serializes changes to the table.

What a later `read` returns, and when a waiting `read` wakes up, depends on when signals are sent. A signal from outside the guest, or one raised by the host kernel, such as `SIGCHLD` from a child exiting or a timer firing, arrives at a time the guest does not control. That nondeterminism comes from `read` and from signal delivery, not from this call. The body records it because the descriptor exists to expose it (F12).

## Hermit design

Current behaviour (hermit 239ca43d3092): signalfd records are reconciled against Detcore's signal model instead of passing through bare. A read runs as a nonblockable descriptor call and each returned record is put through the child-exit ledger and the discarded-at-send filtering (`read_signalfd`, `detcore/src/syscalls/files.rs:10804`); a read whose records were all dropped duplicates reads again or answers 0, and a faulted copy-out that would silently drop a record is refused by name rather than losing it. Where the mask covers a signal with ledger state, reads are taken one record per kernel read so the reconciliation order is the kernel's dequeue order. This supersedes the summary claim below that signal delivery and blocking reads are simply left to the kernel outside the scheduler.

Target policy: `model`. The kernel holds the signal queue and the mask. Detcore keeps the descriptor in its mirror so that `fcntl`, `dup` and the nonblocking-I/O dispatch know what it is.

- The call goes to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:4218`). The order of checks and every errno (F3, F6 to F10) therefore come from the kernel.
- The returned descriptor is registered with `add_fd` as `FdType::Signalfd`, with the guest's `SFD_CLOEXEC|SFD_NONBLOCK` bits (`detcore/src/syscalls/files.rs:4219` to `detcore/src/syscalls/files.rs:4227`). Unlike event counters and sockets, Detcore does not make a signalfd secretly nonblocking: the handler never calls `maybe_set_nonblocking_fd`.
- A `read` goes through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:1854`). A nonblocking signalfd gets one attempt. A blocking one takes the blocking path and waits in the kernel through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:250`), so Detcore cannot turn that wait into scheduler-owned polling.
- `signalfd` uses the same handler through a conversion that sets `flags = 0` (`detcore/src/lib.rs:2576`) (F11).

Creation (F1, F4, F5) is realized. `refuse` would break programs built on `signalfd`, such as systemd-style event loops. `passthrough` would leave the descriptor missing from the mirror, and `dup` and `fcntl` on it would then fail with `EBADF`; the comment at `detcore/src/syscalls/io.rs:1151` describes the same failure for epoll descriptors. So `model` is the right target, and the gaps below must be closed before the code fully realizes it.

Gaps:

- **Mask update (F2) is modelled as a creation.** The handler has no case for `fd != -1`. It calls `add_fd` on the descriptor the kernel returned, which is `fd` itself. That allocates a new open-file id and overwrites the slot (`detcore/src/tool_local.rs:751`, `detcore/src/tool_local.rs:747`), and it takes the close-on-exec and nonblocking state from this call's `flags`, which Linux ignores. From reading the code, this has three effects:
  - A `dup` copy made before the update keeps the old record, so the two descriptors no longer share one description in the mirror.
  - `fcntl(F_GETFL)` answers from the new record (`detcore/src/syscalls/files.rs:3097`). If the signalfd was created with `SFD_NONBLOCK` and updated with `flags = 0`, Detcore hides an `O_NONBLOCK` the kernel still has.
  - In the reverse case, a blocking signalfd updated with `SFD_NONBLOCK`, Detcore believes the descriptor is nonblocking in the kernel. It then sends a `read` straight through (`detcore/src/syscalls/helpers.rs:773`), and the kernel waits outside the scheduler's view.
  The test program `tests/c/signalfd_create.c:12` avoids mask updates because such an update "diverges across backends". These effects are hypotheses from the code; T3 checks them.
- **Target (owner decision 14):** a blocking signalfd becomes physically nonblocking behind the guest's back, so a blocking `read` is a non-blocking probe plus a scheduler wait. `handle_signalfd4` adds `SFD_NONBLOCK` to the injected flags under the condition `handle_eventfd2` uses (`use_nonblocking_sockets()` and not record/replay), registers the guest's own flags and calls `maybe_set_nonblocking_fd` (`detcore/src/syscalls/files.rs:4189-4207`), and `F_SETFL` adds `FdType::Signalfd` to its forced list (`detcore/src/syscalls/files.rs:3113`). The read then takes `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:776-778`) instead of `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:250`). The mask-update fix above is a prerequisite: an update that rebuilds the record would lose the internal flag. Not `refuse`: blocking signalfd reads are the descriptor's normal use.
- **Blocking reads wait in the kernel.** Whether a blocking `read` gives the same result on every `--verify` run, when the signal comes from another guest thread, has not been measured (T2).
- **Record and replay.** The Recorder's subscription list (`hermit-cli/src/recorder.rs:384` to `hermit-cli/src/recorder.rs:474`) contains `read` but neither `signalfd4` nor `signalfd`, so creation runs live again on replay. Replay of a signalfd `read` is not covered by any test named here.

## Recommended tests

- T1 (covers F1, F4, F5): create two signalfds with `signalfd(-1, mask, SFD_NONBLOCK|SFD_CLOEXEC)` and with `flags = 0`. Check the descriptor numbers, `F_GETFD` and `F_GETFL`, and that a `read` with nothing pending returns `EAGAIN` on the nonblocking one.
  Existing candidate: `tests/c/signalfd_create.c` (cell `c-programs/signalfd-create`, `tests/e2e/manifests/c-programs.yaml:13656`, verify ci on ptrace). It creates two signalfds with `SFD_NONBLOCK|SFD_CLOEXEC` (`tests/c/signalfd_create.c:44`, `tests/c/signalfd_create.c:51`), does not read from them, and does not use `flags = 0`.
- T2 (covers F5, F7, F12): block `SIGUSR1`, raise it, and `read` one `signalfd_siginfo` with the right `ssi_signo`, with the same result on every `--verify` run. Repeat with a blocking signalfd and the signal sent from a second guest thread with `tgkill`. Put `SIGKILL` in the mask and check that the call still succeeds.
  Existing candidate: `signalfd_delivery_is_deterministic` (`detcore/tests/misc/notification_fds.rs:312`, cargo test target `tests_misc`). It reads a raised signal through a nonblocking signalfd (`detcore/tests/misc/notification_fds.rs:120`). It does not try a blocking `read` or `SIGKILL` in the mask.
- T3 (covers F2): create a signalfd with `SFD_NONBLOCK`, `dup` it, then update the mask with `signalfd(fd, newmask, 0)`. Check:
  - the call returns `fd`;
  - `F_GETFL` on both descriptors still shows `O_NONBLOCK`;
  - a signal in the new mask can be read through the `dup` copy;
  - a `read` with nothing pending returns `EAGAIN` and does not wait.
  Then the reverse: create a blocking signalfd, update it with `SFD_NONBLOCK`, and check that `F_GETFL` does not show `O_NONBLOCK` and that the run does not hang. If Hermit differs from Linux, file it against `detcore/src/syscalls/files.rs:4219`.
- T4 (covers F3, F6, F8, F9): with a raw `syscall(SYS_signalfd4, ...)`, check `EINVAL` when `fd` is a pipe, `EBADF` when `fd` is closed, `EINVAL` for `sizemask = 4`, `EFAULT` for `mask = (void *)1`, and `EINVAL` for `flags = 1`.
- T5 (covers F10, F11): with `RLIMIT_NOFILE` lowered to the number of open descriptors, creation returns `EMFILE`. A raw `syscall(SYS_signalfd, -1, &mask, 8)` returns a blocking descriptor that is not close-on-exec.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (creation realized; mask update on an existing signalfd is modelled as a new descriptor, a suspected defect T3 checks); linux_determinism deterministic for the call itself, read timing tracked under read; 12 flag behaviours and 5 tests listed; open questions recorded; no coverage token, tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decision 14 (blocking signalfd becomes internally nonblocking, read is probe plus scheduler wait); target_policy unchanged; no new F/T ids; removed the secretly-nonblocking question.

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated for signalfd record reconciliation against the signal model; code facts also refreshed at conversion.

## Discussion

[Start a discussion about `signalfd4`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+signalfd4&body=Discussion+for+the+%60signalfd4%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsignalfd4.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60signalfd4%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `signalfd4`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should the handler treat `fd != -1` as a mask update: keep the existing record, open-file id and flags, and register nothing new? That matches Linux and seems the obvious fix. It changes shared descriptor bookkeeping, though, so it should be confirmed by T3 and reviewed before it is called settled. This is a code defect to file against `detcore/src/syscalls/files.rs:4219`, not a policy choice; no GitHub issue was filed from this read-only task.
