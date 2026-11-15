---
schema: syscall-pages/v1
syscall: setsockopt
nr_x86_64: 54
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/459
  - https://github.com/rrnewton/hermit/issues/466
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setsockopt&body=Discussion+for+the+%60setsockopt%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetsockopt.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setsockopt%60%2C+or+in+what+the+page+says%3F%0A
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

Sets an option on a socket: generic `SOL_SOCKET` options such as buffer sizes, timeouts and timestamps, or options for a protocol such as `TCP_NODELAY`. The call itself returns only 0 or an error. Its effect shows up in later socket I/O and in `getsockopt`.

Manual reference: [setsockopt(2)](https://man7.org/linux/man-pages/man2/getsockopt.2.html).

## Linux semantics

`setsockopt(fd, level, optname, optval, optlen)` copies `optlen` bytes from `optval` and passes them to the handler for `level`. `SOL_SOCKET` is handled by the generic socket layer. `IPPROTO_TCP`, `IPPROTO_IP`, `IPPROTO_IPV6`, `SOL_UDP`, `SOL_NETLINK` and other levels go to that protocol's own handler. An option that the level does not know gives `ENOPROTOOPT`.

The option state lives on the socket in the kernel. Nothing is returned except the status. Some options are stored exactly as given; others are changed on the way in. For example, `SO_RCVBUF` and `SO_SNDBUF` are doubled and clamped to the host's `net.core.rmem_max`/`wmem_max`. Options that need a capability, such as the `*BUFFORCE` options, `SO_MARK` and `IP_TRANSPARENT`, check it against the user namespace that owns the socket's network namespace.

Two host probes are on record (see Additional information): enabling `SO_KEEPALIVE` on a local UNIX socket succeeded, and real programs were seen setting `TCP_NODELAY`.

### Flags and options

- F1: `SOL_SOCKET` boolean options that only change local behaviour: `SO_REUSEADDR`, `SO_REUSEPORT`, `SO_KEEPALIVE`, `SO_BROADCAST`, `SO_DONTROUTE`, `SO_OOBINLINE`, `SO_PASSCRED`, `SO_PASSSEC`, `SO_ZEROCOPY`. The value is an `int`; any non-zero value enables the option.
- F2: `SO_RCVBUF` / `SO_SNDBUF`: the kernel stores twice the requested size, clamped to the host's `rmem_max`/`wmem_max` and raised to a minimum. The stored size decides when later writes come back short or with `EAGAIN`.
- F3: `SO_RCVBUFFORCE` / `SO_SNDBUFFORCE`: like F2 but ignore the sysctl limits. They need `CAP_NET_ADMIN`; without it they fail with `EPERM`.
- F4: `SO_RCVTIMEO` / `SO_SNDTIMEO` (old and `_NEW` forms): timeouts for blocking receive and send. When the timeout expires, the call returns a partial count or `EAGAIN`/`EWOULDBLOCK` (`EINPROGRESS` for `connect`). A `timeval` out of range gives `EDOM`.
- F5: `SO_RCVLOWAT`: the minimum number of bytes before a blocking receive returns. Setting `SO_SNDLOWAT` fails with `ENOPROTOOPT` on Linux.
- F6: `SO_LINGER`: with `l_onoff` set, `close` on a connected stream socket waits up to `l_linger` seconds for unsent data. `l_linger` 0 resets the connection instead.
- F7: `SO_TIMESTAMP`, `SO_TIMESTAMPNS`, `SO_TIMESTAMPING` (old and `_NEW` forms): ask for receive timestamps, delivered later as `SCM_TIMESTAMP*` control messages from `recvmsg`.
- F8: routing and marking: `SO_PRIORITY`, `SO_MARK` (needs `CAP_NET_ADMIN`), `SO_BINDTODEVICE` / `SO_BINDTOIFINDEX`. A device name or index that does not exist in the namespace gives `ENODEV`.
- F9: socket filters: `SO_ATTACH_FILTER`, `SO_ATTACH_BPF`, `SO_DETACH_FILTER`, `SO_LOCK_FILTER`, `SO_ATTACH_REUSEPORT_CBPF` / `SO_ATTACH_REUSEPORT_EBPF`.
- F10: CPU, polling and pacing options: `SO_INCOMING_CPU`, `SO_BUSY_POLL`, `SO_PREFER_BUSY_POLL`, `SO_MAX_PACING_RATE`, `SO_TXTIME`.
- F11: `SO_PEEK_OFF`: the offset at which `MSG_PEEK` reads start.
- F12: TCP batching options at `IPPROTO_TCP`: `TCP_NODELAY`, `TCP_CORK`, `TCP_QUICKACK`. They change when segments are sent, not what bytes arrive.
- F13: TCP timer options: `TCP_KEEPIDLE`, `TCP_KEEPINTVL`, `TCP_KEEPCNT`, `TCP_USER_TIMEOUT`, `TCP_DEFER_ACCEPT`, `TCP_SYNCNT`, `TCP_LINGER2`.
- F14: TCP options whose success depends on the host: `TCP_CONGESTION` (an algorithm that is not available gives `ENOENT`), `TCP_ULP` (needs the module), `TCP_FASTOPEN`, `TCP_MAXSEG`.
- F15: IP options at `IPPROTO_IP` and `IPPROTO_IPV6`: `IP_TOS`, `IP_TTL`, `IP_RECVERR`, `IP_PKTINFO`, `IP_FREEBIND`, `IP_TRANSPARENT` (needs `CAP_NET_ADMIN`), multicast membership and loop options, `IPV6_V6ONLY`.
- F16: other levels, each with its own handler: `SOL_UDP` (`UDP_CORK`, `UDP_SEGMENT`), `SOL_NETLINK` (`NETLINK_ADD_MEMBERSHIP` and others), `SOL_PACKET`, `SOL_ALG`, `SOL_TLS`.
- F17: errors: `EBADF` (bad descriptor), `ENOTSOCK` (not a socket), `EFAULT` (bad `optval`), `EINVAL` (`optlen` too small or a bad value), `ENOPROTOOPT` (unknown level or option), `EPERM` (missing capability).

## Determinism analysis

`linux_determinism: conditional`, kept from both legacy issues. The call returns only a status. That status is the same on every run for options that need no privilege and have no host-dependent input (F1, F5, F11, F12, F13, F16 with fixed arguments, F17). It depends on the host for options that check a capability (F3, F8, F15), that name a device (F8), or that need a kernel module or algorithm (F14).

Most of the nondeterminism is not in this call. It shows up later, in the I/O the option affects:

- buffer sizes (F2, F3) decide when a write comes back short or with `EAGAIN`
- timeouts (F4, F6, F13) depend on wall-clock time and peer activity
- timestamps (F7) carry host time in later `recvmsg` control messages
- batching options (F12) change segment timing, which only a host peer can see

The call repeats when every peer is inside one deterministic schedule in a private network namespace and the options used do not depend on host configuration.

### Nondeterminism sources

- Protocol implementation and kernel version: which options and algorithms exist
- The network namespace and its devices: `SO_BINDTODEVICE` targets and multicast routes
- Host sysctl limits: the `rmem_max`/`wmem_max` clamp on `SO_RCVBUF`/`SO_SNDBUF`
- Credentials: the capability check on `CAP_NET_ADMIN`-gated options
- Peer activity and timing: whether a timeout expires and what timestamps say

### Cross-host variation

- Congestion-control algorithms and ULP modules the host kernel has built or loaded: whether `TCP_CONGESTION` with a real algorithm name (for example "bbr") or `TCP_ULP` "tls" succeeds or gives `ENOENT` differs between hosts (F14). The call is forwarded unchanged (`detcore/src/syscalls/files.rs:3873`).
- Host kernel version: options added in newer kernels (for example `SO_PREFER_BUSY_POLL`, `SO_TXTIME`, `UDP_SEGMENT`) succeed on one host and give `ENOPROTOOPT` or `EINVAL` on an older one (F10, F16, F17).

## Hermit design

Target policy: `model`. The kernel stays the owner of option state. Detcore makes the call reproducible by isolating the network and by putting every later I/O operation the option affects under its own scheduler.

What the code does at f4055eb9:

- The arm at `detcore/src/lib.rs:2556` sends the call to `handle_setsockopt` (`detcore/src/syscalls/files.rs:3868`). The arm and the handler carry `AUTONOMOUS-BOT-IMPLEMENTED` and `TODO-HUMAN-REVIEW(#663)` markers.
- The handler body is a single `record_or_replay` (`detcore/src/syscalls/files.rs:3873`; helper at `detcore/src/tool_local.rs:382`). In run mode the kernel executes the call unchanged. Record mode saves the result. Per the doc comment at `detcore/src/syscalls/files.rs:3865`, replay re-applies a successful option before the socket I/O that follows. No option is read or rewritten.
- The default `--network=local` gives the guest its own network namespace with only loopback (`hermit-cli/src/bin/hermit/global_opts.rs:53`). Every peer is then a guest process, and device-dependent options (F8, F15) see the same single interface on every run. `--network=host` gives up that isolation.
- Later blocking I/O does not block in the kernel. Sockets are made physically nonblocking when threads are sequentialized (`maybe_set_nonblocking_fd`, `detcore/src/syscalls/helpers.rs:695`), and reads and writes are retried under the scheduler (`execute_nonblockable_fd_syscall`, `detcore/src/syscalls/helpers.rs:202`). The effect of F2 and F12 on readiness therefore reaches the guest through scheduler-ordered retries.
- Timestamps enabled with F7 are rewritten when they arrive: `recvmsg` finds `SCM_TIMESTAMP*` control messages (`detcore/src/syscalls/io.rs:272`, constants at `detcore/src/syscalls/io.rs:215`) and replaces their contents with Hermit time.

Why `model` and not the alternatives:

- `passthrough` alone is not enough: the option's effects have to be kept in step with the scheduler, as F4 and F7 show.
- `emulate` would mean keeping a model of every socket option outside the kernel, which duplicates the network stack.
- `refuse` would break almost every network server, and most clients.

What remains before the code fully realizes `model`. These are hypotheses read from the code, not measurements:

- **F4 timeouts.** No code in `detcore/src` reads `SO_RCVTIMEO` or `SO_SNDTIMEO`. A blocking receive on a socket is retried until it succeeds, and the retry with a logical timeout (`retry_nonblocking_syscall_with_timeout`, `detcore/src/syscalls/helpers.rs:1432`) is used by poll, epoll, futex and signal waits, not by socket reads. A guest that sets `SO_RCVTIMEO` and waits on a socket that never gets data may then wait forever under Hermit, where Linux returns `EAGAIN` once the timeout expires. T4 checks this.
- **F6 linger.** A lingering `close` can sleep in the kernel, outside the scheduler, until the peer acknowledges or `l_linger` expires. With every peer in the guest this ends, but the time it takes is host time. T5 checks it.
- **F2 and F3 sizes.** The kernel's clamp uses host sysctls, so the stored size, and with it the point where writes come back short, can differ between hosts. `tests/c/socket_options.c:12` already leaves `SO_SNDBUF` out for this reason. The size read back is covered by the getsockopt issue (https://github.com/rrnewton/hermit/issues/609).
- **Capability-gated options (F3, F8, F15).** Inside Hermit's user namespace the guest may hold `CAP_NET_ADMIN` over its own network namespace, so a call that fails with `EPERM` natively may succeed under Hermit. That is consistent run to run, but it differs from native. T3 and T8 record both results.

`linux_determinism` stays `conditional`. Hermit removes the peer and device sources by isolating the network. Host sysctl limits and module availability stay outside its control.

Target (owner decisions 14 and 13): `SO_RCVTIMEO` and `SO_SNDTIMEO` (F4) bound Detcore's scheduler wait, not a kernel sleep, and are measured on virtual time. Detcore must learn the value, either by keeping it in its fd table when this call succeeds (the kernel still validates it, so `EDOM` keeps Linux's order) or by reading it back with `getsockopt` when a wait starts. The socket waits in `accept`, `connect`, `recv*` and `send*` then end at that virtual-time deadline with Linux's result: `EAGAIN` or a partial count, and `EINPROGRESS` for `connect`. Today `handle_setsockopt` (`detcore/src/syscalls/files.rs:3868`) only forwards the value to the kernel, where it has no effect on the physically nonblocking socket in run mode and measures host time on the blocking path. `SO_LINGER` (F6) falls under the same rule: a lingering `close` must not sleep in the kernel while holding its turn, and `l_linger` is virtual time; that change belongs to `close`. `setsockopt` stays `model`.

## Recommended tests

- T1 (covers F1, F17): on a UNIX socketpair, set and read back each F1 option under `hermit run --verify` on ptrace. Also check the errors: `EBADF` for a closed fd, `ENOTSOCK` for a regular file, `EINVAL` for `optlen` 0, `ENOPROTOOPT` for an unknown option, and `EFAULT` for a bad `optval`.
  Existing candidate: `tests/c/socket_options.c`, which round-trips `SO_REUSEADDR`, `SO_KEEPALIVE` and `SO_BROADCAST` (cell `c-programs/socket-options`, program at `tests/e2e/manifests/c-programs.yaml:14358`, verify ci on ptrace). It has no error cases.
- T2 (covers F1): record and replay a guest that sets `SO_REUSEADDR` and `SO_REUSEPORT` and then binds two sockets to one port. Replay must re-apply both options, so the second bind succeeds again.
  Existing candidate: `tests/c/record_replay_setsockopt.c` (program at `tests/e2e/manifests/c-programs.yaml:7177`), run by `record_c_setsockopt_replay` at `hermit-cli/tests/record_replay.rs:2316`.
- T3 (covers F2, F3): set `SO_RCVBUF`/`SO_SNDBUF` to a small and a large value, read them back, and fill the socket until `EAGAIN`. Run it twice under `--verify`; the results must match. Record what `SO_RCVBUFFORCE` returns natively and under Hermit.
- T4 (covers F4, F5): on a socketpair with no writer, set `SO_RCVTIMEO` to 50 ms and call a blocking `recv`. Linux returns `EAGAIN`. Record what Hermit does. A hang confirms the gap above. Also check `SO_RCVLOWAT`, and that `SO_SNDLOWAT` gives `ENOPROTOOPT`.
- T5 (covers F6): a loopback TCP pair with `SO_LINGER` {1, 1} and with {1, 0}. Check the `close` return value and what the peer sees (EOF or `ECONNRESET`), under `--verify`.
- T6 (covers F7): enable `SO_TIMESTAMP` and receive a datagram. The control message must hold Hermit time, the same in every run.
  Existing candidates: `tests/c/socket_timestamp_timeval.c:31` (cell `c-programs/socket-timestamp-timeval`, program at `tests/e2e/manifests/c-programs.yaml:9032`, verify ci on ptrace) and `tests/c/socket_timestamp_edge_cases.c:48` (program at `tests/e2e/manifests/c-programs.yaml:8911`).
- T7 (covers F12, F13, F14): on a loopback TCP pair, set `TCP_NODELAY` (the call seen in the strace from https://github.com/rrnewton/hermit/issues/466), `TCP_CORK`, `TCP_KEEPIDLE`, `TCP_USER_TIMEOUT`, `TCP_CONGESTION` "reno" and a name that does not exist. Check every return value and that the bytes delivered are the same under `--verify`.
- T8 (covers F8, F9, F10, F11, F15, F16): set `SO_PRIORITY`, `SO_BINDTODEVICE` "lo" and a missing device (`ENODEV`), attach an accept-all classic BPF filter, set `SO_PEEK_OFF`, `SO_BUSY_POLL`, `IP_TOS`, `IPV6_V6ONLY`, `UDP_SEGMENT` and a netlink membership. Record each return value natively and under Hermit; under `--verify` they must match run to run.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/459: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/466 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel owns option state; record_or_replay at detcore/src/syscalls/files.rs:3873, isolated network, scheduler-mediated I/O); linux_determinism conditional kept; merged duplicate https://github.com/rrnewton/hermit/issues/466; 17 flags, 8 tests; open gaps: SO_RCVTIMEO/SO_SNDTIMEO not honoured (hypothesis, T4), lingering close, host buffer clamps; no coverage recorded, so todo.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host-loaded congestion algorithms and ULP modules, kernel version for newer options); no U items, since ENOENT and ENOPROTOOPT are reachable on CI with an unknown name or option; no open questions
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 13 and 14 applied (Target paragraph: SO_RCVTIMEO and SO_SNDTIMEO bound the scheduler wait on virtual time; SO_LINGER close follows the same rule); no new F/T ids; no open questions
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by hermit-cli/tests/record_replay.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setsockopt`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setsockopt&body=Discussion+for+the+%60setsockopt%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetsockopt.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setsockopt%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setsockopt" (https://github.com/rrnewton/hermit/issues/459), legacy variant A1. Its duplicate https://github.com/rrnewton/hermit/issues/466 was filed the same day with the same title (legacy variant A1); its comment history (0 comments) stays on that issue. Its text was merged into this body on 2026-10-03.

How the two legacy texts were reconciled:

- **Recommended action.** https://github.com/rrnewton/hermit/issues/459 said NEEDS-HANDLER; https://github.com/rrnewton/hermit/issues/466 said NEEDS-INVESTIGATION. Both are superseded: a handler now exists (`detcore/src/syscalls/files.rs:3868`), and this body chooses `model`.
- **Classification.** Both said CONDITIONAL; kept.
- **Facts kept from https://github.com/rrnewton/hermit/issues/466:** the list of options that change I/O timing (`SO_RCVTIMEO`/`SO_SNDTIMEO`, `SO_RCVBUF`, `TCP_NODELAY`; now F2, F4, F12), the point that socket behaviour is part of the network I/O Hermit determinizes at read and write time, and its strace line.
- **Superseded claim in both:** that the syscall went through a temporary catch-all forwarding list in `detcore/src/syscall_classification.rs`. That was true at `dd60278f`; at f4055eb9 the call has a typed handler.

### 2026-07-24 host strace evidence

Both probes ran on `the measurement host` (Linux 6.17, x86_64, glibc).

From https://github.com/rrnewton/hermit/issues/459: a direct `syscall()` harness, with safe self-targeting, no-op, temporary-file or deliberately invalid arguments, traced with `strace -qq -f -e trace=setsockopt`. The harness exited 0.

```text
setsockopt(3, SOL_SOCKET, SO_KEEPALIVE, [1], 4) = 0
```

From https://github.com/rrnewton/hermit/issues/466: `strace -f -e trace=setsockopt` on a direct `syscall()` harness and on common programs. This line came from a real program:

```text
setsockopt(19, SOL_TCP, TCP_NODELAY, [1], 4) = 0
```

---
*Both legacy issues came from the autonomous syscall-classification sweep: https://github.com/rrnewton/hermit/issues/459 from batch 5 (ranks 201-250), https://github.com/rrnewton/hermit/issues/466 from ranks 151-250, of the list of syscalls that had no policy in `syscall_classification.rs`. Manual research plus host strace evidence; neither proposed a code change.*
