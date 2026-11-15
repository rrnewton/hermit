---
schema: syscall-pages/v1
syscall: bind
nr_x86_64: 49
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+bind&body=Discussion+for+the+%60bind%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fbind.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60bind%60%2C+or+in+what+the+page+says%3F%0A
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

`bind(2)` gives a socket a local address. Detcore chooses every address the kernel would otherwise choose itself: AF_UNIX autobind names, AF_NETLINK port IDs requested with `nl_pid == 0`, and AF_INET/AF_INET6 ports requested as 0. It takes them from a global deterministic allocator and then forwards the bind to the kernel. Explicit addresses are passed through, and their ports are recorded so the allocator does not hand them out.

## Linux semantics

`bind(fd, addr, addrlen)` reads a `sockaddr` whose layout depends on its family and attaches it to the socket. The kernel writes nothing back to `addr`; the chosen address can only be read later with `getsockname(2)`. A socket can be bound only once. Most families reject a second bind with `EINVAL`.

Some requests leave the choice to the kernel. An AF_INET or AF_INET6 port of 0 means "pick an ephemeral port from `net.ipv4.ip_local_port_range`". An AF_UNIX address that holds only the family means "autobind": the kernel gives the socket an abstract name, a NUL byte followed by five hex digits. An AF_NETLINK `nl_pid` of 0 means "pick a port ID". Recent kernels randomize all three choices: the ephemeral port search starts at a random offset, and the AF_UNIX autobind counter starts at a random value.

Side effects: a pathname AF_UNIX bind creates a socket inode in the filesystem. Binding fails with `EADDRINUSE` if the path already exists or if another socket in the same network namespace holds the port.

### Flags and options

- F1: AF_UNIX pathname address: creates a socket inode at `sun_path`, subject to the umask and directory permissions. `EADDRINUSE` if the path exists; `ENOENT`/`ENOTDIR`/`EACCES`/`EROFS` from path lookup.
- F2: AF_UNIX abstract address (`sun_path[0] == '\0'`): the name lives in the network namespace, not the filesystem. `EADDRINUSE` if another socket holds the name.
- F3: AF_UNIX autobind (`addrlen == sizeof(sa_family_t)`): the kernel picks an abstract name of a NUL byte and five hex digits.
- F4: AF_NETLINK with `nl_pid == 0`: the kernel picks a port ID. With an explicit `nl_pid`, that ID is used and `EADDRINUSE` if taken. `nl_groups` subscribes to multicast groups (`EPERM` without privilege for some protocols).
- F5: AF_INET with `sin_port == 0`: the kernel picks an ephemeral port from `ip_local_port_range`.
- F6: AF_INET with an explicit port: `EADDRINUSE` if it is in use (subject to `SO_REUSEADDR`/`SO_REUSEPORT`). `EACCES` for a port below `net.ipv4.ip_unprivileged_port_start` without `CAP_NET_BIND_SERVICE`.
- F7: AF_INET6 with `sin6_port == 0` or an explicit port: the same rules as F5 and F6, plus `sin6_scope_id` for link-local addresses and the `IPV6_V6ONLY` interaction with IPv4 binds.
- F8: address not local to the network namespace: `EADDRNOTAVAIL` (unless `IP_FREEBIND`/`IP_TRANSPARENT`).
- F9: `addrlen` too short for the family, or a family that does not match the socket: `EINVAL` or `EAFNOSUPPORT`. `addr == NULL` with `addrlen == 0` reaches the protocol's length check and gives `EINVAL`.
- F10: socket already bound: `EINVAL`.
- F11: bad descriptors and pointers: `EBADF` for a closed fd, `ENOTSOCK` for a non-socket fd, `EFAULT` for an unreadable `addr`.
- F12: other address families (AF_PACKET, AF_VSOCK, ...): family-specific rules, passed to the kernel unchanged.

## Determinism analysis

`linux_determinism: conditional`, kept. With an explicit address, bind's result depends only on the arguments and on namespace state: which names, paths and ports are already taken. That state repeats when every other user of the namespace is inside the same deterministic run. The kernel-chosen addresses (F3, F4, F5, F7 with port 0) are randomized by Linux and do not repeat.

### Nondeterminism sources

- Kernel-chosen ephemeral ports, AF_UNIX autobind names and netlink port IDs
- Ports, abstract names and paths held by processes outside the guest. This applies when the guest shares the host network namespace (`--network=host`) or a filesystem directory with the host.
- The host's `ip_local_port_range` and `ip_unprivileged_port_start` sysctls

## Hermit design

Target policy: `model`. The kernel stays the authority for binding, and Detcore owns every address the kernel would choose. The handler is `handle_bind` at `detcore/src/syscalls/files.rs:4014`, reached from the typed dispatch arm at `detcore/src/lib.rs:2553`.

- AF_UNIX autobind (F3), `detcore/src/syscalls/files.rs:4040`: Detcore asks the global allocator for a port (`GlobalRequest::RequestPort`, served at `detcore/src/tool_global.rs:1731`). It writes the abstract name `\0%05x` of that number (`unix_autobind_address`, `detcore/src/syscalls/files.rs:366`) into a scratch area on the guest stack and binds to that name instead (`detcore/src/syscalls/files.rs:4052`-`4061`). A full allocator gives `EADDRINUSE` (`detcore/src/syscalls/files.rs:4046`).
- AF_NETLINK with `nl_pid == 0` (F4), `detcore/src/syscalls/files.rs:4064`: the port ID becomes `0x4000_0000 | port` (`DETERMINISTIC_NETLINK_PORT_ID_BASE`, `detcore/src/syscalls/files.rs:392`; applied at `detcore/src/syscalls/files.rs:4075`). The rewritten address is also placed on the scratch stack.
- AF_INET port 0 (F5), `detcore/src/syscalls/files.rs:4117`, and AF_INET6 port 0 (F7), `detcore/src/syscalls/files.rs:4158`: Detcore requests a port and binds to it.
- Explicit AF_INET/AF_INET6 ports (F6, F7) are registered with `GlobalRequest::AddUsedPort` (`detcore/src/syscalls/files.rs:4106`-`4115`; served at `detcore/src/tool_global.rs:1757`) so the allocator skips them. The bind then goes to the kernel unchanged.
- Every other case (F1, F2, F8-F12) goes to the kernel unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:4174`).
- Reservations are freed when the last descriptor for the socket closes: `handle_close` at `detcore/src/syscalls/files.rs:1147` and `close_range` at `detcore/src/syscalls/files.rs:1182` call `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`).
- The default `--network=local` gives the guest a private network namespace with only loopback (`hermit-cli/src/bin/hermit/global_opts.rs:53`). Under that default, no host process can hold a guest port or abstract name.
- Record mode does not record `bind`. The recorder has no arm for it and injects it live (`hermit-cli/src/recorder.rs:648`), and the replayer re-executes it (`hermit-cli/src/replayer.rs:575`). Detcore's allocator runs before both, so the replayed bind asks for the same address.

Why `model` and not the alternatives: `emulate` would need a full socket address table outside the kernel, and the kernel's state is already confined to the guest's namespace. `refuse` would break every server. Code facts rule out `passthrough` for a determinized syscall, and plain forwarding would expose the kernel's randomized choices.

Gaps between the code and `model`:

- **Port 0 writes into the guest's input buffer.** On the AF_INET and AF_INET6 port-0 paths, Detcore writes the assigned port back into the caller's own `sockaddr` (`detcore/src/syscalls/files.rs:4122`-`4125` and `detcore/src/syscalls/files.rs:4161`-`4164`). Linux never writes to `addr`, and the buffer may be read-only or reused by the caller. The AF_UNIX and AF_NETLINK paths already use a scratch stack copy, which shows the fix.
- **`addr == NULL` gives `EFAULT`.** `call.umyaddr().ok_or(Errno::EFAULT)` at `detcore/src/syscalls/files.rs:4031` returns `EFAULT` before the kernel sees the call. Linux's AF_INET/AF_UNIX length check returns `EINVAL` for `NULL, 0` (F9).
- **The port is reserved before the kernel answers.** A port-0 or explicit-port bind that the kernel then refuses (`ENOTSOCK`, `EINVAL` for an already bound socket, `EADDRNOTAVAIL`) still holds its reservation until the descriptor closes. That narrows the allocator, but deterministically.
- **The AF_INET path reads a full `sockaddr_in`** (`detcore/src/syscalls/files.rs:4093`) whatever `addrlen` says. A short, unreadable buffer gives `EFAULT` where Linux gives `EINVAL`.
- **The allocator's range is the host's.** `read_port_range` reads `/proc/sys/net/ipv4/ip_local_port_range` from the host (`detcore/src/tool_global.rs:2783`). A different host sysctl gives different port numbers.
- **Implicit binds bypass the allocator.** `listen`, `connect` and an unbound UDP `sendto` make the kernel autobind an ephemeral port without calling bind. Only `handle_bind` asks the allocator, so those ports stay kernel-chosen (see the `listen` and `getsockname` bodies).

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. For `bind` this is today's behaviour (F5, F7) and the reference the other calls must match; the change belongs in the callers named in the last bullet above, not in `handle_bind`.

## Recommended tests

- T1 (covers F2, F10): bind an abstract AF_UNIX name, read it back with `getsockname`, and check that a second bind of the same socket gives `EINVAL`.
  Existing candidate: `tests/c/bind_getsockname.c` (cell `c-programs/bind-getsockname`, `tests/e2e/manifests/c-programs.yaml:14534`, verify ci on ptrace).
- T2 (covers F3): AF_UNIX autobind for STREAM, DGRAM and SEQPACKET sockets. The name must be a NUL byte and five hex digits, and identical across runs.
  Existing candidates: `tests/c/unix_autobind_common.h` through cells `unix-autobind-stream`, `unix-autobind-dgram` and `unix-autobind-seqpacket` (`tests/e2e/manifests/c-programs.yaml:10541`, `:10425`, `:10483`), verify ci on ptrace.
- T3 (covers F4): netlink bind with `nl_pid == 0`. Two sockets must get distinct, nonzero, run-stable port IDs. Add an explicit `nl_pid` bind and an `EADDRINUSE` collision.
  Existing candidates for the `nl_pid == 0` part: `tests/c/netlink_autobind_common.h` through cells `netlink-autobind-route`, `netlink-autobind-generic` and `netlink-autobind-usersock` (`tests/e2e/manifests/c-programs.yaml:4417`, `:4355`, `:4479`).
- T4 (covers F5, F7): AF_INET and AF_INET6 loopback binds with port 0. `getsockname` must report the same port in every run. Also check that the caller's `sockaddr` still holds port 0 after the call, which fails today (Hermit writes the port back).
  Existing candidates for the port-stability part: `tests/c/socket_epoll_ordering.c` (cell at `tests/e2e/manifests/c-programs.yaml:13718`) prints the ephemeral IPv4 port, and the `tcp-info-accept6` cell (`tests/e2e/manifests/c-programs.yaml:9855`) binds AF_INET6.
- T5 (covers F6, F7): an explicit port bind followed by a port-0 bind in the same run: the allocator must not hand out the explicit port. Add an `EADDRINUSE` collision between two sockets binding one explicit port, and `EACCES` for port 80 as an unprivileged guest.
- T6 (covers F1): pathname AF_UNIX bind creates a socket inode (`S_ISSOCK`). A second bind to the same path gives `EADDRINUSE`, and a missing directory gives `ENOENT`.
- T7 (covers F8, F9, F11): `EADDRNOTAVAIL` for a non-local IPv4 address; `EINVAL` for a short `addrlen` and for `NULL, 0` (fails today with `EFAULT`); `EBADF`, `ENOTSOCK` and `EFAULT`.
- T8 (covers F12): bind an AF_PACKET or AF_VSOCK socket where the host allows it, or check that the kernel's errno passes through unchanged.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (Detcore allocates autobind names, netlink IDs and port-0 ports before the kernel bind); linux_determinism conditional kept; F1-F12 inventoried, T1-T8 proposed; gaps: port-0 write-back into the guest's sockaddr, EFAULT for a NULL addr, implicit binds bypass the allocator.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 12 applied (Target paragraph: implicit binds take their port from the allocator that explicit port-0 binds use; bind is the reference path); no new F/T ids; removed the open question on routing implicit binds through the allocator
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/bind_getsockname.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `bind`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+bind&body=Discussion+for+the+%60bind%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fbind.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60bind%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `bind`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should the AF_INET/AF_INET6 port-0 path stop writing the assigned port into the guest's own `sockaddr` (`detcore/src/syscalls/files.rs:4122`-`4125`, `:4161`-`4164`) and use a scratch stack copy, as the AF_UNIX and AF_NETLINK paths do?
- Should a reservation be released when the kernel refuses the bind, rather than at close?
