---
schema: syscall-pages/v1
syscall: ioctl
nr_x86_64: 16
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ioctl&body=Discussion+for+the+%60ioctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fioctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ioctl%60%2C+or+in+what+the+page+says%3F%0A
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

`ioctl` sends a device- or file-type-specific request to the object behind a file descriptor. Hermit models the requests that change file-descriptor state (`FIOCLEX`, `FIONCLEX`, `FIONBIO`), refuses `SIOCETHTOOL` with `ENODEV`, returns logical time for the socket receive-timestamp requests, and forwards every other request to the kernel, recording its decoded output for replay.

## Linux semantics

`ioctl(fd, request, argp)` dispatches on the type of the open file: the VFS handles a few generic requests (`FIOCLEX`, `FIONCLEX`, `FIONBIO`, `FIOASYNC`, `FIOQSIZE`, `FICLONE` and the `FS_IOC_*` family), and everything else goes to the file's driver, socket family or terminal line discipline. The result is 0 or a request-specific non-negative value, or `-1` with `errno`. `argp` is an integer or a pointer to a request-specific structure that the kernel reads, writes, or both.

- Most request numbers encode a direction (none, read, write, read-write) and an argument size in their `_IOC` bits. Older requests, including most socket (`0x89xx`) and terminal (`0x54xx`) numbers, carry no such bits, so the layout of `argp` is known only from the request's documentation.
- State changed through `ioctl` belongs either to the file descriptor (close-on-exec), to the open file description shared by every duplicate (`O_NONBLOCK`), or to the object behind it (terminal settings, socket options, file extents), and is visible to every process that shares it.
- An unknown request for the file's type fails with `ENOTTY`; some drivers use `EINVAL`.

### Flags and options

- F1: `FIOCLEX` and `FIONCLEX` set and clear the close-on-exec flag of the descriptor; `argp` is ignored.
- F2: `FIONBIO` reads an `int` from `argp` and sets `O_NONBLOCK` on the open file description when it is non-zero, clears it when it is zero; a NULL or unmapped `argp` gives `EFAULT`.
- F3: `FIONREAD` (also `TIOCINQ` and `SIOCINQ`) writes the number of bytes available to read on a pipe, socket, terminal or regular file.
- F4: `SIOCGSTAMP` writes a `struct timeval` and `SIOCGSTAMPNS` a `struct timespec` holding the receive time of the last packet read from a socket; the time64 encodings `SIOCGSTAMP_NEW` and `SIOCGSTAMPNS_NEW` have the same layout on x86_64. With no packet received yet the call gives `ENOENT`; on a descriptor that is not a socket it gives `ENOTTY`.
- F5: `SIOCETHTOOL` runs an ethtool command through a `struct ifreq` whose `ifr_data` points to a nested command structure, for example `ETHTOOL_GLINK`, and reports host network-interface state.
- F6: the other network-interface requests on a socket (`SIOCGIFCONF`, `SIOCGIFFLAGS`, `SIOCGIFADDR`, `SIOCGIFHWADDR`, `SIOCGIFINDEX`, `SIOCGIFMTU` and their setters) read or change the configuration of the network namespace.
- F7: `FICLONE` and `FICLONERANGE` share extents from a source file into the destination; they give `EXDEV` across filesystems, `EINVAL` for unaligned ranges, and `EBADF` when the destination is not open for writing.
- F8: other file requests (`FS_IOC_GETFLAGS`, `FS_IOC_SETFLAGS`, `FIGETBSZ`, `FIBMAP`, `FS_IOC_FIEMAP`, `FIOQSIZE`) read or change inode flags and the on-disk layout.
- F9: terminal requests (`TCGETS` and the `TCSETS*` family, `TIOCGWINSZ`, `TIOCSWINSZ`, `TIOCGPGRP`, `TIOCSPGRP`, `TIOCSCTTY`, `TIOCNOTTY`) read or change terminal settings, window size and job-control state; on a descriptor that is not a terminal they give `ENOTTY`. `isatty(3)` is a `TCGETS` call.
- F10: requests owned by other kernel objects: block devices (`BLKGETSIZE64`), namespace files (`NS_GET_NSTYPE`, `NS_GET_USERNS`), pidfds, perf event, userfaultfd and seccomp-notify descriptors, and character devices such as `/dev/kvm`, `/dev/net/tun` or a GPU.
- F11: a request the file's type does not implement gives `ENOTTY` (or `EINVAL` from some drivers).
- F12: a descriptor that is not open gives `EBADF`.
- F13: a pointer argument that is NULL or points to unmapped memory gives `EFAULT`; a request that both reads and writes may have changed object state before the copy-out fails.
- F14: the request number's `_IOC` direction and size bits describe the argument; a legacy request number without them gives no such description.
- F15: host-config failure: `FICLONE` and `FICLONERANGE` give `EOPNOTSUPP` when the host filesystem behind the files has no reflink support (for example ext4 or tmpfs, unlike btrfs or XFS with reflink).

## Determinism analysis

`linux_determinism` is `nondeterministic`. `ioctl` is a gateway to many kernel objects, and several of its requests return a value that differs between runs no matter how the guest behaves:

- Time. The F4 requests return the wall-clock receive time of a packet.
- Progress of other parties. F3 counts the bytes another thread, process or remote peer has written so far.
- Host configuration. F5 and F6 report the host's network interfaces and link state; F7 and F8 depend on the filesystem's type, features and block layout; F9 reports the controlling terminal's settings and window size, which the user can change while the program runs; F10 reports devices and kernel objects outside the guest.

The call repeats only for requests on descriptor-local state (F1, F2), for error paths (F11, F12, F13), and for requests on objects the guest itself created and drives in a fixed order.

### Cross-host variation

- Filesystem type behind the guest's files: whether `FICLONE` and `FICLONERANGE` succeed or give `EOPNOTSUPP` (F7, F15), and which inode flags `FS_IOC_GETFLAGS` reports (F8).
- Filesystem block size and on-disk layout: the values `FIGETBSZ`, `FIBMAP` and `FS_IOC_FIEMAP` return (F8). Detcore forwards these requests unchanged (`detcore/src/syscalls/files.rs:3232`).

## Hermit design

Target policy: `model`. Detcore keeps its own copy of the descriptor state that `ioctl` can change, replaces the time-valued requests with logical time, refuses the request whose output it cannot capture, and leaves the rest to the kernel.

- Dispatch. A guard arm sends the socket-timestamp requests to their own handler (`detcore/src/lib.rs:2423-2427`); every other request goes to `handle_ioctl` (`detcore/src/lib.rs:2428`, defined at `detcore/src/syscalls/files.rs:3190`). `ioctl` is charged `IO_NS` of logical time (`detcore/src/syscall_time.rs:16`, `detcore/src/syscall_time.rs:84-87`).
- F5. `SIOCETHTOOL` returns `ENODEV` without reaching the kernel (`detcore/src/syscalls/files.rs:3198-3201`), because link state belongs to the host network namespace. Record and replay use the same rule through `deterministic_ioctl_error` (`hermit-cli/src/event.rs:25-35`), which also matches the raw number `0x8946`.
- F1 and F2. `FIOCLEX` and `FIONCLEX` map to a close-on-exec change, and `FIONBIO` reads its `int`, returning `EFAULT` for a NULL pointer (`detcore/src/syscalls/files.rs:3202-3206`). When Detcore already keeps a socket, pipe or eventfd physically nonblocking for its scheduler, and record/replay is off, `FIONBIO` only updates the logical flag and returns 0 without reaching the kernel (`detcore/src/syscalls/files.rs:3217-3229`), so a guest that clears `O_NONBLOCK` still gets blocking behaviour from the scheduler. Otherwise the request is forwarded, and only after success does Detcore update its descriptor record (`detcore/src/syscalls/files.rs:3232-3242`).
- F4. `handle_socket_timestamp_ioctl` (`detcore/src/syscalls/socket_timestamp_ioctl.rs:74`) recognizes the legacy and time64 numbers (`detcore/src/syscalls/socket_timestamp_ioctl.rs:24-26`, `detcore/src/syscalls/socket_timestamp_ioctl.rs:48-54`). With logical time on, which is the default and is turned off with `--no-virtualize-time` (`detcore-model/src/config.rs:70-72`), it returns `ENOTTY` for a descriptor that is not a socket, `ENOENT` when no receive has been observed, and otherwise writes the logical time of the last receive (`detcore/src/syscalls/socket_timestamp_ioctl.rs:97-115`). That time is stored on the open file description, so duplicates share it, by `observe_socket_receive` (`detcore/src/syscalls/io.rs:1453-1463`), which `recvmsg`, the generic socket receive path and `recvmmsg` call (`detcore/src/syscalls/io.rs:1508`, `detcore/src/syscalls/io.rs:1797`, `detcore/src/syscalls/io.rs:1847`). With logical time off, the request is rewritten to its time64 number, whose `_IOC` bits describe the 16-byte output, and forwarded so record/replay can capture it (`detcore/src/syscalls/socket_timestamp_ioctl.rs:80-94`).
- Everything else (F3, F6 to F14) is forwarded unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:3232`). The kernel produces the error codes of F11, F12 and F13.
- Record and replay. The recorder (`hermit-cli/src/recorder/fs.rs:729`, reached from `hermit-cli/src/recorder.rs:602`) records the deterministic error first (`hermit-cli/src/recorder/fs.rs:736-739`), then injects the call (`hermit-cli/src/recorder/fs.rs:741`). It records a file snapshot for F7 (`hermit-cli/src/recorder/fs.rs:747`), a plain return value for F1 and F2 (`hermit-cli/src/recorder/fs.rs:809-813`), the decoded output bytes for any request whose output the Reverie decoder can read (`hermit-cli/src/recorder/fs.rs:814-816`), and a plain return value otherwise (`hermit-cli/src/recorder/fs.rs:817-818`). The replayer (`hermit-cli/src/replayer/fs.rs:1048`, reached from `hermit-cli/src/replayer.rs:487`) mirrors this: deterministic error (`hermit-cli/src/replayer/fs.rs:1055-1056`), F7 snapshot (`hermit-cli/src/replayer/fs.rs:1061-1063`), F1 and F2 re-applied as side effects (`hermit-cli/src/replayer/fs.rs:1143-1148`), read-direction output written back to guest memory (`hermit-cli/src/replayer/fs.rs:1149-1152`), and a plain return value otherwise (`hermit-cli/src/replayer/fs.rs:1153-1155`).

Why `model` and not the alternatives: `passthrough` would let Detcore's descriptor record drift from the kernel's close-on-exec and nonblocking state, would let a guest break the scheduler's nonblocking invariant, and would expose packet receive times. `emulate` is not possible for the open set of drivers in F10. `refuse` would break `isatty` and every program that sets `FIONBIO`.

What Detcore does not model today, and so leaves as host input in run mode: the byte count of F3 when the writer is outside the guest, the network-interface configuration of F6, the filesystem features and layout of F7 and F8, the terminal state of F9 when a guest descriptor is a real terminal, and the device state of F10. Whether that is acceptable is the open question under Additional information.

## Recommended tests

- T1 (covers F1, F2): set and clear `FIOCLEX` and `FIONBIO`, checking each change with `fcntl(F_GETFD)` and `fcntl(F_GETFL)`.
  Existing coverage: `tests/c/ioctl_fioclex.c` (cell `c-programs/ioctl-fioclex`, `tests/e2e/manifests/c-programs.yaml:2470`, verify ci on ptrace) uses `FIONBIO` on a pipe at lines 27 and 33 and `FIOCLEX`/`FIONCLEX` on `/dev/null` at lines 44 and 49. Still missing: `FIONBIO` with a NULL `argp` (`EFAULT`), and `FIONBIO` clear on a socket followed by a read that must block until a peer thread writes, which exercises the logical path.
- T2 (covers F3): `FIONREAD` on a pipe after a write of known length.
  Existing coverage: `tests/c/ioctl_fionread.c` (cell `c-programs/ioctl-fionread`, `tests/e2e/manifests/c-programs.yaml:12139`, verify ci on ptrace), `FIONREAD` at line 61. Still missing: `FIONREAD` on a socketpair written by a second guest thread, checked identical across the two runs of `hermit run --verify`.
- T3 (covers F4): receive a datagram and read `SIOCGSTAMP`, `SIOCGSTAMPNS`, repeated reads and a read through a duplicate.
  Existing coverage: `tests/c/socket_ioctl_timestamp.c` (cell `c-programs/socket-ioctl-timestamp`, `tests/e2e/manifests/c-programs.yaml:8836`, verify ci on ptrace) at lines 72-74, 91, 104-106 and 123, but the ptrace `guest_args` run only the `v4-us` mode, so the `SIOCGSTAMPNS` and IPv6 paths are not in CI. Still missing: `ENOENT` before any receive, `ENOTTY` on a pipe, `EFAULT` for a NULL pointer, and the time64 request numbers through a raw `syscall(SYS_ioctl, ...)`.
- T4 (covers F5): `SIOCETHTOOL` with `ETHTOOL_GLINK` on `lo` returns `ENODEV`.
  Existing coverage: `tests/c/ioctl_siocethtool.c` line 35 (cell `c-programs/ioctl-siocethtool`, `tests/e2e/manifests/c-programs.yaml:2535`, verify ci on ptrace).
- T5 (covers F6): `SIOCGIFCONF` and `SIOCGIFFLAGS` on `lo`, printing the results, under `hermit run --verify`.
- T6 (covers F7): `FICLONE` from a source file into a new file, then into a read-only descriptor, printing each result and `errno`.
  Related existing test: `tests/c/record_replay_file_state.c` uses `FICLONE` at lines 145 and 188, but its verify cell (`c-programs/record-replay-file-state`, `tests/e2e/manifests/c-programs.yaml:6977`) has ptrace `ci: false` at line 6996 and records a determinism failure at line 7005.
- T7 (covers F8): `FIGETBSZ` and `FS_IOC_GETFLAGS` on a file the guest created, under `hermit run --verify`.
- T8 (covers F9, F11): `TIOCGPGRP`, `TCGETS` and `TIOCGWINSZ` on a pipe and on `/dev/null` return `ENOTTY`; on a pseudo-terminal pair from `posix_openpt`, `TIOCSWINSZ` then `TIOCGWINSZ` round-trips.
  Existing partial coverage: `tests/c/dbt_copied_tiocgpgrp.c` line 24 expects `ENOTTY` from `TIOCGPGRP` on stderr (cell `c-programs/dbt-copied-tiocgpgrp`, `tests/e2e/manifests/c-programs.yaml:720`, verify ci on ptrace).
- T9 (covers F10): `NS_GET_NSTYPE` on `/proc/self/ns/uts` returns `CLONE_NEWUTS`, under `hermit run --verify`.
- T10 (covers F12, F13, F14): with raw `syscall(SYS_ioctl, ...)`, check `EBADF` on a closed descriptor, `EFAULT` for `FIONREAD` with a NULL pointer, and `ENOTTY` for an unused `_IOR` number on a regular file; then run a read-direction request under `hermit record` and `hermit replay` and check that replay writes the recorded bytes.
- U1 (covers F15; host-config): the CI host's filesystem decides whether reflink is supported, so CI cannot produce both the success of F7 and the `EOPNOTSUPP` of F15. Hermit forwards the kernel's answer unchanged (`detcore/src/syscalls/files.rs:3232`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (Detcore models FIOCLEX/FIONCLEX/FIONBIO, refuses SIOCETHTOOL with ENODEV, returns logical time for SIOCGSTAMP*, forwards the rest through record/replay); linux_determinism nondeterministic (timestamps, peer progress, host terminal, network and device state); 14 flag behaviours and 10 tests listed; open question on forwarding host-state requests in run mode, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type decides reflink support and inode flags, filesystem block size and layout decide FIGETBSZ/FIBMAP/FIEMAP); F7 split, new F15 (EOPNOTSUPP without reflink) with U1 host-config; no open question resolved
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T4 covered by tests/c/ioctl_siocethtool.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `ioctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ioctl&body=Discussion+for+the+%60ioctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fioctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ioctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `ioctl`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.


### Open questions

- Owner policy: requests other than F1, F2, F4 and F5 reach the kernel unchanged in run mode (`detcore/src/syscalls/files.rs:3208`, `detcore/src/syscalls/files.rs:3232`), so terminal state (`TCGETS`, `TIOCGWINSZ` when a guest descriptor is a real terminal), network-interface configuration (`SIOCGIF*`), filesystem flags and layout (`FS_IOC_*`, `FIEMAP`) and device requests return host state. Should Hermit (a) keep an allowlist and refuse the rest deterministically, as it already does for `SIOCETHTOOL`; (b) virtualize the common ones, for example a fixed terminal answer for guest stdio; or (c) accept them as environment input outside the determinism guarantee? The choice decides what F6, F8, F9 and F10 must show before this body can close.
- Code-reading question, not measured: the recorder stores output bytes only when the Reverie decoder can read them (`hermit-cli/src/recorder/fs.rs:814-818`), while the replayer chooses by the request's direction bits (`hermit-cli/src/replayer/fs.rs:1149-1155`). A request whose number carries read-direction bits but which the decoder does not describe may be recorded as a plain return and replayed as an output event. T10 should confirm or rule this out.
