---
schema: syscall-pages/v1
syscall: recvmsg
nr_x86_64: 47
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+recvmsg&body=Discussion+for+the+%60recvmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frecvmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60recvmsg%60%2C+or+in+what+the+page+says%3F%0A
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

`recvmsg` receives one message into a scatter list of buffers, together with the sender's address, ancillary data and result flags. Hermit models it:
- Detcore owns the blocking, through physically nonblocking sockets and scheduler-ordered retries;
- it replaces host timestamps in `SCM_TIMESTAMP*` messages with logical time;
- it records a logical receive time for `SIOCGSTAMP`;
- it canonicalizes host socket identities in netlink dumps.

The per-call flags and other ancillary messages go to the kernel uninspected.

## Linux semantics

`recvmsg(sockfd, msg, flags)` reads a `struct msghdr` and fills:
- the buffers named by `msg_iov`;
- the source address in `msg_name`, updating `msg_namelen`;
- ancillary messages in `msg_control`, updating `msg_controllen`;
- result flags in `msg_flags`.

It returns the number of data bytes received. Datagram, stream, blocking and EOF behaviour follow `recvfrom`. Ancillary data is produced by kernel state, and some of it is host-specific:
- descriptors passed with `SCM_RIGHTS` become new descriptors in the receiver;
- `SCM_CREDENTIALS` carries the sender's pid, uid and gid;
- `SCM_TIMESTAMP*` carries the host arrival time.

The kernel allows the control buffer to overlap the `msghdr` itself.

### Flags and options

- F1: `msg_name`/`msg_namelen`: the source address, truncated to `msg_namelen`, with the real length written back. A null `msg_name` is allowed.
- F2: `msg_iov`/`msg_iovlen`: scatter into several buffers. More than `IOV_MAX` entries gives `EMSGSIZE`.
- F3: `msg_control`/`msg_controllen`: the ancillary buffer. It may be null or zero-length. The length written back covers only what fit.
- F4: `msg_flags` output:
  - `MSG_TRUNC`, when the datagram was longer than the buffers;
  - `MSG_CTRUNC`, when the control buffer was too small;
  - `MSG_EOR`, `MSG_OOB` and `MSG_ERRQUEUE`.
- F5: `SCM_RIGHTS` reception: descriptors are installed at the lowest free numbers. `MSG_CMSG_CLOEXEC` sets close-on-exec on them. Descriptors that do not fit are closed, and `MSG_CTRUNC` is set.
- F6: `SCM_CREDENTIALS`, with `SO_PASSCRED` set: the sender's pid, uid and gid.
- F7: `SCM_TIMESTAMP`, `SCM_TIMESTAMPNS` and `SCM_TIMESTAMPING`, in both the old and the new (time64) layouts, enabled by `SO_TIMESTAMP*`: host arrival times.
- F8: Protocol ancillary data:
  - `IP_PKTINFO`, `IP_TTL`, `IP_TOS` and `IPV6_PKTINFO`;
  - `UDP_GRO` and `SO_RXQ_OVFL`;
  - `SO_INCOMING_CPU`-related state.
- F9: `MSG_ERRQUEUE`: extended errors (`sock_extended_err`), zerocopy completions and transmit timestamps from the error queue.
- F10: `MSG_DONTWAIT`: nonblocking for this call only.
- F11: `MSG_PEEK`: returns data without consuming it.
- F12: `MSG_WAITALL`: on stream sockets, waits for the full length.
- F13: `MSG_TRUNC` as an input flag: returns the real datagram length.
- F14: `MSG_OOB`: reads urgent data.
- F15: Return 0: an orderly stream shutdown or a zero-length datagram.
- F16: Blocking versus nonblocking descriptor.
- F17: `SO_RCVTIMEO`: a blocking receive gives up with `EAGAIN` after the timeout.
- F18: Signal interruption: `EINTR`, or a restart under `SA_RESTART`.
- F19: Errors:
  - `EBADF` and `ENOTSOCK`;
  - `EFAULT`, for a null or bad `msg`, bad iovecs, or a bad control buffer;
  - `ENOTCONN`, `ECONNREFUSED`, `ECONNRESET`, `EINVAL` and `EMSGSIZE`.
- F20: A control buffer that overlaps the `msghdr` itself.
- F21: Netlink dumps. `NETLINK_SOCK_DIAG` and `NETLINK_ROUTE` replies in `msg_iov` carry host socket inode numbers and live interface counters.
- F22: Receive timestamp side state, readable through `SIOCGSTAMP`/`SIOCGSTAMPNS`.

## Determinism analysis

Which data arrives, and when, depends on the peer and the schedule. On top of the data, `recvmsg` returns ancillary values that differ between hosts and between runs even under one fixed schedule:
- host arrival timestamps (F7);
- descriptor numbers, which depend on the receiver's table (F5);
- credentials (F6);
- host socket identities in netlink dumps (F21).

### Nondeterminism sources

- Peer progress, which decides counts, short reads, `MSG_TRUNC` and blocking (F4, F12, F15, F16).
- External peers, ICMP errors (F9, F19), and kernel-chosen source ports (F1).
- Host clocks: `SCM_TIMESTAMP*` (F7), `SIOCGSTAMP` (F22), and real-time receive timeouts (F17).
- Host identities: socket inodes and interface counters (F21), and credential ids (F6).
- The arrival time of signals (F18).

`linux_determinism: conditional`.

## Hermit design

`target_policy: model`. Dispatch: `Syscall::Recvmsg(s) => self.handle_recvmsg(guest, s)` at detcore/src/lib.rs:2708. `handle_recvmsg` is at detcore/src/syscalls/io.rs:1465-1518 and handles the cases in this order:
1. A netlink diagnostics or route socket goes to `handle_sock_diag_recvmsg` (io.rs:1478-1480, 1673-1695). That function snapshots the iovec list before the receive and sanitizes the reply in place (io.rs:1569-1632). The same predicate covers `recvfrom`, `read`, `readv` and `recvmmsg` (io.rs:1529-1535).
2. Without `virtualize_time`, the call is a plain `execute_nonblockable_fd_syscall` (io.rs:1482-1484).
3. With a null `msg`, a null control pointer or a zero control length, it goes to `handle_socket_receive`, which records the logical receive time (io.rs:1486-1499, 1785-1800).
4. Otherwise:
   - It snapshots the `msghdr` before the receive, because the control buffer may overlap the header (io.rs:1491-1494).
   - It clamps the control length to `MAX_CONTROL_BYTES` (64 KiB, io.rs:221, 1500), and checks the control buffer is readable before a datagram is consumed (io.rs:1503-1505).
   - After the receive it records the logical time (io.rs:1507-1508) and rereads the control buffer.
   - If the buffer holds any `SCM_TIMESTAMP*` message (io.rs:215-220, 272), it rewrites those timestamps to logical time (io.rs:1511-1516, 330).

The receive time stored on the open file description is what `SIOCGSTAMP` returns (detcore/src/syscalls/socket_timestamp_ioctl.rs:73-102). `Recvmsg` counts `EAGAIN`/`EWOULDBLOCK` as "would have blocked" (helpers.rs:1304-1315).

`SCM_RIGHTS` descriptors are installed by the kernel; Detcore registers them only when it first sees them. With `discover_live_file_metadata` set, `with_detfd` discovers an unknown descriptor from the live process (detcore/src/tool_local.rs:2559-2568). The type comes from `fstat` and the flags from `F_GETFL`, and the descriptor's `flock` state is marked unknown (tool_local.rs:696-733). Without discovery, the lookup fails and the next nonblockable call on that descriptor takes the blocking path (helpers.rs:212-226).

Shared send/receive path at hermit f4055eb9 (run mode with `sequentialize_threads`, the default for `hermit run`):

- Socket creation makes every guest socket physically nonblocking while the guest-visible flags stay as requested. `socket` adds `SOCK_NONBLOCK` and records the guest's own `SOCK_NONBLOCK | SOCK_CLOEXEC` bits (detcore/src/syscalls/files.rs:3772-3787, gated by `use_nonblocking_sockets` at detcore-model/src/config.rs:872-874). `socketpair` does the same (files.rs:3827-3828), and so does `accept4` for the connection it returns (files.rs:4531-4536).
- `execute_nonblockable_fd_syscall` (detcore/src/syscalls/helpers.rs:202) reads the descriptor's logical and physical nonblocking bits and picks a strategy (helpers.rs:745-778):
  - logically blocking on a physically nonblocking socket gives `NonblockizeRetry`;
  - logically nonblocking gives `PassThru`, a single kernel call (helpers.rs:268-275);
  - both blocking gives `Blocking`.
- `NonblockizeRetry` requests the scheduler resource `InternalIOPolling` and calls `retry_nonblocking_syscall` (helpers.rs:254-267). The helper re-issues the call at scheduler-chosen points while the result is `EAGAIN` or `EWOULDBLOCK` (helpers.rs:1501-1530). Any other result is returned to the guest (helpers.rs:1531-1534). A signal that arrives while the call waits returns `ERESTARTSYS` (helpers.rs:1476-1484, default at helpers.rs:953-954). The kernel still moves the bytes; Detcore decides when a blocked call is retried, so a blocking wait becomes a deterministic scheduling point.
- If Detcore cannot classify the descriptor, the call runs through the blocking path so the kernel's errno precedence is kept (helpers.rs:212-226).
- In record/replay mode, sockets from `socket` stay physically blocking (files.rs:3758-3769), while `socketpair` endpoints are still made physically nonblocking (the condition at files.rs:3827 does not check for record/replay). Sockets are never treated as container-internal, because Hermit cannot yet tell a socket between two guests from one to a host peer (helpers.rs:782-797). The call therefore goes through `record_or_replay_blocking` with the `BlockingExternalIO` resource (helpers.rs:241-250, 111-120). `tests/rust/shutdown.rs:26-31` skips record mode because socketpair guests currently hang there.
- No `MSG_*` flag passed by the guest is inspected anywhere in `detcore/src`; `MSG_TRUNC` appears only in comments. `setsockopt` is forwarded unchanged (files.rs:3868-3875), so `SO_RCVTIMEO` and `SO_SNDTIMEO` are never modelled.

Why `model`. Detcore owns the parts of `recvmsg` that differ between runs: when a blocked receive resumes, the time values written to the guest, and host socket identities. The kernel keeps the data and the descriptor transfer. `passthrough` would leak host timestamps and inodes, and would let a blocking receive stall the scheduler.

Existing ptrace coverage at f4055eb9. All of these cells have `verify` `ci: true` on ptrace unless noted.
- `c-programs/socket-timestamp-timeval` (tests/e2e/manifests/c-programs.yaml:9022): UDP loopback with `SO_TIMESTAMP`; `recvmsg` and an `SCM_TIMESTAMP` check (tests/c/socket_timestamp_timeval.c:48-57).
- `c-programs/recvmsg-scm-rights-mmap` (c-programs.yaml:7225): receiving `SCM_RIGHTS` (tests/c/recvmsg_scm_rights_mmap.c:69). Its replay cell is not in CI. `record_sqlite_memory_query` (hermit-cli/tests/record_replay.rs:1874-1885) documents the record/replay event misalignment that `recvmsg` with `SCM_RIGHTS` used to cause.
- `c-programs/socket-timestamp-edge-cases` (c-programs.yaml:8901): a truncated control buffer (tests/c/socket_timestamp_edge_cases.c:66-68) and an aliased control buffer (:110-112). This cell has `verify` ptrace CI disabled: the recorded reason is a crash-error, "verify exited with status 125 before producing a terminal comparison" (c-programs.yaml:8919-8930).

What remains: hypotheses from reading the code, not yet measured.
- H1 (F10). `MSG_DONTWAIT` on a logically blocking socket is retried rather than returned as `EAGAIN`.
- H2 (F12). `MSG_WAITALL` can return short on the physically nonblocking socket.
- H3 (F17). `SO_RCVTIMEO` never fires in run mode.
- H4 (F3, F7). Timestamp messages that lie past the 64 KiB clamp keep host time.
- H5 (F6). `SCM_CREDENTIALS` values are not virtualized by Detcore.
- H6 (F9). Timestamps delivered through `MSG_ERRQUEUE` go through the same rewrite only if they use the `SCM_TIMESTAMP*` types listed at io.rs:215-220.
- The open crash-error on the edge-cases cell needs a diagnosis before F20 can be counted as covered.

Target (owner decision 14, with decision 13 for the timeout and the timestamps): a guest-blocking `recvmsg` waits in Detcore's scheduler, never in the kernel, through a nonblocking probe plus a scheduler wait. Run mode already does this through `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:254`, retry loop `retry_nonblocking_syscall` at `detcore/src/syscalls/helpers.rs:1414`). Missing: the cases that still block in the kernel while holding the turn, through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:225` and `detcore/src/syscalls/helpers.rs:250`): no scheduler, record and replay, and a descriptor Detcore cannot classify; per-call flags, where `MSG_DONTWAIT` returns the kernel's `EAGAIN` at once (F10, H1) and `MSG_WAITALL` keeps waiting for the full length (F12, H2); and a deadline, where `SO_RCVTIMEO` ends the wait with `EAGAIN` or the bytes already received once that much virtual time has passed (F16, F17, H3). `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) already takes a logical deadline for poll, epoll, futex and signal waits, but nothing in `detcore/src` reads `SO_RCVTIMEO` or `SO_SNDTIMEO` today (see the `setsockopt` body). Under decision 13 every `SCM_TIMESTAMP*` value the guest receives is virtual time, including messages past the `MAX_CONTROL_BYTES` clamp (`detcore/src/syscalls/io.rs:221`; H4) and timestamps from the error queue (F9, H6). `recvmsg` stays `model`, not `refuse`, because nearly every networked program receives on sockets.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F13): on UDP and `AF_UNIX` datagram pairs:
  - `msg_name` full-size, truncated and null;
  - a three-buffer scatter;
  - a datagram longer than the buffers, setting `MSG_TRUNC` in `msg_flags`, and with `MSG_TRUNC` as an input flag returning the full length;
  - a control buffer too small for `SCM_RIGHTS`, setting `MSG_CTRUNC`.

  Run under `--verify`.
- T2 (covers F5): receive 1 and 3 descriptors with `SCM_RIGHTS`. The descriptor numbers must match across runs, and `MSG_CMSG_CLOEXEC` must set `FD_CLOEXEC`. Then use a received socket for a blocking `read`, with and without metadata discovery. Extends `tests/c/recvmsg_scm_rights_mmap.c`.
- T3 (covers F6, F8): with `SO_PASSCRED` set, the received `SCM_CREDENTIALS` pid and uid must match the guest's `getpid`/`getuid` and be identical across runs (H5). Also `IP_PKTINFO` and `IP_TTL` on UDP loopback.
- T4 (covers F7, F20, F22): timestamp ancillary data and aliasing:
  - `SCM_TIMESTAMP`, `SCM_TIMESTAMPNS` and `SCM_TIMESTAMPING`, old and new layouts, must equal logical time and match `SIOCGSTAMP`;
  - a control buffer larger than 64 KiB, with a timestamp near its end (H4);
  - an overlapping control buffer.

  Requires the crash-error on `c-programs/socket-timestamp-edge-cases` to be diagnosed.
- T5 (covers F9, F19): `MSG_ERRQUEUE` after a UDP send to a closed loopback port, reading `sock_extended_err` (H6). Also:
  - `EFAULT` for a null `msg` and for a bad `msg_control`;
  - `EBADF` and `ENOTSOCK`;
  - `ENOTCONN` on an unconnected stream;
  - more than `IOV_MAX` entries, expecting `EMSGSIZE`.
- T6 (covers F10, F16): on a logically blocking socketpair with nothing queued, `MSG_DONTWAIT` must return `EAGAIN` (H1). The same call on a `SOCK_NONBLOCK` pair returns `EAGAIN`.
- T7 (covers F11, F12, F14, F15): receive flags and EOF on stream and datagram pairs:
  - `MSG_PEEK` then a consuming receive return the same bytes;
  - `MSG_WAITALL` across two delayed 4-byte sends must return 8 (H2);
  - `MSG_OOB` on TCP;
  - EOF after the peer calls `shutdown(SHUT_WR)`, and a zero-length datagram.
- T8 (covers F17, F18): with `SO_RCVTIMEO` set and no data, `recvmsg` returns `EAGAIN` in logical time (H3). A signal during a blocked `recvmsg` gives `EINTR`, or a restart under `SA_RESTART`.
- T9 (covers F21): `recvmsg` on a `NETLINK_SOCK_DIAG` dump and on an `RTM_GETLINK` dump, with the reply scattered over two iovecs. Inode numbers and counters must match across runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_recvmsg: nonblockable retry, SCM_TIMESTAMP* rewritten to logical time within a 64 KiB control clamp, netlink dumps sanitized). linux_determinism stays conditional. 22 flags and 9 tests. The MSG_DONTWAIT, MSG_WAITALL, SO_RCVTIMEO, clamp, SCM_CREDENTIALS and MSG_ERRQUEUE hypotheses are under Open questions, as is the socket-timestamp-edge-cases crash-error. Stays todo; no coverage recorded.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 13 and 14 applied (Target paragraph: blocking receives wait under the scheduler in every mode, honour MSG_DONTWAIT and MSG_WAITALL, end at a virtual-time SO_RCVTIMEO deadline, and every received timestamp is virtual time); no new F/T ids; no open questions removed (H1-H6 and the crash diagnosis are measurements)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `recvmsg`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+recvmsg&body=Discussion+for+the+%60recvmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frecvmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60recvmsg%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `recvmsg`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

These questions need a measurement, not an owner decision, and none of them changes the `target_policy` choice. Each is a hypothesis under Hermit design and names the test that answers it.

- H1: on a logically blocking socket, does `MSG_DONTWAIT` wait instead of returning `EAGAIN`? (T6)
- H2: does `MSG_WAITALL` return short? (T7)
- H3: does `SO_RCVTIMEO` never fire in run mode? (T8)
- H4: do timestamp messages past the 64 KiB control clamp keep host time? (T4)
- H5: do the received `SCM_CREDENTIALS` values match the guest's virtual ids? (T3)
- H6: are transmit timestamps from `MSG_ERRQUEUE` rewritten? (T5)
- What causes the crash-error ("verify exited with status 125") recorded for `c-programs/socket-timestamp-edge-cases` (tests/e2e/manifests/c-programs.yaml:8929-8930)? (T4)
