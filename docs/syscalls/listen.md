---
schema: syscall-pages/v1
syscall: listen
nr_x86_64: 50
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/631
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+listen&body=Discussion+for+the+%60listen%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flisten.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60listen%60%2C+or+in+what+the+page+says%3F%0A
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

`listen(2)` marks a stream socket as passive, so it accepts connections. Detcore forwards it to the kernel unchanged. That is deterministic for a socket that is already bound. On an unbound AF_INET/AF_INET6 socket, the kernel autobinds a port that Detcore's port allocator never sees.

## Linux semantics

`listen(fd, backlog)` moves a bound connection-oriented socket into the listening state and sizes its accept queue. The kernel silently clamps `backlog` to `net.core.somaxconn`; the comparison is unsigned, so a negative `backlog` becomes `somaxconn`. Calling `listen` again on a listening socket only changes the backlog. Nothing is copied to or from user memory.

An unbound AF_INET or AF_INET6 TCP socket is autobound by `listen` to an ephemeral port from `ip_local_port_range`, chosen from a random starting offset. An unbound AF_UNIX socket cannot listen (`EINVAL`).

Which later connections fit in the accept queue, and when a full queue drops or delays a SYN, depends on how `connect` and `accept` interleave. That is a property of those calls; `listen` only sets the queue size.

### Flags and options

- F1: `backlog` in `1..=somaxconn` on a bound TCP, SCTP or AF_UNIX STREAM/SEQPACKET socket: the socket listens with that queue size.
- F2: `backlog` of 0, negative, or greater than `somaxconn`: accepted. A negative value or one above the limit is clamped to `somaxconn`; 0 gives the smallest queue.
- F3: `listen` on an already listening socket: returns 0 and updates the backlog.
- F4: `listen` on an unbound AF_INET/AF_INET6 socket: the kernel autobinds an ephemeral port. `EADDRINUSE` if no port is free.
- F5: `listen` on an unbound AF_UNIX socket: `EINVAL`.
- F6: socket type without a listening state (SOCK_DGRAM, SOCK_RAW): `EOPNOTSUPP`. A TCP socket that is connected or connecting: `EINVAL`.
- F7: address conflict found at listen time, for example two sockets bound to one port with `SO_REUSEADDR` but not `SO_REUSEPORT`: `EADDRINUSE` on the second `listen`.
- F8: bad descriptors: `EBADF` for a closed fd, `ENOTSOCK` for a non-socket fd.

## Determinism analysis

`linux_determinism: nondeterministic -> conditional`. `listen`'s own result depends only on the socket's state, its arguments and the `somaxconn` sysctl; it reads no clock and returns no host identity. The one nondeterministic effect is F4: the port the kernel autobinds is randomized and becomes visible through `getsockname(2)` and through the peer's `getpeername(2)`. On a socket that was bound first, `listen` repeats.

The legacy text blamed "backlog and observable readiness" on peer timing. Accept-queue overflow and readiness belong to `connect`, `accept` and the polling calls, and repeat when Hermit schedules every peer. A peer outside the guest is only possible with `--network=host`.

### Nondeterminism sources

- The ephemeral port the kernel chooses when `listen` autobinds an unbound INET socket (F4)
- The host's `net.core.somaxconn` and `ip_local_port_range` sysctls
- Connections from outside the guest, only with `--network=host`

## Hermit design

Target policy: `model`. The kernel owns the listening state inside the guest's private network namespace, and Detcore must own any port the kernel would choose.

What the code does now:

- The typed dispatch arm at `detcore/src/lib.rs:2559` calls `handle_listen` (`detcore/src/syscalls/files.rs:3879`), which forwards the call unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:3884`). Both carry an autonomous-bot "needs human review" tag.
- `listen` never blocks, so it needs no nonblocking conversion. Detcore schedules the blocking half, `accept`, separately.
- Socket descriptors are tracked: `get_fd` lists `Listen` among the single-fd syscalls (`detcore/src/syscalls/helpers.rs:815`).
- By default the guest has its own network namespace with only loopback (`hermit-cli/src/bin/hermit/global_opts.rs:53`), so no host socket can collide with a guest port (F7) or connect to a guest listener.
- Record mode does not record `listen`. The recorder injects it live (`hermit-cli/src/recorder.rs:648`) and the replayer re-executes it (`hermit-cli/src/replayer.rs:575`). Replay therefore needs the socket to be really bound, which is true because `bind` is re-executed the same way.

Why `model`: code facts rule out `passthrough` for a determinized syscall. `emulate` would need a listening-socket model outside the kernel for no gain. `refuse` would break every server.

Gap between the code and `model`:

- **Autobind at `listen` bypasses the allocator (F4).** Only `handle_bind` asks the global allocator for a port (`GlobalRequest::RequestPort` at `detcore/src/syscalls/files.rs:4119`; no other caller in `detcore/src`). `handle_listen` forwards an unbound INET socket straight to the kernel, which picks a random port. A fix is to detect an unbound AF_INET/AF_INET6 socket and issue a port-0 bind through `handle_bind`'s allocator path before forwarding `listen`. Owner decision 12 makes this the target; see below.

`linux_determinism` becomes `conditional` because `listen` repeats exactly when the socket was bound first, which is the common server pattern.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. Today `handle_listen` (`detcore/src/syscalls/files.rs:3879`) forwards an unbound socket to the kernel, which picks the port in `inet_csk_get_port` from a random offset in that range (F4). How Detcore learns that a socket is unbound is an implementation choice: it can keep a bound flag in its fd table, set when `bind` or an implicit bind succeeds, or probe with `getsockname` for port 0 before forwarding.

## Recommended tests

- T1 (covers F1): bind a loopback TCP socket to port 0, `listen` with a small backlog, then connect and accept several clients under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/socket_epoll_ordering.c` (`listen(srv, NCONN + 1)` at line 93; cell `c-programs/socket-epoll-ordering`, `tests/e2e/manifests/c-programs.yaml:13718`), `tests/c/tcp_info_accept4.c` (`listen(listener, 1)` at line 60; cell at `tests/e2e/manifests/c-programs.yaml:9797`), and `tests/c/record_replay_setsockopt.c` (two `listen` calls at line 62; cell at `tests/e2e/manifests/c-programs.yaml:7167`), all verify ci on ptrace.
- T2 (covers F2, F3): `listen` with backlog 0, -1 and `INT_MAX` each returns 0, and a second `listen` on a listening socket returns 0. A client can still connect after each.
- T3 (covers F4): `listen` on an unbound AF_INET socket, then `getsockname`. The port must be nonzero and identical across `--verify` runs. This is expected to fail until the gap above is fixed.
- T4 (covers F5, F6, F8): `EINVAL` for an unbound AF_UNIX stream socket, `EOPNOTSUPP` for a UDP socket, `EINVAL` for a connected TCP socket, `EBADF` for a closed fd, and `ENOTSOCK` for a pipe.
- T5 (covers F7): two sockets with `SO_REUSEADDR` bound to one port. The first `listen` succeeds and the second returns `EADDRINUSE`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/631: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model; linux_determinism nondeterministic -> conditional (listen repeats on a bound socket; only the kernel's autobind port at listen is random); F1-F8 inventoried, T1-T5 proposed; gap: autobind at listen bypasses Detcore's port allocator.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 12 applied (Target paragraph: listen on an unbound INET socket takes its port from Detcore's allocator; how to detect an unbound socket is left as an implementation choice); no new F/T ids; removed the open question on routing listen through the allocator
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/socket_epoll_ordering.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `listen`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+listen&body=Discussion+for+the+%60listen%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flisten.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60listen%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: listen" (https://github.com/rrnewton/hermit/issues/631), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### Open questions

- Should record mode record `listen`, or is live re-execution on replay (`hermit-cli/src/replayer.rs:575`) the intended design?
