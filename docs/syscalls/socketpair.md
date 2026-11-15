---
schema: syscall-pages/v1
syscall: socketpair
nr_x86_64: 53
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+socketpair&body=Discussion+for+the+%60socketpair%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsocketpair.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60socketpair%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`socketpair(2)` creates two connected, unnamed AF_UNIX sockets. Detcore forwards it to the kernel with `SOCK_NONBLOCK` added, registers both descriptors with the flags the guest asked for, and schedules later I/O on them itself. Record mode does not record the call; replay runs it again.

## Linux semantics

`socketpair(domain, type, protocol, sv)` creates two sockets connected to each other and writes their descriptors to `sv[0]` and `sv[1]`. Linux supports this only for AF_UNIX. The descriptors are the two lowest free numbers. Neither socket has a name, and each reports the creating process's credentials as its peer's (`SO_PEERCRED`). `SOCK_NONBLOCK` and `SOCK_CLOEXEC` in `type` apply to both ends.

If writing `sv` fails, the kernel discards both new sockets and returns `EFAULT`. No descriptor is installed. Data ordering, short writes and readiness on the pair are properties of the later `read`/`write`/`sendmsg`/`recvmsg` calls.

### Flags and options

- F1: `domain`: only AF_UNIX (AF_LOCAL) creates a pair. A known family without pair support, such as AF_INET, gives `EOPNOTSUPP`; an unknown family gives `EAFNOSUPPORT`.
- F2: `SOCK_STREAM`: a connected byte-stream pair.
- F3: `SOCK_DGRAM`, and `SOCK_RAW`, which AF_UNIX treats as `SOCK_DGRAM`: a datagram pair that preserves message boundaries.
- F4: `SOCK_SEQPACKET`: a connection-oriented pair that preserves message boundaries.
- F5: `SOCK_NONBLOCK`: both ends start with `O_NONBLOCK`.
- F6: `SOCK_CLOEXEC`: both ends start with `FD_CLOEXEC`.
- F7: invalid arguments: a `protocol` other than 0 or `PF_UNIX` gives `EPROTONOSUPPORT`; an unknown socket type gives `ESOCKTNOSUPPORT`; a stray flag bit in `type` gives `EINVAL`.
- F8: `sv` not writable: `EFAULT`, and no descriptors are created.
- F9: descriptor and memory limits: `EMFILE` when the process has fewer than two free descriptors under `RLIMIT_NOFILE`; `ENFILE` or `ENOMEM` from system limits.
- F10: results: the two lowest free descriptor numbers, an unnamed address on both ends (`getsockname` returns only the family), and `SO_PEERCRED` naming the creator.

## Determinism analysis

`linux_determinism: conditional`, kept. The descriptor numbers, flags and connection are fixed by the arguments and the process's descriptor table. The socket inode numbers, visible through `fstat` and `/proc/self/fd` as `socket:[N]`, come from a host-wide counter. The `SO_PEERCRED` pid is the creator's pid in its namespace. Both repeat only if the run controls them.

### Nondeterminism sources

- Host-assigned socket inode numbers
- The creator's pid and credentials, reported through `SO_PEERCRED`
- Later I/O on the pair: which writer's data arrives first, and how much a stream write accepts, depend on scheduling and on the host's socket buffer sysctls. These belong to the I/O calls but are why this pair is a common source of guest nondeterminism.

## Hermit design

Target policy: `model`. The kernel creates and owns the pair. Detcore keeps both ends physically nonblocking so that no guest thread sleeps inside the kernel, and it presents the guest's own flags.

What the code does now:

- The typed dispatch arm at `detcore/src/lib.rs:2551` calls `handle_socketpair` (`detcore/src/syscalls/files.rs:3822`).
- When threads are sequentialized and `debug_externalize_sockets` is off, the call is sent with `SOCK_NONBLOCK` added (`detcore/src/syscalls/files.rs:3827`-`3831`).
- After the call, Detcore reads both descriptors from `sv` (`detcore/src/syscalls/files.rs:3833`-`3835`) and registers each as `FdType::Socket` with the logical flags `type & (SOCK_NONBLOCK | SOCK_CLOEXEC)` (`detcore/src/syscalls/files.rs:3838`-`3855`). `F_GETFL` therefore shows `O_NONBLOCK` only when the guest asked for it (F5).
- Both ends are then marked physically nonblocking (`maybe_set_nonblocking_fd`, `detcore/src/syscalls/helpers.rs:695`; called at `detcore/src/syscalls/files.rs:3857`-`3858`).
- Record mode does not record `socketpair`: the recorder has no arm for it and injects it live (`hermit-cli/src/recorder.rs:648`), and the replayer re-executes it (`hermit-cli/src/replayer.rs:575`). Descriptor numbering is deterministic, so replay gets the same numbers.

Why `model`: code facts rule out `passthrough` for a determinized syscall, and plain forwarding would let reads block inside the kernel, out of the scheduler's sight. `emulate` would need Detcore to implement AF_UNIX sockets. `refuse` would break shells, process supervisors and most IPC libraries.

Gaps between the code and `model`:

- **Socketpair ends count as external.** `syscall_targets_internal_fd` treats only pipes as container-internal (`detcore/src/syscalls/helpers.rs:797`-`808`). Its comment at `detcore/src/syscalls/helpers.rs:792` says sockets stay external because Hermit cannot tell internal sockets from external ones. Both ends of a `socketpair` are internal by construction, and Detcore knows that at `detcore/src/syscalls/files.rs:3838`.
- **Possible spurious `EAGAIN` in record mode.** This is a hypothesis from the code, not a measurement at this SHA. In record and replay modes, I/O on a non-internal descriptor goes through the blocking external path (`detcore/src/syscalls/helpers.rs:239`-`250`). `handle_socket` keeps sockets blocking in those modes (`detcore/src/syscalls/files.rs:3758`), but `handle_socketpair`'s `SOCK_NONBLOCK` condition at `detcore/src/syscalls/files.rs:3827` does not check `recordreplay_modes`. If record mode sequentializes threads, a guest's blocking read on an empty end would reach the kernel on a nonblocking socket and get `EAGAIN`, which Linux would not return. T8 checks this.

## Recommended tests

- T1 (covers F2, F5, F6): `socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK)`: both ends have `FD_CLOEXEC` and `O_NONBLOCK`, and `SO_TYPE`, `SO_DOMAIN` and `SO_ACCEPTCONN` are right.
  Existing candidate: `tests/c/socketpair_flags.c` (call at line 29; cell `c-programs/socketpair-flags`, `tests/e2e/manifests/c-programs.yaml:15509`, verify ci on ptrace). It checks `O_NONBLOCK` only on the first end (lines 46-49).
- T2 (covers F2, F5): a pair created without `SOCK_NONBLOCK` must not show `O_NONBLOCK`, `F_SETFL` must round-trip, and data from several writer threads must arrive in the same order in every run.
  Existing candidate: `tests/c/ipc_determinism.c` (`socketpair_order` at lines 207-240; flag check at line 212; cell `c-programs/ipc-determinism`, `tests/e2e/manifests/c-programs.yaml:2603`, verify ci on ptrace).
- T3 (covers F3): a datagram pair preserves message boundaries and passes descriptors with `SCM_RIGHTS`. Add a `SOCK_RAW` pair, which must behave as `SOCK_DGRAM`.
  Existing candidates: `tests/c/shutdown_socketpair.c` (datagram pair at line 48; cell at `tests/e2e/manifests/c-programs.yaml:14410`) and `tests/c/recvmsg_scm_rights_mmap.c` (datagram pair at line 28; cell at `tests/e2e/manifests/c-programs.yaml:7225`).
- T4 (covers F4): a `SOCK_SEQPACKET` pair preserves record boundaries, and a short read truncates the record identically in every run. No `SOCK_SEQPACKET` socketpair test exists in `tests/c` at this SHA.
- T5 (covers F1, F7): `EOPNOTSUPP` for AF_INET, `EAFNOSUPPORT` for family 255, `EPROTONOSUPPORT` for protocol 1, `ESOCKTNOSUPPORT` for type 99, and `EINVAL` for a stray high flag bit.
- T6 (covers F8, F9): an unwritable `sv` returns `EFAULT`, and the next `open` still gets the lowest free number, proving no descriptor leaked. With `RLIMIT_NOFILE` leaving one free slot, the call returns `EMFILE`.
- T7 (covers F10): print both descriptor numbers, both `getsockname` lengths and the `SO_PEERCRED` pid. All must be identical across `--verify` runs.
  Existing partial coverage: `tests/c/sockname_unnamed.c` checks the family-only `getsockname` length on a pair (line 46; cell at `tests/e2e/manifests/c-programs.yaml:14472`).
- T8 (covers F2, F3): under `hermit record` and replay, a thread blocks reading an empty stream end and a second thread writes later. The read must return the data, never `EAGAIN`, and replay must match the recording.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (Detcore forces physical SOCK_NONBLOCK and keeps the guest's logical flags); linux_determinism conditional kept; F1-F10 inventoried, T1-T8 proposed; gaps: socketpair ends are classified external, possible record-mode EAGAIN (hypothesis, T8).

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `socketpair`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+socketpair&body=Discussion+for+the+%60socketpair%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsocketpair.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60socketpair%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `socketpair`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should `syscall_targets_internal_fd` (`detcore/src/syscalls/helpers.rs:797`) treat descriptors created by `socketpair` as container-internal, as it does pipes?
- Should `handle_socketpair` skip the forced `SOCK_NONBLOCK` in record and replay modes, as `handle_socket` does at `detcore/src/syscalls/files.rs:3758`, or should record mode route socketpair I/O through the internal polling path?
