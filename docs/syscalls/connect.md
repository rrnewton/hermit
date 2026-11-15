---
schema: syscall-pages/v1
syscall: connect
nr_x86_64: 42
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+connect&body=Discussion+for+the+%60connect%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fconnect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60connect%60%2C+or+in+what+the+page+says%3F%0A
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

Connects a socket to a peer address: for a stream socket it opens a connection, and for a datagram socket it sets the default peer. A blocking `connect` can wait for a long time. In `hermit run` Hermit makes the socket physically nonblocking and, for a guest that asked to block, retries the call under the deterministic scheduler, so no thread ever sleeps inside the kernel.

## Linux semantics

`connect(fd, addr, addrlen)` associates socket `fd` with the peer at `addr`. For TCP it sends the handshake and, on a blocking socket, waits until the connection is established or fails; it does not wait for the peer to call `accept`. For a nonblocking TCP socket it returns `EINPROGRESS`; completion is then seen through `poll` or `epoll` and `getsockopt(SO_ERROR)`. For a datagram socket it only records the peer. If the socket has no local address yet, the kernel binds one first (autobind); for TCP and UDP that is an ephemeral port chosen by the kernel.

### Flags and options

- F1: `AF_INET` or `AF_INET6` stream socket: TCP handshake; a blocking socket waits until it is established or fails.
- F2: datagram socket: sets the default peer and filters incoming datagrams; returns at once and sends nothing.
- F3: `AF_UNIX` path name: connects to a socket listening on a filesystem path. `ENOENT` if the path does not exist, `ECONNREFUSED` if nothing listens there. A full backlog makes a blocking caller wait and a nonblocking caller get `EAGAIN`.
- F4: `AF_UNIX` abstract name (leading NUL byte): like F3, but the name lives in the network namespace, not the filesystem.
- F5: implicit autobind: an unbound socket gets a local address first; for TCP and UDP that is an ephemeral port the kernel picks.
- F6: `AF_UNSPEC` address: dissolves a datagram socket's association, or disconnects a TCP socket.
- F7: nonblocking stream socket: `EINPROGRESS`; a second call while pending gives `EALREADY`; after completion the next call returns 0 once and then `EISCONN`, or returns the pending error.
- F8: `connect` on an already connected stream socket gives `EISCONN`.
- F9: connection failures: `ECONNREFUSED`, `ETIMEDOUT`, `ENETUNREACH`, `EHOSTUNREACH`, `EADDRNOTAVAIL` (no free ephemeral port), and `EACCES` or `EPERM` (for example, broadcast without `SO_BROADCAST`, or a firewall rule).
- F10: a signal during a blocking connect: `EINTR` without `SA_RESTART`. The connection carries on in the background, so a restarted or repeated call gets `EALREADY`. A send timeout (`SO_SNDTIMEO`) ends the wait with `EINPROGRESS`.
- F11: argument errors: `EBADF`, `ENOTSOCK`, `EFAULT` (bad `addr`), `EINVAL` (short `addrlen`), `EAFNOSUPPORT` (address family does not match the socket).

## Determinism analysis

`linux_determinism: conditional`. The result depends on what is at the other end:

- An `AF_UNIX` connect (F3, F4) or a datagram connect (F2) between sockets in the same program gives the same result every run, as do argument errors (F11).
- A TCP connect over loopback to a listener in the same program succeeds every run, but its autobound local port (F5) is picked by the kernel's ephemeral port allocator, which uses a per-boot random secret. That port shows up in `getsockname` and in the peer address `accept` returns.
- A connect to an outside peer depends on the network and the peer: success, refusal, timeout and the time taken all vary (F1, F9).
- For a nonblocking connect, how many times the guest sees `EALREADY` before completion depends on timing (F7), and so does where a signal interrupts a blocking connect (F10).

### Nondeterminism sources

- Ephemeral port selection during autobind.
- Outside peers: availability, latency, refusals and timeouts.
- `AF_UNIX` path sockets of host services, which are reachable from the guest whenever their path is visible.
- Completion timing of an in-progress connection, relative to the caller's polls and to signals.

## Hermit design

Target policy: `model`. The kernel performs the connection, and Detcore owns every wait. The dispatch arm at `detcore/src/lib.rs:2552` calls `handle_connect` at `detcore/src/syscalls/io.rs:1336`:

- With the `ConnectBind` scheduling heuristic, the thread first passes a priority change point (`detcore/src/syscalls/io.rs:1345`).
- The call then goes through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/io.rs:1357`; helper at `detcore/src/syscalls/helpers.rs:202`), which chooses one of three paths.
  - If Detcore cannot classify the descriptor, the call runs once on the blocking path, so the kernel's errno wins (`detcore/src/syscalls/helpers.rs:212`). This keeps Linux's error order for cases like an invalid fd with an invalid address (F11).
  - Without the scheduler, in a record or replay run (sockets count as outside endpoints there, `detcore/src/syscalls/helpers.rs:239`), or when the socket is physically blocking, the call runs on the blocking path (`detcore/src/syscalls/helpers.rs:241`).
  - In `hermit run`, sockets are physically nonblocking (see the `socket` body). For a socket the guest believes is blocking, `ioaction_based_on_fd_status` (`detcore/src/syscalls/helpers.rs:745`) picks a nonblocking retry: the call is retried while holding the scheduler's `InternalIOPolling` resource (`detcore/src/syscalls/helpers.rs:254`). For a socket the guest made nonblocking, the call runs once and the guest sees `EINPROGRESS` itself (`detcore/src/syscalls/helpers.rs:268`).
- The retry treats `EAGAIN`, `EWOULDBLOCK`, `EINPROGRESS` and `EALREADY` as "would block" (`detcore/src/syscalls/helpers.rs:1376`), and turns an `EISCONN` on a retried call into success (`detcore/src/syscalls/helpers.rs:1397`; unit test `connect_nonblocking_results` at `detcore/src/syscalls/helpers.rs:1671`). A guest that asked to block therefore sees 0 or the final error, as on Linux (F1, F3, F7).
- If a signal arrives while the retry waits, the retry helper returns the call's interrupt errno (`detcore/src/syscalls/helpers.rs:1476`), which for connect is the trait default `ERESTARTSYS` (`detcore/src/syscalls/helpers.rs:954`). The guest therefore sees `EINTR` without `SA_RESTART` and a restarted call with it, and the kernel connection keeps going, as on Linux (F10). One difference follows from the retry: Linux returns `EALREADY` to a restarted TCP connect that is still pending, while Hermit's restarted call treats `EALREADY` as "would block" and keeps waiting until the connection completes.
- After the kernel has answered, and only when `discover_live_file_metadata` is set, Detcore records whether the peer is a loopback address as a scheduling hint (`detcore/src/syscalls/io.rs:1358`). This never changes the result.

The default `--network local` (`hermit-cli/src/bin/hermit/run.rs:495`) puts the guest in a private namespace with only a loopback interface (`hermit-cli/src/bin/hermit/run.rs:905`), so outside IP peers are unreachable and fail the same way every run. `--network host` lifts that.

Why `model` and not the alternatives:

- `passthrough` lets a blocking connect sleep in the kernel, invisible to the deterministic scheduler; the scheduler then cannot run the thread that will `accept` or `listen`.
- `refuse` would break every client.
- `emulate` (a user-space network stack) is not needed for loopback traffic in the private namespace.

What remains before the code fully realizes `model`:

- **F5:** the autobound ephemeral port is the host kernel's choice. `handle_bind` gives deterministic ports only to an explicit `bind` to port 0 (`detcore/src/syscalls/files.rs:4119`); an unbound socket that calls `connect` gets a host-chosen port, visible through `getsockname` and through the peer address of the matching `accept`. T1 checks it.
- **F10, send timeout:** I found no use of `SO_SNDTIMEO` in Detcore, and the connect retry has no deadline. By this reading of the code, a guest that set a send timeout and connects to a peer that never answers keeps waiting under Hermit, where Linux would return `EINPROGRESS` after the timeout. This is a hypothesis from the code, not a measurement.
- **Record and replay:** sockets take the blocking path (`detcore/src/syscalls/helpers.rs:241`), so a connect whose peer is in the same guest relies on the recording's thread order to avoid a hang.
- **Outside peers through `AF_UNIX` paths** (F3): a host service socket visible in the guest's filesystem is still reachable, and its replies are not deterministic.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. Today `handle_connect` (`detcore/src/syscalls/io.rs:1336`) forwards an unbound socket to the kernel, which picks the port from a randomized offset in that range: `inet_hash_connect` for TCP, and `inet_autobind` called from `inet_dgram_connect` for UDP (F5).

Target (owner decision 14, with decision 13 for the timeout): a guest-blocking `connect` waits in Detcore's scheduler, never in the kernel, through a nonblocking probe plus a scheduler wait. `hermit run` already does this through `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:254`, retry loop `retry_nonblocking_syscall` at `detcore/src/syscalls/helpers.rs:1414`). Two parts are missing. First, the cases that still block in the kernel while holding the turn, through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:225` and `detcore/src/syscalls/helpers.rs:250`): no scheduler, record and replay, and a descriptor Detcore cannot classify. Second, a deadline: with `SO_SNDTIMEO` set, the wait must end with `EINPROGRESS` once that much virtual time has passed (F10), as Linux's `inet_stream_connect` does when `sock_sndtimeo` expires. `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) already takes a logical deadline for poll, epoll, futex and signal waits, but Detcore must first learn the timeout: nothing in `detcore/src` reads `SO_RCVTIMEO` or `SO_SNDTIMEO` today (see the `setsockopt` body). `connect` stays `model`, not `refuse`, because almost every client calls it.

## Recommended tests

- T1 (covers F1, F5): a blocking TCP connect over loopback to a listener in the same guest, under `hermit run --verify` on ptrace. Print `getsockname` on the client and the peer address from `accept`, and check both are identical across runs.
  Existing candidates (none print the local port): `tests/c/socket_epoll_ordering.c:103` (cell `c-programs/socket-epoll-ordering`, `tests/e2e/manifests/c-programs.yaml:13718`, verify ci on ptrace); `tests/c/tcp_info_client4.c:68` (cell `c-programs/tcp-info-client4`, `tests/e2e/manifests/c-programs.yaml:9913`, verify ci on ptrace); `tests/c/so_incoming_cpu_tcp4.c:48` (cell `c-programs/so-incoming-cpu-tcp4`, `tests/e2e/manifests/c-programs.yaml:8482`, verify ci on ptrace).
- T2 (covers F2, F6): connect a UDP socket, send to the default peer, then connect with `AF_UNSPEC`; a following `send` without an address must fail with `EDESTADDRREQ`.
- T3 (covers F3, F4): `AF_UNIX` connects to a path listener and an abstract listener; `ENOENT` for a missing path; `ECONNREFUSED` for a path bound but not listening; with `listen(fd, 0)` and a filled backlog, a nonblocking connect gets `EAGAIN` and a blocking one completes after the server accepts.
- T4 (covers F7, F8): a nonblocking TCP connect over loopback. Expect `EINPROGRESS`, then `POLLOUT` with `SO_ERROR` 0, then `EISCONN` on a repeat call. On a blocking socket, the same connect must return 0 directly under Hermit.
- T5 (covers F9): connect to a loopback port with no listener; both blocking and nonblocking sockets must end with `ECONNREFUSED` (the nonblocking one through `SO_ERROR`). Under `--network local`, connect to `192.0.2.1` and check that the same error comes back on every run.
- T6 (covers F10): block a connect on a full `AF_UNIX` backlog, then deliver `SIGALRM` with and without `SA_RESTART`. Record `EINTR`, and the result of the next call. Set `SO_SNDTIMEO` and check whether the wait ends; this confirms or refutes the timeout hypothesis above.
- T7 (covers F11): a raw `syscall(SYS_connect, ...)` error matrix compared with native results: fd -1 with an invalid address (`EBADF`), a regular-file fd (`ENOTSOCK`), `addrlen` 1 (`EINVAL`), an `AF_INET6` address on an `AF_INET` socket (`EAFNOSUPPORT`), and an unmapped `addr` (`EFAULT`).
- T8 (covers F1, F3): record and replay a loopback client and server in one guest; replay must give the same connect results without hanging.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (blocking connects retried nonblocking under the scheduler in run mode); linux_determinism stays conditional (autobind ephemeral port, outside peers); left todo with open questions on the undeterminized autobind port and SO_SNDTIMEO.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 12, 13 and 14 applied (Target paragraphs: an implicit bind takes its port from Detcore's allocator; a blocking connect waits under the scheduler in every mode, with SO_SNDTIMEO on virtual time giving EINPROGRESS); no new F/T ids; removed both open questions (autobind port, SO_SNDTIMEO on logical time) and the empty heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `connect`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+connect&body=Discussion+for+the+%60connect%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fconnect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60connect%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `connect`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
