---
schema: syscall-pages/v1
syscall: recvfrom
nr_x86_64: 45
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+recvfrom&body=Discussion+for+the+%60recvfrom%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frecvfrom.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60recvfrom%60%2C+or+in+what+the+page+says%3F%0A
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

`recvfrom` receives bytes from a socket and can report the sender's address. Hermit models it:
- Detcore owns the blocking, through physically nonblocking sockets and scheduler-ordered retries.
- It records a logical receive time for `SIOCGSTAMP`.
- It canonicalizes host socket identities in `NETLINK_SOCK_DIAG` and `NETLINK_ROUTE` dumps.

The per-call flags go to the kernel uninspected.

## Linux semantics

`recvfrom(sockfd, buf, len, flags, src_addr, addrlen)` copies up to `len` bytes of queued data into `buf` and returns the count.

- Source address. If `src_addr` is set, the kernel writes the sender's address, truncated to `*addrlen`, and stores the real length back in `*addrlen`.
- Message boundaries. A datagram is consumed whole: excess bytes are discarded unless `MSG_PEEK` is passed.
- Blocking. A stream read returns whatever is queued, up to `len`. On a blocking socket with nothing queued, the call waits.
- Return 0 means an orderly shutdown by a stream peer, or a zero-length datagram.
- `recv(2)` has no x86_64 syscall of its own: glibc issues `recvfrom` with a null address. Python's `socket.recv()` takes this path.
- Side effects:
  - consuming queued data, which frees buffer space and wakes blocked senders;
  - setting the socket's last-receive timestamp, which `SIOCGSTAMP`/`SIOCGSTAMPNS` read.

### Flags and options

- F1: `src_addr`/`addrlen`.
  - Null, or a buffer.
  - The address is truncated to `*addrlen`, and `*addrlen` is set to the full length.
  - A connected stream socket reports no address.
  - An unnamed `AF_UNIX` peer reports only the family.
- F2: `MSG_DONTWAIT`: nonblocking for this call only; `EAGAIN`/`EWOULDBLOCK` when nothing is queued.
- F3: `MSG_PEEK`: returns data without consuming it.
- F4: `MSG_WAITALL`: on stream sockets, waits for the full `len`. It can still return less on a signal, an error, a disconnect, or a change of message type.
- F5: `MSG_TRUNC`:
  - on datagram, raw and netlink sockets, returns the real datagram length even when it exceeds `len`;
  - on TCP, discards the data.
- F6: `MSG_OOB`: reads urgent data.
- F7: `MSG_ERRQUEUE`: reads from the socket error queue. Without ancillary data, the error details are lost.
- F8: Return 0: an orderly shutdown by a stream peer, a zero-length datagram, or a request with `len` 0.
- F9: Blocking versus nonblocking descriptor (`O_NONBLOCK`/`SOCK_NONBLOCK`).
- F10: `SO_RCVTIMEO`: a blocking receive gives up with `EAGAIN` after the timeout.
- F11: Signal interruption: `EINTR`, or a restart under `SA_RESTART` when no receive timeout is set.
- F12: Errors:
  - `EBADF`, `ENOTSOCK`, `EFAULT`, `ENOTCONN`, `ECONNRESET` and `EINVAL`;
  - `ECONNREFUSED`, a pending ICMP error on a connected UDP socket.
- F13: `recv(2)` lowering: glibc implements `recv` as `recvfrom` with a null `src_addr`.
- F14: Netlink dumps. Replies on `NETLINK_SOCK_DIAG` and `NETLINK_ROUTE` sockets carry host socket inode numbers and live interface counters.
- F15: Receive timestamp side state: the kernel records when the last packet arrived, and `SIOCGSTAMP`/`SIOCGSTAMPNS` read it.

## Determinism analysis

The bytes a receive returns depend on what the peer has sent so far, and so on the interleaving of sender and receiver. Under one fixed schedule, with every peer inside the guest, the call repeats.

### Nondeterminism sources

- Peer progress. How much is queued decides the return count, short reads, and whether the call would block (F4, F8, F9).
- External peers and the network: data arrival, ICMP errors (F12), and the peer's source address (F1), including a kernel-chosen ephemeral port.
- Host-assigned identities. Socket inode numbers and interface statistics in netlink dumps (F14).
- Host time. Kernel receive timestamps (F15) and real-time receive timeouts (F10).
- The arrival time of signals (F11).

`linux_determinism: conditional`.

## Hermit design

`target_policy: model`. Dispatch is at detcore/src/lib.rs:2693-2705.
- If `sock_diag_reply_fd` holds, the call goes to `handle_sock_diag_recvfrom` (lib.rs:2702-2704).
- Otherwise it goes to `handle_socket_receive(guest, s, s.fd(), true)` (lib.rs:2705).

The comment at lib.rs:2693-2700 records why the netlink case is checked for `recvfrom`: until the sanitizer was shared, four of the five receive syscalls returned raw host socket inode numbers, and `socket.recv()` alone skipped it.

- `handle_socket_receive` (detcore/src/syscalls/io.rs:1785-1800) runs `execute_nonblockable_fd_syscall`. When `virtualize_time` is set and the call delivered a packet (a result above 0, or 0 here), it then calls `observe_socket_receive` (io.rs:1795-1798). That function stores the thread's logical time as the receive timestamp of the open file description (io.rs:1453-1463). `handle_socket_timestamp_ioctl` reads that value back for `SIOCGSTAMP`/`SIOCGSTAMPNS` (detcore/src/syscalls/socket_timestamp_ioctl.rs:73-102).
- `handle_sock_diag_recvfrom` (io.rs:1634-1651) receives the data, then calls `sanitize_sock_diag_segments` (io.rs:1569-1632). That function zeroes live interface counters for `NETLINK_ROUTE`, or canonicalizes socket identities for `NETLINK_SOCK_DIAG` (io.rs:1611-1619). The predicate requires `virtualize_metadata` (io.rs:1529-1535), and netlink sockets are marked when created (files.rs:3796-3800).
- `Recvfrom` counts `EAGAIN`/`EWOULDBLOCK` as "would have blocked" (helpers.rs:1290-1301).

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

Why `model`. Detcore owns when a blocked receive resumes, and what time the guest reads back for it. The kernel keeps the data. `passthrough` would let a blocking receive stall the sequentialized scheduler and would leak host inode numbers and timestamps.

Existing ptrace coverage at f4055eb9: `c-programs/so-incoming-cpu-udp4` (tests/e2e/manifests/c-programs.yaml:8604, `verify` `ci: true` on ptrace) receives one UDP byte with `recv`, which glibc lowers to `recvfrom` (tests/c/so_incoming_cpu_udp4.c:56).

What remains: hypotheses from reading the code, not yet measured.
- H1 (F2). In run mode, on a logically blocking socket, `MSG_DONTWAIT` with nothing queued makes the kernel return `EAGAIN`. Hermit classifies that as "would block" and retries, so the guest waits instead of seeing `EAGAIN`.
- H2 (F4). `MSG_WAITALL` on the physically nonblocking socket returns whatever is queued, so a stream receive can come back short where Linux would wait.
- H3 (F10). `SO_RCVTIMEO` cannot fire in run mode, because the retry loop has no deadline.
- H4 (F3, F15). A `MSG_PEEK` that returns data also updates the logical receive timestamp (io.rs:1796). Whether Linux updates the socket stamp on a peek has not been compared.
- H5 (F1). A sender that bound implicitly reports a host-chosen source port.

Target (owner decision 14, with decision 13 for the timeout): a guest-blocking `recvfrom` waits in Detcore's scheduler, never in the kernel, through a nonblocking probe plus a scheduler wait. Run mode already does this through `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:254`, retry loop `retry_nonblocking_syscall` at `detcore/src/syscalls/helpers.rs:1414`). Three parts are missing. First, the cases that still block in the kernel while holding the turn, through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:225` and `detcore/src/syscalls/helpers.rs:250`): no scheduler, record and replay, and a descriptor Detcore cannot classify. Second, the probe must follow the guest's per-call flags, not only the descriptor's mode: `MSG_DONTWAIT` returns the kernel's `EAGAIN` at once (F2, H1), and `MSG_WAITALL` keeps waiting until `len` bytes have arrived (F4, H2). Third, a deadline: with `SO_RCVTIMEO` set, the wait ends with `EAGAIN`, or with the bytes already received, once that much virtual time has passed (F10, H3). `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) already takes a logical deadline for poll, epoll, futex and signal waits, but nothing in `detcore/src` reads `SO_RCVTIMEO` or `SO_SNDTIMEO` today (see the `setsockopt` body). `recvfrom` stays `model`, not `refuse`, because nearly every networked program receives on sockets. For F1 and H5, owner decision 12 makes a sender's implicit-bind port an allocator port (see the `sendto` body), so `src_addr` repeats with no change in this handler.

## Recommended tests

- T1 (covers F1, F8, F13): UDP and `AF_UNIX` datagram pairs. Cover:
  - `recvfrom` with a full-size address buffer, a truncating one, and a null one;
  - an unnamed `AF_UNIX` peer;
  - a zero-length datagram returning 0;
  - EOF after the peer shuts down an `AF_UNIX` stream;
  - `recv` showing up as `recvfrom(` in the `--strace` trace.

  Run under `--verify`.
- T2 (covers F2, F9): on a logically blocking socketpair with nothing queued, `recvfrom` with `MSG_DONTWAIT` must return `EAGAIN` and must not wait (H1). The same call on a `SOCK_NONBLOCK` pair returns `EAGAIN`.
- T3 (covers F3, F5, F6): `MSG_PEEK` then a normal receive return the same bytes. `MSG_TRUNC` on a UDP datagram longer than the buffer returns the full length. `MSG_OOB` reads the urgent byte sent on loopback TCP.
- T4 (covers F4): a sender thread writes 8 bytes as two 4-byte sends at different logical times. `MSG_WAITALL` with `len` 8 must return 8 (H2).
- T5 (covers F7, F12): `MSG_ERRQUEUE` after a UDP send to a closed loopback port. Also:
  - `ECONNREFUSED` on the next `recvfrom` of a connected UDP socket;
  - `EBADF`, `ENOTSOCK` (on a pipe) and `EFAULT`;
  - `ENOTCONN` on an unconnected stream socket.
- T6 (covers F10, F11): receive timeouts and signals:
  - With `SO_RCVTIMEO` set and no data, `recvfrom` returns `EAGAIN` in logical time instead of hanging (H3).
  - A blocked `recvfrom` interrupted by a signal returns `EINTR` without `SA_RESTART` and restarts with it.
- T7 (covers F14): `recvfrom` (and `recv`) on a `NETLINK_SOCK_DIAG` dump and on an `RTM_GETLINK` dump. Inode numbers and counters must match across two runs.
- T8 (covers F15): `recvfrom` on UDP loopback, then `SIOCGSTAMP`. The value must be logical time and identical across runs, including after a `MSG_PEEK` (H4).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_socket_receive: nonblockable retry plus a logical receive timestamp; netlink diag and route dumps sanitized via handle_sock_diag_recvfrom). linux_determinism stays conditional. 15 flags and 8 tests. The MSG_DONTWAIT, MSG_WAITALL, SO_RCVTIMEO, MSG_PEEK-timestamp and source-port hypotheses are under Open questions for measurement. Stays todo; no coverage recorded.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 12, 13 and 14 applied (Target paragraph: blocking receives wait under the scheduler in every mode, honour MSG_DONTWAIT and MSG_WAITALL, and end at a virtual-time SO_RCVTIMEO deadline; src_addr ports repeat through the sender's allocator port); no new F/T ids; no open questions removed (H1-H5 are measurements)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `recvfrom`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+recvfrom&body=Discussion+for+the+%60recvfrom%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frecvfrom.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60recvfrom%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `recvfrom`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

These questions need a measurement, not an owner decision, and none of them changes the `target_policy` choice. Each is a hypothesis under Hermit design and names the test that answers it.

- H1: on a logically blocking socket in run mode, does `recvfrom` with `MSG_DONTWAIT` and nothing queued wait instead of returning `EAGAIN`? (T2)
- H2: does `MSG_WAITALL` return short on the physically nonblocking socket? (T4)
- H3: does `SO_RCVTIMEO` never fire in run mode? (T6)
- H4: does a `MSG_PEEK` update the receive timestamp the way Linux does? (T8)
- H5: does a sender that bound implicitly report a host-chosen source port? (T1, together with sendto T7)
