---
schema: syscall-pages/v1
syscall: getsockname
nr_x86_64: 51
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/608
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getsockname&body=Discussion+for+the+%60getsockname%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetsockname.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getsockname%60%2C+or+in+what+the+page+says%3F%0A
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

`getsockname(2)` returns the local address a socket is bound to. Detcore forwards it to the kernel. The result is deterministic when Detcore chose the address in `bind(2)`: AF_UNIX autobind names, netlink port IDs, and ports requested as 0. Ports the kernel autobinds during `connect`, `listen` or `sendto` are still host-chosen.

## Linux semantics

`getsockname(fd, addr, addrlen)` asks the protocol for the socket's local address and copies it to `addr`. `*addrlen` is the buffer size on entry and the address's full size on return. If the buffer is too small, the address is truncated and the returned `*addrlen` is larger than the buffer. The call does not change socket state.

What comes back depends on how the socket got its address: an explicit `bind`, a `bind` that left the choice to the kernel, an implicit autobind during `connect`, `listen` or an unbound datagram send, or no address at all.

### Flags and options

- F1: `*addrlen` in/out: the input is the buffer size; the output is the real address length. A short buffer truncates the copy, and the returned length exceeds the buffer.
- F2: bound AF_INET/AF_INET6 socket: the bound address and port, including a port the kernel picked for a port-0 `bind`.
- F3: unbound AF_INET/AF_INET6 socket: the wildcard address with port 0.
- F4: AF_INET/AF_INET6 socket autobound by `connect`, `listen` or an unbound UDP `sendto`: the kernel's ephemeral port and the source address it routed from.
- F5: AF_UNIX: a pathname, an abstract name, an autobind name (a NUL byte and five hex digits; length `offsetof(sun_path) + 6`), or an unnamed socket (from `socketpair` or never bound) with length `sizeof(sa_family_t)`.
- F6: AF_NETLINK: `nl_pid` (the explicit or autobound port ID) and `nl_groups`.
- F7: errors: `EBADF` for a closed fd, `ENOTSOCK` for a non-socket fd, `EFAULT` for an unreadable `addrlen` or unwritable `addr`, `EINVAL` for a negative `*addrlen`.
- F8: other families (AF_PACKET, AF_VSOCK, ...): family-specific addresses, returned by the kernel.

## Determinism analysis

`linux_determinism: conditional`, kept. The call only reports state, so it repeats whenever the address it reports was chosen deterministically. Explicit addresses repeat. Kernel-chosen ports, autobind names and netlink port IDs are randomized by Linux, and an address from the host network differs across hosts.

### Nondeterminism sources

- Kernel-chosen ephemeral ports (F2 with port 0 on Linux, F4), AF_UNIX autobind names and netlink port IDs (F5, F6)
- Interface addresses of the host network namespace, only with `--network=host`

## Hermit design

Target policy: `model`. The kernel reports the address, and Detcore makes the address deterministic when it is assigned. `getsockname` itself needs no rewriting once every assignment goes through Detcore.

What the code does now:

- The typed dispatch arm at `detcore/src/lib.rs:2562` calls `handle_getsockname` (`detcore/src/syscalls/files.rs:3890`), which forwards the call through `record_or_replay` (`detcore/src/syscalls/files.rs:3895`). Both carry an autonomous-bot "needs human review" tag.
- `handle_bind` (`detcore/src/syscalls/files.rs:4014`) assigns the addresses F2, F5 and F6 report: ports for port-0 binds, `\0%05x` AF_UNIX autobind names, and netlink IDs `0x4000_0000 | port`. See the `bind` body.
- By default the guest has a private network namespace with only loopback (`hermit-cli/src/bin/hermit/global_opts.rs:53`), so the local IP addresses repeat.
- Record mode captures the output buffer: the recorder sends `Getsockname` to `handle_sockopt_family` (`hermit-cli/src/recorder.rs:631`), and the replayer returns the recorded bytes (`hermit-cli/src/replayer.rs:538`).

Why `model`: code facts rule out `passthrough` for a determinized syscall. `emulate` would duplicate the kernel's address table. `refuse` would break servers that bind port 0 and print their port.

Gap between the code and `model`:

- **Implicit autobind ports (F4).** Only `handle_bind` asks the global port allocator (`GlobalRequest::RequestPort` at `detcore/src/syscalls/files.rs:4043`, `:4072`, `:4119` and `:4158`, all inside `handle_bind`). `connect` (`detcore/src/syscalls/io.rs:1336`) and `listen` (`detcore/src/syscalls/files.rs:3879`) forward unbound sockets to the kernel, which picks a random port. `getsockname` then reports that port, and so does the peer's `getpeername`. Fixing it belongs in those calls, not here.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. `getsockname` itself needs no change: once the implicit binds take allocator ports, the port it reports for F4 is the allocator's. Today `connect` (`detcore/src/syscalls/io.rs:1336`), `listen` (`detcore/src/syscalls/files.rs:3879`) and an unbound `sendto` (`handle_sendrecv`, `detcore/src/syscalls/io.rs:1409`) forward the socket to the kernel, which picks a random port.

## Recommended tests

- T1 (covers F2): bind AF_INET and AF_INET6 loopback sockets to port 0 and print the port `getsockname` reports. It must be identical across `--verify` runs on ptrace.
  Existing candidates: `tests/c/socket_epoll_ordering.c` (prints the port; cell at `tests/e2e/manifests/c-programs.yaml:13718`), `tests/c/record_replay_setsockopt.c` (getsockname at line 49; cell at `tests/e2e/manifests/c-programs.yaml:7167`), and `tests/c/tcp_info_accept6.c` for AF_INET6 (cell at `tests/e2e/manifests/c-programs.yaml:9855`), all verify ci on ptrace.
- T2 (covers F5): AF_UNIX abstract, autobind and unnamed sockets, plus a pathname bind (new).
  Existing candidates: `tests/c/bind_getsockname.c` (abstract name echo; cell at `tests/e2e/manifests/c-programs.yaml:14534`), `tests/c/unix_autobind_common.h` (autobind shape check at lines 40-57; cells `unix-autobind-stream`/`-dgram`/`-seqpacket` at `tests/e2e/manifests/c-programs.yaml:10541`, `:10425`, `:10483`), and `tests/c/sockname_unnamed.c` (family-only length at line 46; cell at `tests/e2e/manifests/c-programs.yaml:14472`).
- T3 (covers F6): netlink `nl_pid` after a `nl_pid == 0` bind.
  Existing candidates: `tests/c/netlink_autobind_common.h` through cells `netlink-autobind-route`/`-generic`/`-usersock` (`tests/e2e/manifests/c-programs.yaml:4417`, `:4355`, `:4479`).
- T4 (covers F1): pass a 4-byte buffer for an AF_INET6 socket. The returned length must be 28, and bytes past the fourth must be untouched.
- T5 (covers F3, F4): an unbound TCP socket reports `0.0.0.0:0`. After `connect` to a loopback listener without `bind`, the local port must be identical across `--verify` runs. This is expected to fail until implicit autobind goes through the allocator.
- T6 (covers F7): `EBADF` for fd -1, `ENOTSOCK` for a pipe, `EFAULT` for a bad `addrlen`, `EINVAL` for `*addrlen == -1`.
  Existing partial coverage: `EBADF` in `tests/c/sockname_unnamed.c:59`.
- T7 (covers F2, F8): record and replay a program that prints its port-0 bind port, and check that the replayed output matches. Separately, check that an AF_PACKET or AF_VSOCK address passes through where the host allows it.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/608: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (Detcore assigns the addresses at bind; the recorder captures the output buffer); linux_determinism conditional kept; F1-F8 inventoried, T1-T7 proposed; gap: ports autobound by connect/listen/sendto are kernel-chosen.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 12 applied (Target paragraph: implicit binds take allocator ports, so the F4 port getsockname reports repeats with no change here); no new F/T ids; removed the open question (allocator in the callers versus rewriting the reported port) and the empty heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getsockname`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getsockname&body=Discussion+for+the+%60getsockname%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetsockname.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getsockname%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getsockname" (https://github.com/rrnewton/hermit/issues/608), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
