---
schema: syscall-pages/v1
syscall: sendmsg
nr_x86_64: 46
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sendmsg&body=Discussion+for+the+%60sendmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsendmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sendmsg%60%2C+or+in+what+the+page+says%3F%0A
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

`sendmsg` sends one message from a gather list of buffers, with an optional destination address and ancillary data (file descriptors, credentials, per-packet options). Hermit models it:
- the kernel moves the data and installs passed descriptors;
- Detcore orders when a call that would block is retried;
- after any successful send, Detcore forgets its cached `flock` modes, because descriptors may have left the process.

## Linux semantics

`sendmsg(sockfd, msg, flags)` reads a `struct msghdr` from guest memory:
- `msg_name`/`msg_namelen` give the destination;
- `msg_iov`/`msg_iovlen` give the data to gather;
- `msg_control`/`msg_controllen` give the ancillary messages.

It queues the data and returns the number of bytes accepted. The data path follows `sendto`:
- a datagram goes whole or not at all;
- a stream send on a blocking socket waits until every byte is queued;
- a broken stream raises `SIGPIPE` and `EPIPE`, unless `MSG_NOSIGNAL` is passed;
- an unbound `AF_INET`/`AF_INET6` datagram socket binds to an ephemeral port on its first send.

`SCM_RIGHTS` puts references to the sender's open file descriptions in flight; the receiver gets new descriptors for the same descriptions. The message buffers stay in guest memory, which other threads can change while the call is blocked.

### Flags and options

- F1: `msg_name`/`msg_namelen`, used like `sendto`'s `dest_addr`:
  - `EISCONN` for an explicit destination on a connected `AF_UNIX` stream;
  - `EDESTADDRREQ` or `ENOTCONN` when no destination is known.
- F2: `msg_iov`/`msg_iovlen`: gather from several buffers. An empty list sends 0 bytes; more than `IOV_MAX` entries gives `EMSGSIZE`. `msg_flags` is ignored on input.
- F3: `SCM_RIGHTS`: pass up to `SCM_MAX_FD` (253) descriptors. A bad descriptor gives `EBADF`; exceeding the in-flight limit gives `ETOOMANYREFS`.
- F4: `SCM_CREDENTIALS`: the sender states its pid, uid and gid. Claiming ids the caller is not allowed to use gives `EPERM`.
- F5: Protocol ancillary messages:
  - `IP_PKTINFO`, `IP_TTL`, `IP_TOS` and `IPV6_PKTINFO`;
  - `UDP_SEGMENT`, `SO_MARK` and `SCM_TXTIME`;
  - `SO_TIMESTAMPING` requests.
- F6: `MSG_DONTWAIT`: nonblocking for this call only.
- F7: `MSG_NOSIGNAL`: suppresses `SIGPIPE`.
- F8: `MSG_MORE`: more data follows.
- F9: `MSG_OOB`: urgent data.
- F10: `MSG_CONFIRM` and `MSG_DONTROUTE`: hints to the IP layer.
- F11: `MSG_EOR`: ends a record on `SOCK_SEQPACKET`.
- F12: `MSG_FASTOPEN`: TCP Fast Open with data in the SYN.
- F13: `MSG_ZEROCOPY`: completion notices arrive on the error queue.
- F14: Blocking versus nonblocking descriptor. A blocking stream send waits until all bytes are queued; a nonblocking one returns a short count or `EAGAIN`.
- F15: `SO_SNDTIMEO`: a blocking send gives up after the timeout.
- F16: Signal interruption: `EINTR` or a partial count; a restart under `SA_RESTART` when no send timeout is set.
- F17: Datagram size: `EMSGSIZE`; a zero-length datagram is legal.
- F18: Implicit autobind: a kernel-chosen ephemeral port on the first send.
- F19: Errors:
  - `EBADF` and `ENOTSOCK`;
  - `EFAULT`, for the header, the iovecs or the control buffer;
  - `EINVAL`, for malformed ancillary messages;
  - `EACCES`, `ECONNREFUSED`, `ECONNRESET`, `ENOBUFS` and `EPIPE`.

## Determinism analysis

As with `sendto`, the result depends on the peer's buffer occupancy, and so on the schedule. `SCM_RIGHTS` adds state shared between processes: after a descriptor is passed, another process can change that open file description (its offset, its locks) without the sender seeing it. Under one fixed schedule with every peer inside the guest, the call repeats.

### Nondeterminism sources

- Peer progress, which decides short counts and blocking (F14, F6).
- External peers, ICMP errors (F19), and kernel-chosen ephemeral ports (F18).
- Host buffer sizes and configuration (F12).
- Real-time timeouts and the arrival time of signals (F15, F16).
- Shared kernel state for passed descriptors (F3). Credentials carry host ids unless the container maps them (F4).
- Guest memory that another thread rewrites while the call is blocked, which changes what is actually sent.

`linux_determinism: conditional`.

## Hermit design

`target_policy: model`. Dispatch: `Syscall::Sendmsg(s) => self.handle_sendmsg(guest, s)` at detcore/src/lib.rs:2710. `handle_sendmsg` (detcore/src/syscalls/io.rs:1420-1434) runs `execute_nonblockable_fd_syscall`, then on success calls `forget_flock_modes()` (io.rs:1431-1432). Its doc comment (io.rs:1422-1425) gives the reason: the guest can change message and control memory while the call is descheduled, so parsing the control buffer beforehand would not prove which descriptors were sent, and every cached lock mode is made unknown. `handle_flock` refuses a blocking conversion while the lock state is unknown, because the state may have changed through `SCM_RIGHTS` (detcore/src/syscalls/files.rs:1263-1269). `Sendmsg` counts `EAGAIN`/`EWOULDBLOCK` as "would have blocked" (helpers.rs:1362-1373).

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

Why `model`. Detcore owns when a blocked send resumes, and keeps its own descriptor-related state (the `flock` cache) consistent with what `SCM_RIGHTS` may have moved. The kernel keeps the data and the descriptor transfer. `passthrough` would let a blocking send stall the sequentialized scheduler.

Existing ptrace coverage at f4055eb9: `c-programs/recvmsg-scm-rights-mmap` (tests/e2e/manifests/c-programs.yaml:7225, program at :7235, `verify` `ci: true` on ptrace) passes one descriptor with `SCM_RIGHTS` over an `AF_UNIX` datagram socketpair (tests/c/recvmsg_scm_rights_mmap.c:46-52). The same guest is in `BASELINE_RECORD_WORKLOADS` (hermit-cli/tests/record_replay.rs:310-321).

What remains: hypotheses from reading the code, not yet measured.
- H1 (F6). `MSG_DONTWAIT` on a logically blocking socket is retried rather than returned as `EAGAIN`, as for `sendto`.
- H2 (F14). A blocking stream `sendmsg` larger than the free buffer space returns the kernel's short count.
- H3 (F15). `SO_SNDTIMEO` never fires in run mode.
- H4 (F18). The implicit autobind gives a host-chosen port.
- H5 (F4). The pid, uid and gid in `SCM_CREDENTIALS` reach the peer unchanged. Whether they match the guest's virtual ids has not been checked.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. Today `handle_sendmsg` (`detcore/src/syscalls/io.rs:1426`) forwards an unbound socket to the kernel, which autobinds it at a random port in `inet_autobind`, called from `inet_send_prepare` (F18, H4); a `MSG_FASTOPEN` send on an unbound TCP socket gets its port through the connect path, `inet_hash_connect` (F12).

Target (owner decision 14, with decision 13 for the timeout): a guest-blocking `sendmsg` waits in Detcore's scheduler, never in the kernel, through a nonblocking probe plus a scheduler wait. Run mode already does this through `NonblockizeRetry` (`detcore/src/syscalls/helpers.rs:254`, retry loop `retry_nonblocking_syscall` at `detcore/src/syscalls/helpers.rs:1414`). Four parts are missing. First, the cases that still block in the kernel while holding the turn, through `record_or_replay_blocking` (`detcore/src/syscalls/helpers.rs:225` and `detcore/src/syscalls/helpers.rs:250`): no scheduler, record and replay, and a descriptor Detcore cannot classify. Second, `MSG_DONTWAIT` must return the kernel's `EAGAIN` at once (F6, H1); no code in `detcore/src` reads it today. Third, a blocking stream send keeps probing and waiting until every byte is queued, as Linux does, and returns a short count only for a signal, an error or a timeout (F14, F16, H2). Fourth, a deadline: with `SO_SNDTIMEO` set, the wait ends with the partial count or `EAGAIN` once that much virtual time has passed (F15, H3). `retry_nonblocking_syscall_with_timeout` (`detcore/src/syscalls/helpers.rs:1432`) already takes a logical deadline for poll, epoll, futex and signal waits, but nothing in `detcore/src` reads `SO_RCVTIMEO` or `SO_SNDTIMEO` today (see the `setsockopt` body). `sendmsg` stays `model`, not `refuse`, because nearly every networked program sends on sockets.

## Recommended tests

- T1 (covers F1, F2, F17): on `AF_UNIX` datagram and UDP loopback pairs:
  - `sendmsg` with `msg_name` set, and null after `connect`;
  - `EISCONN` on a connected `AF_UNIX` stream;
  - a three-element iovec gathered in order;
  - an empty iovec;
  - more than `IOV_MAX` entries, expecting `EMSGSIZE`;
  - a zero-length datagram;
  - an oversized UDP payload, expecting `EMSGSIZE`.

  Run under `--verify`.
- T2 (covers F3): pass 1, 2 and 253 descriptors with `SCM_RIGHTS`; a bad descriptor fails with `EBADF`. Then, in the sender, attempt a blocking `flock` conversion on a descriptor it locked earlier. Record whether Hermit refuses it, as files.rs:1263-1269 documents. Extends `tests/c/recvmsg_scm_rights_mmap.c`.
- T3 (covers F4, F5): send `SCM_CREDENTIALS` with the caller's own ids, and with ids it may not claim (`EPERM`); compare what the receiver sees with the guest's `getpid`/`getuid` (H5). Send `IP_PKTINFO` and `IP_TTL` on UDP loopback, and `SO_MARK` where permitted. Two runs must match.
- T4 (covers F6, F14): on a logically blocking socketpair with a full buffer, `MSG_DONTWAIT` must return `EAGAIN` (H1). A blocking send larger than the free space, with a draining reader, must return the full length (H2).
- T5 (covers F7, F19): after the peer closes, `sendmsg` without `MSG_NOSIGNAL` raises `SIGPIPE` and returns `EPIPE`; with `MSG_NOSIGNAL` it returns only `EPIPE`. Also:
  - `EBADF` and `ENOTSOCK`;
  - `EFAULT` for bad `msg_iov` and bad `msg_control`;
  - `EINVAL` for a `cmsg_len` that is too small.
- T6 (covers F8, F9, F10, F11): per-call send hints, each on the socket type that supports it:
  - `MSG_MORE` then a plain send, on TCP;
  - `MSG_OOB` on TCP;
  - `MSG_CONFIRM` and `MSG_DONTROUTE` on UDP;
  - `MSG_EOR` on `SOCK_SEQPACKET`, keeping the record boundary.

  Two runs must match.
- T7 (covers F12, F13): `MSG_FASTOPEN` on loopback TCP behaves the same in every run. `MSG_ZEROCOPY` with `SO_ZEROCOPY` produces an error-queue completion.
- T8 (covers F15, F16): send timeouts and signals:
  - With `SO_SNDTIMEO` set and a full buffer, the send times out in logical time (H3).
  - A signal during a blocked `sendmsg` gives `EINTR` without `SA_RESTART`, or restarts with it.
- T9 (covers F18): an unbound UDP sender uses `sendmsg`, and the receiver prints the source port. It must be identical across two runs (H4).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_sendmsg: nonblockable retry, then forget_flock_modes after a successful send because SCM_RIGHTS may move descriptors). linux_determinism stays conditional. 19 flags and 9 tests. The MSG_DONTWAIT, short-send, SO_SNDTIMEO, autobind and SCM_CREDENTIALS hypotheses are under Open questions for measurement. Stays todo; no coverage recorded.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 12, 13 and 14 applied (Target paragraphs: the implicit bind on first send takes an allocator port; blocking sends wait under the scheduler in every mode, honour MSG_DONTWAIT, queue every byte, and end at a virtual-time SO_SNDTIMEO deadline); no new F/T ids; no open questions removed (H1-H5 are measurements)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sendmsg`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sendmsg&body=Discussion+for+the+%60sendmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsendmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sendmsg%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `sendmsg`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

These questions need a measurement, not an owner decision, and none of them changes the `target_policy` choice. Each is a hypothesis under Hermit design and names the test that answers it.

- H1: on a logically blocking socket, does `MSG_DONTWAIT` wait instead of returning `EAGAIN`? (T4)
- H2: does a blocking stream `sendmsg` larger than the free buffer space return a short count? (T4)
- H3: does `SO_SNDTIMEO` never fire in run mode? (T8)
- H4: is the port chosen by the implicit autobind host-chosen? (T9)
- H5: do the `SCM_CREDENTIALS` values match the guest's virtual pid, uid and gid? (T3)
