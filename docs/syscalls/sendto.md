---
schema: syscall-pages/v1
syscall: sendto
nr_x86_64: 44
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sendto&body=Discussion+for+the+%60sendto%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsendto.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sendto%60%2C+or+in+what+the+page+says%3F%0A
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

`sendto` sends bytes on a socket, optionally naming the destination address. Hermit models it: the kernel moves the data, and Detcore decides when a call that would block is retried, by making guest sockets physically nonblocking and retrying them at points the scheduler chooses. Detcore does not inspect the per-call flags.

## Linux semantics

`sendto(sockfd, buf, len, flags, dest_addr, addrlen)` queues `len` bytes from `buf` on the socket and returns the number of bytes accepted.

- Destination. On a connectionless socket, `dest_addr` names the peer. On a connection-mode socket it is ignored or rejected. The first send on an unbound `AF_INET` or `AF_INET6` datagram socket binds it to an ephemeral port that the kernel chooses.
- Datagrams are sent whole or not at all; one too large fails with `EMSGSIZE`. A stream send on a blocking socket waits until every byte is queued, unless a signal or timeout cuts it short, in which case it returns a partial count. On a nonblocking socket it returns a short count or `EAGAIN`.
- Broken stream. Sending on a stream whose peer has shut down or closed raises `SIGPIPE` and fails with `EPIPE`, unless `MSG_NOSIGNAL` is passed.
- `send(2)` has no x86_64 syscall of its own: glibc issues `sendto` with a null address.
- Side effects visible to the guest:
  - buffer space consumed in the peer's receive queue;
  - wakeups of blocked receivers;
  - an implicit local address, readable through `getsockname`, or seen by the peer as the source address.

### Flags and options

- F1: `dest_addr`/`addrlen`.
  - Null on a connected socket.
  - Explicit on a connectionless socket.
  - Explicit on a connected stream socket, where `AF_UNIX` returns `EISCONN`.
  - Null on an unconnected datagram socket gives `EDESTADDRREQ`; on an unconnected stream socket, `ENOTCONN`.
  - A bad length gives `EINVAL`.
- F2: `MSG_DONTWAIT`: nonblocking for this call only; `EAGAIN`/`EWOULDBLOCK` when the send buffer is full.
- F3: `MSG_NOSIGNAL`: suppresses `SIGPIPE` on a broken stream; `EPIPE` is still returned.
- F4: `MSG_MORE`: more data follows. TCP delays the push and UDP builds one datagram from several calls.
- F5: `MSG_OOB`: urgent data on stream sockets that support it (TCP); `EOPNOTSUPP` elsewhere.
- F6: `MSG_CONFIRM` and `MSG_DONTROUTE`: hints to the IP layer about neighbours and routing; their effect depends on the protocol.
- F7: `MSG_EOR`: ends a record on `SOCK_SEQPACKET`.
- F8: `MSG_FASTOPEN`: TCP Fast Open; connects an unconnected TCP socket and carries data in the SYN. Availability depends on the host's `net.ipv4.tcp_fastopen` setting.
- F9: `MSG_ZEROCOPY`: requires `SO_ZEROCOPY`; completion notices arrive on the socket error queue.
- F10: Blocking versus nonblocking descriptor (`O_NONBLOCK`/`SOCK_NONBLOCK`). A blocking stream send waits until all `len` bytes are queued; a nonblocking one returns a short count or `EAGAIN`.
- F11: `SO_SNDTIMEO`: a blocking send gives up after the timeout, returning a partial count or `EAGAIN`.
- F12: Signal interruption: `EINTR` if nothing was sent, a partial count otherwise. With `SA_RESTART` the call restarts, unless a send timeout is set.
- F13: Datagram size: `EMSGSIZE` when the datagram cannot be sent whole; a zero-length datagram is legal and is delivered.
- F14: Implicit autobind: an unbound `AF_INET`/`AF_INET6` datagram socket gets a kernel-chosen ephemeral port on its first send.
- F15: Errors:
  - `EBADF`, `ENOTSOCK`, `EFAULT`, `EPIPE`, `ENOBUFS` and `ENOMEM`;
  - `EACCES`, for broadcast without `SO_BROADCAST`;
  - `ECONNREFUSED`, a pending ICMP error on a connected UDP socket;
  - `ECONNRESET`.
- F16: `send(2)` lowering: glibc implements `send` as `sendto` with a null `dest_addr`.

## Determinism analysis

Between two endpoints inside the guest, the result of `sendto` depends on how much buffer space the peer has freed, and that depends on the interleaving of sender and receiver. Under one fixed schedule the call repeats: same return value, same short counts, same errors.

### Nondeterminism sources

- Peer progress. How much the receiver has read decides whether a stream send completes, returns a short count, or would block (F10, F2).
- External peers. Network delivery and ICMP errors (`ECONNREFUSED`, `ECONNRESET`) come from outside the container.
- Kernel-chosen ephemeral ports. The implicit bind (F14) picks a host port, which the peer observes as the source address.
- Host configuration. Buffer sizes (`SO_SNDBUF` defaults and autotuning) and `tcp_fastopen` change where short counts and `EAGAIN` occur.
- Real-time timeouts (`SO_SNDTIMEO`) and the arrival time of signals (F11, F12).

`linux_determinism: conditional`: the call is deterministic for a fixed peer state and schedule, but that state comes from other threads, other processes and the network.

## Hermit design

`target_policy: model`. Dispatch: `Syscall::Sendto(s) => self.handle_sendrecv(guest, s)` at detcore/src/lib.rs:2709. `handle_sendrecv` (detcore/src/syscalls/io.rs:1408-1418) calls only `execute_nonblockable_fd_syscall`. `Sendto` counts `EAGAIN`/`EWOULDBLOCK` as "would have blocked" (helpers.rs:1334-1345).

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

Why `model`. Hermit owns the part of `sendto` that makes it nondeterministic: when a blocked send resumes relative to other threads. The kernel keeps the buffer state, and Detcore orders every retry. `passthrough` would be wrong: a blocking send would block the sequentialized scheduler. `emulate` is unnecessary: Detcore has no reason to reimplement socket buffers.

Existing ptrace coverage at f4055eb9, which uses `sendto` as setup rather than as the object of the test:
- `c-programs/socket-timestamp-timeval` (tests/e2e/manifests/c-programs.yaml:9022): an unbound sender calls `sendto` to a bound UDP loopback receiver (tests/c/socket_timestamp_timeval.c:33).
- `c-programs/so-incoming-cpu-udp4` (c-programs.yaml:8604): `sendto` at tests/c/so_incoming_cpu_udp4.c:46.

Both cells have `verify` `ci: true` on ptrace.

What remains: hypotheses from reading the code, not yet measured.
- H1 (F2). In run mode, on a logically blocking socket, `MSG_DONTWAIT` with a full buffer makes the kernel return `EAGAIN`. Hermit classifies that as "would block" and retries, so the guest waits instead of seeing `EAGAIN`.
- H2 (F10). Hermit has no socket counterpart of the pipe write loop. A blocking stream send larger than the free buffer space therefore returns the kernel's short count from the physically nonblocking socket, where Linux would wait to queue every byte.
- H3 (F11). `SO_SNDTIMEO` cannot fire in run mode: the kernel socket is `O_NONBLOCK`, and Hermit retries without a deadline.
- H4 (F14). The implicit autobind is not routed through the deterministic port allocator. Explicit port-0 binds are routed through it (files.rs:4116-4131). The peer therefore sees a host-chosen source port.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. Today `handle_sendrecv` (`detcore/src/syscalls/io.rs:1409`) forwards an unbound socket to the kernel, which autobinds it at a random port in `inet_autobind`, called from `inet_send_prepare` (F14, H4); a `MSG_FASTOPEN` send on an unbound TCP socket gets its port through the connect path, `inet_hash_connect` (F8).

Target (owner decision 14, with decision 13 for the timeout): a guest-blocking `sendto` waits in Detcore's scheduler, never in the kernel, through a nonblocking probe plus a scheduler wait. Run mode already does this through `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:254`, retry loop `retry_nonblocking_syscall` at `detcore/src/syscalls/helpers.rs:1414`). Four parts are missing. First, the cases that still block in the kernel while holding the turn, through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:225` and `detcore/src/syscalls/helpers.rs:250`): no scheduler, record and replay, and a descriptor Detcore cannot classify. Second, `MSG_DONTWAIT` must return the kernel's `EAGAIN` at once (F2, H1); no code in `detcore/src` reads it today. Third, a blocking stream send keeps probing and waiting until every byte is queued, as Linux does, and returns a short count only for a signal, an error or a timeout (F10, F12, H2). Fourth, a deadline: with `SO_SNDTIMEO` set, the wait ends with the partial count or `EAGAIN` once that much virtual time has passed (F11, H3). `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) already takes a logical deadline for poll, epoll, futex and signal waits, but nothing in `detcore/src` reads `SO_RCVTIMEO` or `SO_SNDTIMEO` today (see the `setsockopt` body). `sendto` stays `model`, not `refuse`, because nearly every networked program sends on sockets.

## Recommended tests

- T1 (covers F1, F13, F16): `AF_UNIX` datagram and UDP loopback pairs. Cover:
  - `sendto` with a destination to a bound receiver;
  - `sendto` with a null destination after `connect`;
  - `EDESTADDRREQ` for a null destination on an unconnected UDP socket;
  - `EISCONN` for an explicit destination on a connected `AF_UNIX` stream;
  - a zero-length datagram delivered as a 0-byte read;
  - `EMSGSIZE` for a UDP payload over 65507 bytes;
  - `send(2)` showing up as `sendto(` in the `--strace` trace.

  Run under `hermit run --verify`.
- T2 (covers F2, F10): on a logically blocking `AF_UNIX` stream socketpair, fill the send buffer, then call `sendto` with `MSG_DONTWAIT`; it must return `EAGAIN`, not wait (H1). Then do one blocking send larger than the free space while a reader thread drains the other end. It must return the full length (H2). Repeat on a `SOCK_NONBLOCK` pair, expecting `EAGAIN` or a short count.
- T3 (covers F3, F15): after the peer closes an `AF_UNIX` stream:
  - `sendto` without `MSG_NOSIGNAL` delivers `SIGPIPE` to a handler and returns `EPIPE`;
  - with `MSG_NOSIGNAL` it returns only `EPIPE`.

  Then cover the errors:
  - `EBADF` on fd -1;
  - `ENOTSOCK` on a pipe;
  - `EFAULT` on a bad buffer;
  - `EACCES` on a UDP broadcast without `SO_BROADCAST`.
- T4 (covers F4, F5, F6, F7): per-call send hints, each checked on the socket type that supports it:
  - On loopback TCP, `MSG_MORE` followed by a plain send arrives intact.
  - On loopback TCP, `MSG_OOB` is readable with `recv(MSG_OOB)`.
  - On UDP loopback, `MSG_CONFIRM` and `MSG_DONTROUTE` sends succeed.
  - On a `SOCK_SEQPACKET` socketpair, `MSG_EOR` keeps record boundaries.

  Two runs must match.
- T5 (covers F8, F9): TCP Fast Open and zerocopy sends on loopback:
  - On loopback TCP, `MSG_FASTOPEN` either connects and delivers, or fails the same way in every run.
  - With `SO_ZEROCOPY` and `MSG_ZEROCOPY` on loopback, read the completion from the error queue.

  Compare two runs under `--verify`.
- T6 (covers F11, F12): send timeouts and signals:
  - With `SO_SNDTIMEO` set on a full socketpair that has no reader, `sendto` must time out in logical time instead of hanging (H3).
  - A blocked `sendto` interrupted by a signal returns `EINTR` without `SA_RESTART` and restarts with it.
- T7 (covers F14): an unbound UDP sender sends to a bound receiver. The receiver prints the source port it got from `recvfrom`, and the sender prints its `getsockname`. Both must be identical across two `hermit run` executions (H4).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_sendrecv sends through execute_nonblockable_fd_syscall: a physically nonblocking socket with InternalIOPolling retry; no MSG_* flag is inspected). linux_determinism stays conditional. 16 flags and 7 tests. The MSG_DONTWAIT, short-stream-send, SO_SNDTIMEO and implicit-autobind hypotheses are under Open questions for measurement. Stays todo; no coverage recorded.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 12, 13 and 14 applied (Target paragraphs: the implicit bind on first send takes an allocator port; blocking sends wait under the scheduler in every mode, honour MSG_DONTWAIT, queue every byte, and end at a virtual-time SO_SNDTIMEO deadline); no new F/T ids; trimmed the allocator-policy tail from open question H4, kept the measurement

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sendto`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sendto&body=Discussion+for+the+%60sendto%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsendto.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sendto%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `sendto`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

These questions need a measurement, not an owner decision, and none of them changes the `target_policy` choice. Each is a hypothesis under Hermit design and names the test that answers it.

- H1: on a logically blocking socket in run mode, does `sendto` with `MSG_DONTWAIT` and a full buffer wait instead of returning `EAGAIN`? (T2)
- H2: does a blocking stream `sendto` larger than the free buffer space return a short count? (T2)
- H3: does `SO_SNDTIMEO` never fire in run mode? (T6)
- H4: is the port chosen by the implicit autobind on first send host-chosen and visible to the peer? (T7)
