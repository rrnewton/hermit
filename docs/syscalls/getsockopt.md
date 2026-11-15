---
schema: syscall-pages/v1
syscall: getsockopt
nr_x86_64: 55
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/609
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getsockopt&body=Discussion+for+the+%60getsockopt%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetsockopt.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getsockopt%60%2C+or+in+what+the+page+says%3F%0A
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

Reads an option value from a socket. Most values are configuration the guest set itself or fixed facts about the socket, but some are host identities, counters or timings: socket cookies, the CPU that last handled a packet, TCP statistics, peer credentials. Detcore rewrites four of these and lets the kernel answer the rest.

Manual reference: [getsockopt(2)](https://man7.org/linux/man-pages/man2/getsockopt.2.html).

## Linux semantics

`getsockopt(fd, level, optname, optval, optlen)` asks the handler for `level` to copy the option's value into `optval`. `*optlen` is a value-result argument: on entry it is the size of the buffer, and on success the kernel stores the number of bytes it wrote. Most `int` options accept a shorter buffer and are truncated. Some options require an exact or minimum length and give `EINVAL` otherwise. On error the kernel writes neither `optval` nor `optlen`.

Values come from three places:

- configuration set earlier with `setsockopt`, sometimes changed on the way in (`SO_RCVBUF` is stored doubled)
- fixed facts about the socket, such as its type and domain
- live kernel state: pending errors, peer credentials, cookies from a host-global counter, the CPU that last handled a packet, and protocol statistics such as `TCP_INFO`

### Flags and options

- F1: `optlen` handling: a value-result length; a short buffer truncates an `int` option; a NULL `optval` with `*optlen` 0 succeeds and stores 0.
- F2: fixed facts about the socket: `SO_TYPE`, `SO_DOMAIN`, `SO_PROTOCOL`, `SO_ACCEPTCONN`.
- F3: reading back configuration the guest set: boolean and integer `SOL_SOCKET` options (`SO_REUSEADDR`, `SO_KEEPALIVE`, `SO_BROADCAST`, `SO_PRIORITY`, `SO_RCVLOWAT`, `SO_PEEK_OFF`), `SO_LINGER`, and `SO_RCVTIMEO`/`SO_SNDTIMEO`.
- F4: `SO_RCVBUF` / `SO_SNDBUF`: the stored size, which is twice the requested value, clamped by host sysctls. Without a `setsockopt`, the default comes from host sysctls, and TCP autotuning can change it while the connection runs.
- F5: `SO_ERROR`: returns and clears the pending socket error, for example the result of a nonblocking `connect`. Whether an error is pending depends on when the call runs.
- F6: peer credentials: `SO_PEERCRED` (pid, uid and gid of the peer when it connected), `SO_PEERGROUPS`, `SO_PEERSEC`, `SO_PEERPIDFD`.
- F7: `SO_COOKIE`: a 64-bit cookie identifying the socket, taken from a host-global kernel counter. The buffer must hold 8 bytes, otherwise `EINVAL`.
- F8: `SO_NETNS_COOKIE`: a 64-bit cookie identifying the socket's network namespace, also from a host-global counter.
- F9: `SO_INCOMING_CPU`: the CPU that last processed a packet for the socket, or -1 if none has arrived.
- F10: `SO_MEMINFO`: the socket's memory counters (receive and send allocations, backlog, drops).
- F11: `SO_BINDTODEVICE` / `SO_BINDTOIFINDEX`: the bound interface's name or index.
- F12: `TCP_INFO`: a `struct tcp_info` with the connection state, negotiated options and window scales, and also RTT estimates, timers and packet and byte counters.
- F13: other `IPPROTO_TCP` values: `TCP_CONGESTION` (the algorithm name, a host default), `TCP_MAXSEG` (depends on the route MTU), `TCP_CC_INFO` (congestion-control counters), and read-back of `TCP_NODELAY`, `TCP_CORK` and the keepalive timers.
- F14: `IPPROTO_IP` / `IPPROTO_IPV6` values: `IP_TOS`, `IP_TTL` (default from a sysctl), `IP_MTU` (route state), `IPV6_V6ONLY` (default from `net.ipv6.bindv6only`), `SO_ORIGINAL_DST`.
- F15: other levels: `SOL_NETLINK` (`NETLINK_LIST_MEMBERSHIPS`), `SOL_PACKET` (`PACKET_STATISTICS`, whose counters are cleared when read), `SOL_UDP`.
- F16: errors: `EBADF`, `ENOTSOCK`, `EFAULT`, `EINVAL` (a length the option does not accept), `ENOPROTOOPT` (unknown level or option).

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. F1, F2 and F3 return the same value on every run once the guest's own `setsockopt` calls are the same. The other options can differ between runs or between hosts:

- host-global counters: F7, F8
- which CPU ran the network stack: F9
- timing and peer activity: F5, F10, F12, F13, F15
- host sysctls, routes and modules: F4, F13, F14
- process identities: F6

The call repeats when every peer is in one deterministic schedule in a private network namespace, and the options read are configuration, fixed facts, or values Detcore rewrites.

### Nondeterminism sources

- Host-global cookie counters (F7, F8)
- Which CPU processed a packet (F9)
- TCP timers, RTT estimates and packet counters (F12, F13)
- Memory accounting and drop counters (F10, F15)
- Host sysctls and routes: default buffer sizes, TTL, MTU, `bindv6only`, the default congestion algorithm (F4, F13, F14)
- When asynchronous errors arrive (F5)
- Peer process ids (F6)

### Cross-host variation

- Host socket-buffer sysctls (`net.core.rmem_default`, `net.core.wmem_default`, `net.core.rmem_max`, `net.core.wmem_max`): these are global, not per network namespace, so they set the default `SO_RCVBUF`/`SO_SNDBUF` of UNIX and UDP sockets and the clamp on requested sizes inside the guest's private namespace too (F4). Detcore returns the kernel's value unchanged.
- Host default TCP congestion algorithm (`net.ipv4.tcp_congestion_control`) and the congestion modules loaded: a new network namespace starts with the host's default, so `TCP_CONGESTION` reads it (F13).
- Host LSM: whether `SO_PEERSEC` returns a security label or `ENOPROTOOPT`, and which label (F6).

## Hermit design

Target policy: `model`. The kernel answers, and Detcore rewrites the values that would leak host identity or timing. It does this for four options today.

What the code does at f4055eb9:

- The arm at `detcore/src/lib.rs:2568` calls `handle_getsockopt` (`detcore/src/syscalls/files.rs:3914`). The doc comment at `detcore/src/syscalls/files.rs:3911` promises run determinism only for isolated guest networking, which is the default `--network=local`: the guest gets its own network namespace with only loopback (`hermit-cli/src/bin/hermit/global_opts.rs:53`).
- The kernel call goes through `record_or_replay` (`detcore/src/syscalls/files.rs:3951`; helper at `detcore/src/tool_local.rs:382`). Run mode executes it unchanged, record mode saves the result and the bytes written, and replay returns them.
- **F7 `SO_COOKIE`** (`detcore/src/syscalls/files.rs:3936` and `detcore/src/syscalls/files.rs:3988`): before the call, Detcore reads `*optlen` and the open file's identity. Afterwards it writes `deterministic_socket_cookie()` (`detcore-model/src/fd.rs:164`): the creating thread's id in the high 32 bits and a per-creator socket sequence number in the low 32. Unit tests: `deterministic_socket_cookies_track_open_file_identity` at `detcore-model/src/fd.rs:175` and `regular_file_opens_do_not_shift_socket_cookie_identity` at `detcore/src/tool_local.rs:3195`.
- **F8 `SO_NETNS_COOKIE`** (`detcore/src/syscalls/files.rs:3919` and `detcore/src/syscalls/files.rs:3979`): for a tracked socket, Detcore writes the constant `DETERMINISTIC_NETNS_COOKIE` = 1 (`detcore/src/syscalls/files.rs:389`), because the guest has exactly one network namespace.
- **F9 `SO_INCOMING_CPU`** (`detcore/src/syscalls/files.rs:3953`): after a successful call, the value is replaced with 0, since Hermit presents one virtual CPU.
- **F12 `TCP_INFO`** (`detcore/src/syscalls/files.rs:3967`): after a successful call, `canonicalize_tcp_info` (`detcore/src/syscalls/files.rs:380`) keeps bytes 0, 1, 5 and 6 (`tcpi_state`, `tcpi_ca_state`, `tcpi_options` and the window-scale byte) and zeroes every timing, rate, packet and byte counter.
- Every other option is returned as the kernel produced it.

Why `model`: the kernel already keeps every option's value, and the call is safe to run as is. Only the leaking values need rewriting. `emulate` would duplicate the socket layer. `refuse` would break common calls such as `SO_ERROR` after a nonblocking `connect`, and `SO_TYPE`.

What remains before the code fully realizes `model`. These are hypotheses read from the code, not measurements:

- **Cookie writes are not gated on success.** The F7 and F8 rewrites run whether or not the kernel call succeeded; only F9 and F12 check `result == 0`. Linux writes nothing on error, but Detcore still writes cookie bytes into `optval` when, for example, `SO_COOKIE` is given a 4-byte buffer (`EINVAL`) or used on a tracked fd that is not a socket (`ENOTSOCK`). The return value stays correct; only the buffer differs. T5 checks this.
- **Untracked descriptors.** `SO_COOKIE` calls `with_detfd` before the kernel call (`detcore/src/syscalls/files.rs:3945`), so a descriptor Detcore does not track gets Detcore's error, not the kernel's.
- **F9 on an idle socket.** Linux returns -1 when no packet has arrived; Detcore replaces it with 0.
- **F4 sizes.** The value read back depends on host sysctls and on TCP autotuning. `tests/c/socket_options.c:12` leaves `SO_SNDBUF` out for this reason.
- **F5 timing.** Whether `SO_ERROR` is already set depends on when the kernel finished the connect. With every peer in the guest and connects retried under the scheduler, this should repeat, but it is not tested.
- **F6 identities.** `SO_PEERCRED` returns kernel pids. The guest runs in its own PID namespace (`default_container`, `hermit-cli/src/bin/hermit/container.rs:442`), so the pids repeat when processes are created in the same order. uid and gid are those of the mapped root user.
- **F10, F13, F14, F15 counters and host defaults** pass through unchanged: `SO_MEMINFO`, `TCP_CC_INFO`, `TCP_MAXSEG`, `IP_MTU`, `PACKET_STATISTICS`, and the default congestion algorithm. They need the same treatment as `TCP_INFO`, or a measurement showing they repeat in a loopback-only namespace.

`linux_determinism` stays `conditional`. Detcore removes the cookie, CPU and TCP statistics sources. Host sysctl defaults and the counters listed above remain.

## Recommended tests

- T1 (covers F1, F2): read `SO_TYPE`, `SO_DOMAIN`, `SO_PROTOCOL` and `SO_ACCEPTCONN` with full and short buffers, and with a NULL `optval` and `*optlen` 0, under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/getsockopt_null.c:21` (cell `c-programs/getsockopt-null`, program at `tests/e2e/manifests/c-programs.yaml:2057`, verify ci on ptrace; recorded by `record_c_getsockopt_null` at `hermit-cli/tests/record_replay.rs:2315`) and `tests/c/socketpair_flags.c:51` (program at `tests/e2e/manifests/c-programs.yaml:15519`).
- T2 (covers F3, F4): set and read back the F3 options, then read `SO_RCVBUF`/`SO_SNDBUF` with and without a prior `setsockopt`. The values must match across runs under `--verify`; record whether the defaults differ between hosts.
  Existing candidate: `tests/c/socket_options.c` (program at `tests/e2e/manifests/c-programs.yaml:14358`, verify ci on ptrace), which reads back `SO_REUSEADDR`, `SO_KEEPALIVE` and `SO_BROADCAST`.
- T3 (covers F5, F6): a nonblocking loopback `connect` to a port with no listener and to a listening port, then `SO_ERROR`. On a UNIX socketpair, read `SO_PEERCRED` and `SO_PEERPIDFD` from a forked child. Values must match across runs under `--verify`.
- T4 (covers F7, F8): `SO_COOKIE` is non-zero, different for different sockets and the same across `dup`; `SO_NETNS_COOKIE` is the same for every socket. Both must match across runs.
  Existing candidates: `tests/c/socket_cookie_common.h`, used by `tests/c/socket_cookie_unix.c`, `tests/c/socket_cookie_tcp.c` and `tests/c/socket_cookie_udp.c` (programs at `tests/e2e/manifests/c-programs.yaml:8788`, `:8672` and `:8730`, verify ci on ptrace), and `tests/c/netns_cookie_tcp4.c`, `tests/c/netns_cookie_tcp6.c` and `tests/c/netns_cookie_udp4.c` (programs at `tests/e2e/manifests/c-programs.yaml:4551`, `:4609` and `:4667`).
- T5 (covers F7, F8, F16): fill `optval` with a known pattern and call `SO_COOKIE` with a 4-byte buffer, and on a regular file. Linux returns `EINVAL` and `ENOTSOCK` and leaves the buffer alone. Record whether Detcore writes cookie bytes, as the code suggests. Also check `EBADF`, `EFAULT` and `ENOPROTOOPT`.
- T6 (covers F9): `SO_INCOMING_CPU` after loopback traffic must be 0. On a socket with no traffic, record the Hermit value against the Linux -1.
  Existing candidates: `tests/c/so_incoming_cpu_tcp4.c`, `tests/c/so_incoming_cpu_tcp6.c` and `tests/c/so_incoming_cpu_udp4.c` (programs at `tests/e2e/manifests/c-programs.yaml:8492`, `:8556` and `:8614`, verify ci on ptrace).
- T7 (covers F12, F13): after a loopback TCP transfer, `TCP_INFO` must match across runs, with state and options kept and counters zero. Read `TCP_CONGESTION`, `TCP_MAXSEG` and `TCP_CC_INFO` and check whether they repeat.
  Existing candidates: `tests/c/tcp_info_accept4.c`, `tests/c/tcp_info_accept6.c` and `tests/c/tcp_info_client4.c` (programs at `tests/e2e/manifests/c-programs.yaml:9807`, `:9865` and `:9923`, verify ci on ptrace).
- T8 (covers F10, F11, F14, F15): read `SO_MEMINFO` after a transfer, `SO_BINDTODEVICE` after binding to "lo", `IP_TTL`, `IP_MTU` and `IPV6_V6ONLY` defaults, `NETLINK_LIST_MEMBERSHIPS`, and `PACKET_STATISTICS` where permitted. Record which values differ between two `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/609: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel answers via record_or_replay; Detcore rewrites SO_COOKIE, SO_NETNS_COOKIE, SO_INCOMING_CPU, TCP_INFO in detcore/src/syscalls/files.rs:3914); linux_determinism conditional kept; 16 flags, 8 tests; gaps: cookie writes not gated on success (hypothesis, T5), SO_MEMINFO/TCP_CC_INFO/IP_MTU pass through; no coverage recorded, so todo.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (global net.core socket-buffer sysctls, the host default TCP congestion algorithm inherited by the guest's network namespace, host LSM for SO_PEERSEC); no U items; no open questions


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getsockopt`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getsockopt&body=Discussion+for+the+%60getsockopt%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetsockopt.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getsockopt%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getsockopt" (https://github.com/rrnewton/hermit/issues/609), legacy variant B.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/609.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. At that time the implementation classified `PASS-THRU` entries under fixed-container and stable-state assumptions, and sent every other entry to the explicit unsupported policy until a deterministic handler existed. At f4055eb9 `getsockopt` has that handler (`detcore/src/syscalls/files.rs:3914`).
