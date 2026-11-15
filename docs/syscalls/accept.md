---
schema: syscall-pages/v1
syscall: accept
nr_x86_64: 43
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+accept&body=Discussion+for+the+%60accept%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Faccept.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60accept%60%2C+or+in+what+the+page+says%3F%0A
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

Takes the first pending connection from a listening socket and returns a new descriptor for it, optionally writing the peer's address. A blocking `accept` waits until a client connects. Hermit handles `accept` as `accept4` with no flags: in `hermit run` the wait becomes a nonblocking retry under the deterministic scheduler, and the new descriptor is physically nonblocking while the guest sees an ordinary blocking socket.

## Linux semantics

`accept(fd, addr, addrlen)` removes the first connection from `fd`'s queue of completed connections and returns a new socket descriptor for it, the lowest free number. It works on `SOCK_STREAM` and `SOCK_SEQPACKET` sockets that are listening. If `addr` is not NULL, the peer address is written there, truncated to `*addrlen`, and `*addrlen` is set to the address's real size. `accept` is `accept4` with `flags = 0`: the new descriptor never has `O_NONBLOCK` or `FD_CLOEXEC`, whatever the listener has. On Linux the new socket does not inherit `O_NONBLOCK` or `O_ASYNC` from the listener.

### Flags and options

- F1: blocking listener: waits until a connection is queued, then returns the first one (queue order).
- F2: nonblocking listener (`O_NONBLOCK` on `fd`): `EAGAIN` or `EWOULDBLOCK` when no connection is queued.
- F3: `addr == NULL`: no address is returned and `addrlen` is not used.
- F4: `addr != NULL`: the peer address is written, truncated to `*addrlen`, and `*addrlen` is updated to the full size. An unnamed `AF_UNIX` peer returns only the family, with `*addrlen` equal to `sizeof(sa_family_t)`.
- F5: flags of the new descriptor: no `O_NONBLOCK` and no `FD_CLOEXEC`, regardless of the listener.
- F6: socket-state errors: `EINVAL` (not listening, or a negative `*addrlen`), `EOPNOTSUPP` (a socket type that does not accept connections, such as `SOCK_DGRAM`), `ENOTSOCK`, `EBADF`.
- F7: a signal while blocked: `EINTR` without `SA_RESTART`; with it the call restarts, unless a receive timeout is set.
- F8: `ECONNABORTED` when a queued connection was reset before it was accepted. Linux can also return network errors already pending on the new socket (`ENETDOWN`, `EPROTO`, `EHOSTUNREACH` and others), which callers should treat like `EAGAIN`.
- F9: resource and policy errors: `EMFILE`, `ENFILE`, `ENOBUFS`, `ENOMEM`, `EPERM` (firewall).
- F10: a receive timeout (`SO_RCVTIMEO`) on a blocking listener ends the wait with `EAGAIN`.
- F11: `EFAULT` for an unwritable `addr` or an unreadable or unwritable `addrlen`.

## Determinism analysis

`linux_determinism: conditional`. With the listener and its clients in the same program, which connection `accept` returns is fixed by queue order, but two things still vary:

- When the call returns, relative to other threads, depends on when the client's connect completes (F1, F2, F10).
- The peer address returned (F4) contains the client's port. If the client did not bind explicitly, that is an ephemeral port picked by the host kernel, which differs between runs (see the `connect` body).

With outside clients, which connection arrives, from where, and when, depends entirely on the network. Errors from argument checks (F3, F6, F11) repeat for the same inputs.

### Nondeterminism sources

- The timing of client connects relative to the accepting thread.
- The client's ephemeral port in the returned peer address.
- Outside clients, and resets of queued connections (F8).

## Hermit design

Target policy: `model`. The kernel keeps the connection queue, and Detcore owns the wait and the new descriptor's flags. The dispatch arm at `detcore/src/lib.rs:2597` converts `accept` to `accept4` with no flags and calls `handle_accept4` at `detcore/src/syscalls/files.rs:4526`:

- When `use_nonblocking_sockets()` is true, `SOCK_NONBLOCK` is added to the real call, so the new socket is physically nonblocking (`detcore/src/syscalls/files.rs:4534`).
- The call goes through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:4541`; helper at `detcore/src/syscalls/helpers.rs:202`):
  - A listener the guest believes is blocking but which is physically nonblocking is retried while holding the scheduler's `InternalIOPolling` resource (`detcore/src/syscalls/helpers.rs:254`), with `EAGAIN` and `EWOULDBLOCK` as "would block" (`detcore/src/syscalls/helpers.rs:1270`). The guest sees an ordinary blocking accept (F1).
  - A listener the guest made nonblocking runs once, and the guest sees `EAGAIN` itself (`detcore/src/syscalls/helpers.rs:268`; F2).
  - In a record or replay run, or without the scheduler, the call takes the blocking path, because sockets count as outside endpoints (`detcore/src/syscalls/helpers.rs:239`, `detcore/src/syscalls/helpers.rs:241`).
  - If Detcore cannot classify the listener, the call runs once on the blocking path and the kernel's errno wins (`detcore/src/syscalls/helpers.rs:212`; F6).
- A signal during the retry returns the trait default `ERESTARTSYS` (`detcore/src/syscalls/helpers.rs:954`, `detcore/src/syscalls/helpers.rs:1476`), so `SA_RESTART` decides between `EINTR` and a restart, as on Linux (F7).
- The new descriptor is added to the fd table with only the flags the guest asked for, `oflag_from_sock_bits(flags)` (`detcore/src/syscalls/files.rs:4547`; helper at `detcore/src/syscalls/files.rs:70`). For `accept` that is none, so the guest sees a blocking descriptor without `FD_CLOEXEC` (F5), and `maybe_set_nonblocking_fd` records that the descriptor is physically nonblocking (`detcore/src/syscalls/files.rs:4552`).
- The peer address and `addrlen` are written by the kernel and not changed (F3, F4, F11).

Why `model` and not the alternatives:

- `passthrough` lets a blocking accept sleep in the kernel, invisible to the deterministic scheduler, which then cannot run the client thread that would connect. That is a deadlock for any single-process client and server.
- `refuse` would break every server.
- `emulate` (a user-space connection queue) is not needed while both ends are kernel sockets in the private loopback namespace.

What remains before the code fully realizes `model`:

- **F4:** the client's ephemeral port in the returned address is not determinized (owner decision 12 sets the target; see below).
- **F10:** I found no use of `SO_RCVTIMEO` in Detcore, and the accept retry has no deadline. By this reading of the code, a logically blocking accept with a receive timeout keeps waiting under Hermit until a client connects, where Linux returns `EAGAIN` after the timeout. This is a hypothesis from the code, not a measurement.
- **Syscall number:** Hermit injects `accept4` (nr 288) instead of `accept` (nr 43). The result is the same for the guest; only a tracer or a seccomp filter that tells the two apart can see it.
- **Record and replay:** accept takes the blocking path, so an accept whose client is in the same guest relies on the recording's thread order to avoid a hang.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. `accept` itself needs no change: the client port written into `addr` (F4) becomes the allocator's once the client's `connect` takes one. Today `handle_connect` (`detcore/src/syscalls/io.rs:1336`) forwards an unbound socket to the kernel, which picks a random port.

Target (owner decision 14, with decision 13 for the timeout): a guest-blocking `accept` waits in Detcore's scheduler, never in the kernel, through a nonblocking probe plus a scheduler wait. `hermit run` already does this through `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:254`, retry loop `retry_nonblocking_syscall` at `detcore/src/syscalls/helpers.rs:1414`). Two parts are missing. First, the cases that still block in the kernel while holding the turn, through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:225` and `detcore/src/syscalls/helpers.rs:250`): no scheduler, record and replay, and a descriptor Detcore cannot classify. Second, a deadline: with `SO_RCVTIMEO` set, the wait must end with `EAGAIN` once that much virtual time has passed (F10), as Linux's `inet_csk_accept` does when `sock_rcvtimeo` expires. `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) already takes a logical deadline for poll, epoll, futex and signal waits, but Detcore must first learn the timeout: nothing in `detcore/src` reads `SO_RCVTIMEO` or `SO_SNDTIMEO` today (see the `setsockopt` body). `accept` stays `model`, not `refuse`, because every server calls it.

## Recommended tests

- T1 (covers F1, F3, F5): a blocking `accept(srv, NULL, NULL)` on a loopback listener with clients in the same guest, under `hermit run --verify` on ptrace. Check that the accepted descriptors and the event order are identical across runs, and that `fcntl` reports neither `O_NONBLOCK` nor `FD_CLOEXEC` on the new descriptor.
  Existing candidates (none check the descriptor flags): `tests/c/socket_epoll_ordering.c:107` (cell `c-programs/socket-epoll-ordering`, `tests/e2e/manifests/c-programs.yaml:13718`, verify ci on ptrace); `tests/c/tcp_info_accept4.c:71` (cell `c-programs/tcp-info-accept4`, `tests/e2e/manifests/c-programs.yaml:9797`, verify ci on ptrace); `tests/c/so_incoming_cpu_tcp4.c:51` (cell `c-programs/so-incoming-cpu-tcp4`, `tests/e2e/manifests/c-programs.yaml:8482`, verify ci on ptrace).
- T2 (covers F2): a nonblocking listener with an empty queue returns `EAGAIN`; after a client connects, the next call succeeds.
- T3 (covers F4): accept with a non-NULL `addr` for a TCP client that bound an explicit port and for one that did not, and for an unnamed `AF_UNIX` client. Print the address and `*addrlen`, also with a buffer too small for the address, and check they are identical across runs. The autobound client's port is expected to differ until the autobind question is settled.
- T4 (covers F6, F11): a raw `syscall(SYS_accept, ...)` error matrix compared with native results: a socket that is not listening (`EINVAL`), a `SOCK_DGRAM` socket (`EOPNOTSUPP`), a regular file (`ENOTSOCK`), a closed fd (`EBADF`), and an unmapped `addr` (`EFAULT`).
- T5 (covers F7, F10): a thread blocked in accept gets `SIGALRM` with and without `SA_RESTART`; record `EINTR` or the restarted result. Set `SO_RCVTIMEO` on the listener and check whether the wait ends with `EAGAIN`; this confirms or refutes the timeout hypothesis above.
- T6 (covers F8, F9): a client connects and resets (`SO_LINGER` with a zero timeout, then `close`) before the server accepts; record the result. Lower `RLIMIT_NOFILE` so the accepted descriptor cannot be allocated and check `EMFILE`.
- T7 (covers F1, F4): record and replay a loopback client and server in one guest; replay must return the same descriptors and addresses without hanging.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handled as accept4 with no flags; blocking accepts retried nonblocking under the scheduler in run mode); linux_determinism stays conditional (client ephemeral port, connect timing); left todo with an open question on SO_RCVTIMEO.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 12, 13 and 14 applied (Target paragraphs: the client's implicit-bind port comes from Detcore's allocator; a blocking accept waits under the scheduler in every mode, with SO_RCVTIMEO on virtual time giving EAGAIN); no new F/T ids; removed the open question on SO_RCVTIMEO on logical time and the empty heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `accept`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+accept&body=Discussion+for+the+%60accept%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Faccept.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60accept%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `accept`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`accept4` shares the same handler (`detcore/src/lib.rs:2598`) and has its own body.
