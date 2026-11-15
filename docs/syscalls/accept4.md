---
schema: syscall-pages/v1
syscall: accept4
nr_x86_64: 288
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+accept4&body=Discussion+for+the+%60accept4%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Faccept4.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60accept4%60%2C+or+in+what+the+page+says%3F%0A
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

`accept4(sockfd, addr, addrlen, flags)` takes the first pending connection off a listening stream socket. It returns a new connected socket, can write the peer's address, and sets `SOCK_NONBLOCK` and `SOCK_CLOEXEC` on the new socket. Detcore handles the call at `detcore/src/syscalls/files.rs:4526`, for `accept` as well.

- Under the sequentialized scheduler, Detcore makes the returned socket nonblocking in the kernel, and turns a blocking wait into polling that the scheduler controls.
- It records the flags the guest asked for, and those are what the guest sees.
- The peer address is passed through as the kernel wrote it.

Manual reference: Linux man-pages accept(2).

## Linux semantics

`sockfd` must be a socket that is listening and connection-oriented (`SOCK_STREAM` or `SOCK_SEQPACKET`).

**The pending queue.** The kernel removes the oldest fully established connection from the queue and installs it as the lowest free descriptor. If the queue is empty, the call waits, or fails with `EAGAIN` when `sockfd` is `O_NONBLOCK`. A blocking wait can end with `EINTR`. Linux restarts the call after an `SA_RESTART` handler, unless the socket has a receive timeout (`SO_RCVTIMEO`), which makes it fail with `EAGAIN` when the timeout expires.

**The returned socket.** It does not inherit file status flags such as `O_NONBLOCK` from the listener. Its nonblocking and close-on-exec state come only from `flags`.

**The peer address.** If `addr` is not NULL, the kernel writes the peer's address there, cut to `*addrlen` bytes, and stores the full length in `*addrlen`. For TCP the address is the peer's IP and port. If the peer called `connect` without `bind`, that port is an ephemeral port the peer's kernel chose. For an AF_UNIX peer that never bound, the address is only `sun_family`, and `*addrlen` is `sizeof(sa_family_t)`.

**Errors.**
- On the listener: `EBADF`, `ENOTSOCK`, `EINVAL` (not listening, or a bad `flags` or `addrlen`), `EOPNOTSUPP` (not a stream socket).
- On the user buffers: `EFAULT`.
- Resources: `EMFILE`, `ENFILE`, `ENOBUFS`, `ENOMEM`.
- `ECONNABORTED` when a queued connection was aborted.

**`accept`.** The older `accept(sockfd, addr, addrlen)` (nr 43) is `accept4` with `flags = 0`.

### Flags and options

- F1: `flags = 0`: the new socket is blocking and not close-on-exec. This is also `accept` (nr 43).
- F2: `SOCK_NONBLOCK`: the new socket's description is `O_NONBLOCK`
- F3: `SOCK_CLOEXEC`: the new descriptor is close-on-exec
- F4: any other bit in `flags`: `EINVAL`
- F5: `addr` and `addrlen` are NULL: no peer address is returned
- F6: `addr` is a buffer: the peer address is written, cut to `*addrlen` bytes, and `*addrlen` is set to the full length. A negative `*addrlen` is `EINVAL`; unwritable `addr` or `addrlen` memory is `EFAULT`.
- F7: blocking listener with an empty queue: the call waits for a connection. A signal gives `EINTR`, or a restart after an `SA_RESTART` handler; an expired `SO_RCVTIMEO` gives `EAGAIN`.
- F8: `O_NONBLOCK` listener with an empty queue: `EAGAIN`
- F9: invalid listener: `EBADF` (not open), `ENOTSOCK` (not a socket), `EINVAL` (not listening), `EOPNOTSUPP` (not a stream socket)
- F10: `ECONNABORTED` for an aborted queued connection; resource errors `EMFILE`, `ENFILE`, `ENOBUFS`, `ENOMEM`
- F11: the new socket does not inherit `O_NONBLOCK` (or `O_ASYNC`) from the listener; only `flags` sets them
- F12: shape of the peer address: the IP and port for AF_INET and AF_INET6 (an ephemeral port when the peer did not `bind`); only `sun_family`, or the peer's bound path or abstract name, for AF_UNIX

## Determinism analysis

`linux_determinism: nondeterministic`. There are four sources:

- **Arrival order.** When several clients connect, the order of the queue, and so which client a given `accept4` returns, depends on when each `connect` completed. That depends on host scheduling.
- **Time of the wait.** When a blocking `accept4` returns depends on when a peer connects.
- **Outside peers.** A peer outside the guest, or on the host network, can connect at any time and from any address.
- **The ephemeral port in `addr` (F12).** For a TCP peer that called `connect` without `bind`, the port in `addr` was chosen by the kernel's ephemeral port allocator. On Linux that choice uses a per-boot secret and per-boot random state, and it depends on which ports other host sockets hold. So the port differs between runs even between two processes in one container.

The call repeats only when every peer is inside one deterministic schedule and the peer address is either not requested or has no kernel-chosen part.

## Hermit design

Target policy: `model`. The kernel holds the connection queue. Detcore owns waiting and the guest-visible socket state, and it should own every value in the result that the kernel chose.

**What the handler already does:**
- **Hidden nonblocking.** With thread sequentialization on (`use_nonblocking_sockets`, `detcore-model/src/config.rs:872`), Detcore adds `SOCK_NONBLOCK` to the call (`detcore/src/syscalls/files.rs:4534` to `detcore/src/syscalls/files.rs:4536`). After the call it marks the new socket physically nonblocking (`detcore/src/syscalls/files.rs:4552`, `detcore/src/syscalls/helpers.rs:695`).
- **Guest-visible flags.** The new socket is registered with only the guest's `SOCK_CLOEXEC|SOCK_NONBLOCK` bits, taken from the original call (`detcore/src/syscalls/files.rs:4547`, `detcore/src/syscalls/files.rs:70`). `fcntl(F_GETFL)` answers from that logical state (`detcore/src/syscalls/files.rs:3097`), so the guest sees F1, F2, F3 and F11 as on Linux.
- **Waiting (F7, F8).** The call goes through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/files.rs:4541`). There `ioaction_based_on_fd_status` looks at the listener's logical and physical nonblocking state (`detcore/src/syscalls/helpers.rs:745`):
  - Logically blocking but physically nonblocking: Detcore polls with retries under `InternalIOPolling` (`detcore/src/syscalls/helpers.rs:260`), so the scheduler stays in control.
  - Logically nonblocking: one attempt, and `EAGAIN` is passed through.
  - A run without sequentialization, or record and replay: the call blocks in the kernel through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:250`). Sockets always count as external here, because Detcore has no way yet to tell a connection inside the container from one outside it (`detcore/src/syscalls/helpers.rs:229`, and the doc comment at `detcore/src/syscalls/files.rs:4521`).
- **Invalid listener (F9).** If Detcore cannot classify the listener at all, it lets the kernel decide the errno through the blocking path (`detcore/src/syscalls/helpers.rs:212` to `detcore/src/syscalls/helpers.rs:226`), so F9 errors keep Linux's order of checks.
- **Arrival order between guest threads** is fixed by the sequentialized schedule: each `connect` inside the guest finishes at a point the schedule chooses, before the polling `accept4` sees it.

**What remains before the code fully realizes `model`:**
- **The peer address is not canonicalized (F6, F12).** The handler leaves `addr` exactly as the kernel wrote it. `connect` assigns no port of its own (`detcore/src/syscalls/io.rs:1336`); Detcore assigns ports only on an explicit `bind` to port 0 (`detcore/src/syscalls/files.rs:4158`) and for AF_UNIX autobind (`detcore/src/syscalls/files.rs:4043`). `getpeername` and `getsockname` are plain `record_or_replay` (`detcore/src/syscalls/files.rs:3890`, `detcore/src/syscalls/files.rs:3901`). So a guest TCP server that prints its client's address should see a host-chosen ephemeral port that changes from run to run. This is a hypothesis from the code and from Linux's port allocator, not a measurement; T3 measures it. Owner decision 12 makes that fix the target: `connect` on an unbound socket gets a Detcore-assigned port, as `bind` to port 0 already does (see below).
- **Peers outside the guest** are not controlled. Hermit's stance on external network input is the run's environment policy, not this handler's.
- **Record and replay do not capture `accept`.** The Recorder's subscription list (`hermit-cli/src/recorder.rs:384` to `hermit-cli/src/recorder.rs:474`) has no `accept`, `accept4` or `bind`. The parent-repository report `ai_docs/hermit-record-mode-current-state-20260915.md` (section 4, measured at an earlier hermit SHA, not re-run here) found that every accepting server therefore hangs on replay in a live `accept` that replay never satisfies.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. `accept4` itself needs no change: the peer port written into `addr` (F6, F12) becomes the allocator's once the client's `connect` takes one. Today `handle_connect` (`detcore/src/syscalls/io.rs:1336`) forwards an unbound socket to the kernel, which picks a random port.

Target (owner decision 14, with decision 13 for the timeout): a guest-blocking `accept4` waits in Detcore's scheduler, never in the kernel, through a nonblocking probe plus a scheduler wait. `hermit run` already does this through `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:254`, retry loop `retry_nonblocking_syscall` at `detcore/src/syscalls/helpers.rs:1414`). Two parts are missing. First, the cases that still block in the kernel while holding the turn, through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:225` and `detcore/src/syscalls/helpers.rs:250`): no scheduler, record and replay, and a descriptor Detcore cannot classify. Second, a deadline: with `SO_RCVTIMEO` set, the wait must end with `EAGAIN` once that much virtual time has passed (F7), as Linux's `inet_csk_accept` does when `sock_rcvtimeo` expires. `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) already takes a logical deadline for poll, epoll, futex and signal waits, but Detcore must first learn the timeout: nothing in `detcore/src` reads `SO_RCVTIMEO` or `SO_SNDTIMEO` today (see the `setsockopt` body). `accept4` stays `model`, not `refuse`, because every server calls it.

## Recommended tests

- T1 (covers F1, F3, F5): a server thread calls `accept4(listener, NULL, NULL, SOCK_CLOEXEC)` and plain `accept(listener, NULL, NULL)` against a client inside the guest, under `hermit run --verify` on ptrace. Check `F_GETFD` on both new sockets, and check that data flows.
  Existing candidates:
  - `tests/c/tcp_info_accept4.c` (cell `c-programs/tcp-info-accept4`, `tests/e2e/manifests/c-programs.yaml:9797`, verify ci on ptrace). It calls `accept(listener, NULL, NULL)` (`tests/c/tcp_info_accept4.c:71`), not `accept4`.
  - `tcp_info_hides_host_transport_counters_under_strict_verify` (`hermit-cli/tests/tcp_info_determinism.rs:30`) runs that program on ptrace.
  - `tests/compat/localhost_http_server.c:105` calls `accept4(listener, NULL, NULL, SOCK_CLOEXEC)`. It is a fixture for the real-application compat scripts, and no ptrace CI cell was confirmed for it here.
- T2 (covers F2, F11): accept from a listener the guest made nonblocking, once with `flags = 0` and once with `SOCK_NONBLOCK`. `F_GETFL` on the new socket must show `O_NONBLOCK` only in the second case. With `flags = 0`, a `recv` on the new socket must wait for the client's data rather than fail with `EAGAIN`.
- T3 (covers F6, F12): a TCP loopback client that does not `bind` connects to a guest server. The server prints the port and address from `accept4`'s `addr` and from `getpeername`. The output must be identical on every `--verify` run, and the expectation is that it is not. Repeat with `*addrlen` set to 4 to check truncation and the returned full length, and with an unbound AF_UNIX client, which must give `*addrlen == sizeof(sa_family_t)`.
- T4 (covers F7, F8): a blocking `accept4` in one thread, with a client thread that connects later; the call returns the connection, with the same result on every run. On a nonblocking listener with an empty queue the call returns `EAGAIN`. With `SO_RCVTIMEO` set on a blocking listener, the call returns `EAGAIN` after the timeout.
- T5 (covers F4, F9, F10): with a raw `syscall(SYS_accept4, ...)`, check the errnos:
  - `EINVAL` for `flags = 1`;
  - `EINVAL` on a bound socket that is not listening;
  - `EOPNOTSUPP` on a UDP socket;
  - `ENOTSOCK` on a pipe;
  - `EBADF` on a closed descriptor;
  - `EMFILE` with `RLIMIT_NOFILE` lowered while a connection is pending.
- T6 (covers F1, F7): `hermit record` and then `hermit replay` of a TCP server and client inside the guest. Replay must finish with the recorded output. Expect a hang at this SHA, given the Recorder's subscription list.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (hidden SOCK_NONBLOCK plus scheduler-owned polling; guest flags kept logically); linux_determinism nondeterministic (arrival order, external peers, kernel-chosen peer port); 12 flag behaviours and 6 tests listed; open owner question on canonicalizing the peer port; record mode does not subscribe accept; no coverage token, tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 12, 13 and 14 applied (Target paragraphs: the peer's implicit-bind port comes from Detcore's allocator; a blocking accept4 waits under the scheduler in every mode, with SO_RCVTIMEO on virtual time giving EAGAIN); no new F/T ids; removed the owner-policy question on canonicalizing the peer port

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `accept4`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+accept4&body=Discussion+for+the+%60accept4%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Faccept4.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60accept4%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `accept4`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Record and replay of `accept`/`accept4`: the parent report `ai_docs/hermit-record-mode-current-state-20260915.md` measured replay hanging, at an earlier SHA. The Recorder's subscription list at this SHA (`hermit-cli/src/recorder.rs:384`) still omits both calls. That is a product defect and should have a GitHub issue in rrnewton/hermit if none exists; this read-only task could not search for or file one.
