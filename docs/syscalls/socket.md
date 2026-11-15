---
schema: syscall-pages/v1
syscall: socket
nr_x86_64: 41
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+socket&body=Discussion+for+the+%60socket%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsocket.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60socket%60%2C+or+in+what+the+page+says%3F%0A
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

Creates a socket of a given domain, type and protocol and returns a new file descriptor. Hermit lets the kernel create the socket, registers it in Detcore's fd table, and in `hermit run` makes it physically nonblocking so later `connect`, `accept` and I/O calls never block inside the kernel. The guest still sees the blocking or nonblocking mode it asked for.

## Linux semantics

`socket(domain, type, protocol)` returns the lowest unused file descriptor, bound to a new socket inode on the kernel's socket filesystem. `type` carries the socket type in its low bits and two creation flags, `SOCK_NONBLOCK` and `SOCK_CLOEXEC`. `protocol` 0 selects the default protocol for the domain and type. The call itself never blocks and sends no traffic. Whether a domain or type exists depends on the host kernel's configuration and on the caller's capabilities in its network namespace.

### Flags and options

- F1: `domain = AF_UNIX` (`AF_LOCAL`): local sockets, named by a filesystem path, an abstract name or nothing.
- F2: `domain = AF_INET` or `AF_INET6`: IP sockets in the caller's network namespace.
- F3: `domain = AF_NETLINK`, with a netlink protocol such as `NETLINK_ROUTE` or `NETLINK_SOCK_DIAG`: kernel-to-user messaging that reports live kernel state.
- F4: other domains (`AF_PACKET`, `AF_VSOCK`, `AF_ALG` and others): available only if the host kernel supports them; otherwise `EAFNOSUPPORT`.
- F5: `type = SOCK_STREAM`.
- F6: `type = SOCK_DGRAM`, including unprivileged ICMP sockets, which are gated by `net.ipv4.ping_group_range`.
- F7: `type = SOCK_SEQPACKET`, `SOCK_RAW` or `SOCK_RDM`. `SOCK_RAW` needs `CAP_NET_RAW` and otherwise gives `EPERM`.
- F8: `SOCK_NONBLOCK` in `type`: the new open file description has `O_NONBLOCK`.
- F9: `SOCK_CLOEXEC` in `type`: the new descriptor has `FD_CLOEXEC`.
- F10: `protocol`: 0 for the default, or an explicit protocol. A protocol the domain does not support gives `EPROTONOSUPPORT`, and a type the domain does not support gives `ESOCKTNOSUPPORT` or `EPROTOTYPE`.
- F11: unknown bits in `type` give `EINVAL`.
- F12: resource error: `EMFILE` when the descriptor table is full under `RLIMIT_NOFILE`.
- F13: identity of the result: the descriptor number is the lowest unused one, and the socket inode number, visible through `fstat` and `/proc/self/fd`, comes from a host-wide counter.
- F14: host-transient resource errors: `ENFILE` at the system-wide file limit, and `ENOBUFS` or `ENOMEM` when the host cannot allocate.
- F15: host-config error: `EACCES` when the host's security policy (an LSM) refuses the socket.

## Determinism analysis

`linux_determinism: conditional`. For fixed descriptor state and a fixed host, the return value is deterministic: the lowest free descriptor, or the same errno. It is not deterministic in three ways:

- the socket's inode number is drawn from a counter shared by every process on the host (F13);
- whether a domain, type or protocol exists depends on the host kernel's configuration and capabilities (F4, F7, F10, F12);
- a netlink socket opened here later returns live kernel state (F3). That nondeterminism shows up in later `recvmsg` calls, not in `socket` itself.

### Nondeterminism sources

- The host-wide socket inode counter.
- Host kernel configuration, loaded modules, capabilities and security policy.
- The network namespace: with host networking the guest's sockets reach real interfaces and real peers.

### Cross-host variation

- Host `RLIMIT_NOFILE` inherited by Hermit's guest: the kernel enforces that limit, not the virtual 1048576 that Detcore reports (`detcore/src/tool_local.rs:289`), so the descriptor count at which F12's `EMFILE` appears differs between hosts.
- Host kernel configuration and loaded modules: whether a domain such as `AF_VSOCK` or `AF_ALG`, or a protocol such as `IPPROTO_SCTP`, exists, so the same call succeeds on one host and gives `EAFNOSUPPORT` or `EPROTONOSUPPORT` on another (F4, F10). The guest cannot load modules from its user namespace.
- Host LSM policy: whether a socket is refused with `EACCES` (F15).

## Hermit design

Target policy: `model`. The kernel creates the socket, and Detcore owns its blocking behaviour, its place in the fd table and the sanitizing of what netlink sockets return. The dispatch arm at `detcore/src/lib.rs:2550` calls `handle_socket` at `detcore/src/syscalls/files.rs:3751`. The handler has two paths:

- **No sequentializing scheduler, or a record or replay run** (`detcore/src/syscalls/files.rs:3758`): the call goes through `record_or_replay` exactly as written, and the descriptor is added to the fd table (`detcore/src/syscalls/files.rs:3761`). The socket is not made nonblocking, so later blocking calls on it take the blocking path.
- **`hermit run` with the scheduler**: when `use_nonblocking_sockets()` is true (`detcore-model/src/config.rs:872`: `sequentialize_threads && !debug_externalize_sockets`), `SOCK_NONBLOCK` is added to the real call (`detcore/src/syscalls/files.rs:3772`). The fd table records only the flags the guest asked for, `SOCK_NONBLOCK` and `SOCK_CLOEXEC` (`detcore/src/syscalls/files.rs:3782`), and `maybe_set_nonblocking_fd` marks the descriptor as physically nonblocking (`detcore/src/syscalls/files.rs:3787`; helper at `detcore/src/syscalls/helpers.rs:695`). Later blocking calls are then retried under the scheduler instead of sleeping in the kernel (see the `connect` and `accept` bodies).
- On both paths, `mark_sock_diag_fd` (`detcore/src/syscalls/files.rs:3800`) flags `AF_NETLINK` sockets with `NETLINK_SOCK_DIAG`, so their dump replies get determinized inode numbers, and with `NETLINK_ROUTE` (`detcore/src/syscalls/files.rs:3814`), whose link dumps carry live interface counters.
- No domain, type or protocol is refused; host errors come back unchanged.

The network the socket sees is set by `hermit run --network` (`hermit-cli/src/bin/hermit/run.rs:495`). The default, `local`, gives the guest a private network namespace with only a loopback interface (`hermit-cli/src/bin/hermit/run.rs:905`). `host` exposes the host network, and the option's help text says it "compromises isolation and deterministic reproducibility". AF_UNIX sockets bound to paths on host-visible filesystems are not scoped by the network namespace and can still reach host services.

Why `model` and not the alternatives:

- `passthrough` would leave sockets blocking, so a later `accept` or `connect` would sleep in the kernel where the deterministic scheduler cannot see it.
- `refuse` would break every networked program, including local AF_UNIX clients.
- `emulate` (a user-space socket implementation) is far more work than the kernel-backed model and is not needed for loopback traffic inside the private namespace.

What remains before the code fully realizes `model`:

- **F13:** I found no code in this handler that hides the host socket inode number. Whether `fstat` or `/proc/self/fd` show it deterministically depends on other handlers; T7 checks it.
- **Record and replay path:** the flags stored in the fd table come from `OFlag::from_bits_truncate(type)` (`detcore/src/syscalls/files.rs:3764`), which reads the socket-type bits as open-mode bits (`SOCK_STREAM` = 1 = `O_WRONLY`, `SOCK_DGRAM` = 2 = `O_RDWR`). The run-mode path masks them out. Whether any later code reads those bits is not checked here; see Open questions.
- **F3:** only `NETLINK_SOCK_DIAG` and `NETLINK_ROUTE` replies are sanitized. Other netlink protocols that report live kernel state are forwarded as is.

Target (owner decision 14): every guest socket is physically nonblocking whenever Detcore schedules threads, so that later `accept`, `connect`, `recv*` and `send*` calls can always use a nonblocking probe plus a scheduler wait. Run mode does this today (`detcore/src/syscalls/files.rs:3772`-`3787`). On the record and replay path the socket stays blocking (`detcore/src/syscalls/files.rs:3758`), so those calls block in the kernel while holding their turn. The target removes that difference, recording each retried attempt as internal pipes already are (`retry_nonblocking_syscall`, `detcore/src/syscalls/helpers.rs:1414`). `socket` stays `model`.

## Recommended tests

- T1 (covers F2, F5, F13): create `AF_INET` and `AF_INET6` `SOCK_STREAM` sockets in a loopback listen, connect and accept flow under `hermit run --verify` on ptrace, and check the descriptor numbers are identical across runs.
  Existing candidates: `tests/c/socket_epoll_ordering.c:65` (cell `c-programs/socket-epoll-ordering`, `tests/e2e/manifests/c-programs.yaml:13718`, verify ci on ptrace); `tests/c/tcp_info_accept4.c:45` (cell `c-programs/tcp-info-accept4`, `tests/e2e/manifests/c-programs.yaml:9797`, verify ci on ptrace); `tests/c/tcp_info_accept6.c` (cell at `tests/e2e/manifests/c-programs.yaml:9855`).
- T2 (covers F1, F5): an `AF_UNIX` `SOCK_STREAM` socket, bound and queried.
  Existing candidate: `tests/c/bind_getsockname.c:43` (cell `c-programs/bind-getsockname`, `tests/e2e/manifests/c-programs.yaml:14534`, verify ci on ptrace).
- T3 (covers F6, F9): an `AF_INET` `SOCK_DGRAM | SOCK_CLOEXEC` socket; `fcntl(F_GETFD)` must report `FD_CLOEXEC`, and a plain `SOCK_DGRAM` socket must not.
  Existing partial candidate: `tests/c/ioctl_siocethtool.c:21` (cell `c-programs/ioctl-siocethtool`, `tests/e2e/manifests/c-programs.yaml:2535`) creates such a socket but does not check its flags.
- T4 (covers F8): `fcntl(F_GETFL)` reports `O_NONBLOCK` for a `SOCK_NONBLOCK` socket and does not report it for a plain socket, even though Hermit made the plain socket physically nonblocking. An `accept` on the plain socket must still wait for a connection instead of returning `EAGAIN`.
- T5 (covers F3): `AF_NETLINK` sockets with `NETLINK_ROUTE` and `NETLINK_SOCK_DIAG`; dump replies must be identical across runs.
  Existing candidate: `tests/c/netlink_autobind_route.c:4` (cell `c-programs/netlink-autobind-route`, `tests/e2e/manifests/c-programs.yaml:4417`, verify ci on ptrace).
- T6 (covers F4, F7, F10, F11): a raw `syscall(SYS_socket, ...)` error matrix, compared with native results: an unknown domain (`EAFNOSUPPORT`), `SOCK_STREAM` with `IPPROTO_UDP` (`EPROTONOSUPPORT`), `SOCK_RAW` without `CAP_NET_RAW` (`EPERM`), an undefined flag bit in `type` (`EINVAL`), and a successful `AF_UNIX` `SOCK_SEQPACKET`.
- T7 (covers F12, F13): close a low descriptor and check `socket` reuses it; lower `RLIMIT_NOFILE` and check `EMFILE`; print `fstat` `st_ino` and the `/proc/self/fd/N` link text (`socket:[N]`) and check both are identical across two runs.
- T8 (covers F1, F2, F5, F8): record and replay a guest that creates blocking and nonblocking sockets and runs a loopback exchange. Replay must return the same descriptors and the guest must see the same blocking modes.
- U1 (covers F14; host-transient): `ENFILE`, `ENOBUFS` and `ENOMEM` depend on host-wide state shared with other processes. The host call at `detcore/src/syscalls/files.rs:3777` returns the error to the guest unchanged today. F14 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F15; host-config): the CI host's LSM policy decides whether any socket is refused, so CI cannot produce `EACCES`. Hermit forwards the kernel's answer unchanged (`detcore/src/syscalls/files.rs:3777`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel creates, Detcore forces physical nonblocking in run mode and keeps the guest's logical flags); linux_determinism stays conditional (host socket inode counter, host kernel config); left todo with open questions on socket inode exposure and record-mode fd flags.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (the kernel enforces the host RLIMIT_NOFILE, not the virtual one, so the EMFILE point of F12 differs between hosts; host kernel configuration and modules decide which domains and protocols exist; host LSM); F12 split into F12 (EMFILE), F14 (ENFILE, ENOBUFS, ENOMEM; U1 host-transient) and F15 (LSM EACCES; U2 host-config); no open questions resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 applied (Target paragraph: sockets are physically nonblocking in record and replay too, so later blocking calls use the probe plus scheduler wait); no new F/T ids; no open questions removed
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/bind_getsockname.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `socket`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+socket&body=Discussion+for+the+%60socket%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsocket.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60socket%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `socket`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`socketpair` has its own handler (`detcore/src/syscalls/files.rs:3822`) and its own body.

### Open questions

- Is the host socket inode number visible to the guest through `fstat` or `/proc/self/fd`, and if so, should Detcore virtualize it the way the sock_diag path does for netlink dumps? T7 answers the first half.
- On the record and replay path, `OFlag::from_bits_truncate(type)` (`detcore/src/syscalls/files.rs:3764`) stores the socket type as if it were an access mode. If any later handler reads the access mode of a socket descriptor, this is a defect; the run-mode path's mask (`detcore/src/syscalls/files.rs:3782`) is probably the intended behaviour.
