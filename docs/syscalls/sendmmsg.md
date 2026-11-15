---
schema: syscall-pages/v1
syscall: sendmmsg
nr_x86_64: 307
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sendmmsg&body=Discussion+for+the+%60sendmmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsendmmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sendmmsg%60%2C+or+in+what+the+page+says%3F%0A
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

Sends several messages on a socket in one call: the batched form of `sendmsg`. Hermit runs it through the same nonblocking-socket machinery as `sendmsg`, so the Detcore scheduler owns any wait, and after a call that sent at least one message it forgets every cached `flock` mode, because the batch may have passed descriptors with `SCM_RIGHTS`.

## Linux semantics

`sendmmsg(sockfd, msgvec, vlen, flags)` sends each `struct mmsghdr` in `msgvec` as `sendmsg` would, writes the number of bytes sent for each message to its `msg_len`, and returns the number of messages sent. The kernel itself sets `MSG_BATCH` on every message but the last, so the protocol can defer work until the end of the batch. Data, addresses and ancillary data go to the peer; the only outputs to the caller are the return value and the `msg_len` fields.

### Flags and options

- F1: `sockfd`: a socket; `EBADF` for a bad descriptor, `ENOTSOCK` for a non-socket.
- F2: `vlen`: values above `UIO_MAXIOV` (1024) are clamped to 1024, not rejected; `vlen == 0` returns 0.
- F3: per-message `msg_name`/`msg_namelen`: the destination of each message on an unconnected datagram socket (`EISCONN` or `EDESTADDRREQ` for a mismatch with the connection state). An unbound socket is autobound to a kernel-chosen address on the first send.
- F4: per-message `msg_iov` gather list, and the `msg_len` output with the bytes sent for that message.
- F5: `SCM_RIGHTS` control messages: descriptors passed to the peer.
- F6: `SCM_CREDENTIALS` control messages: the sender's pid, uid and gid, checked by the kernel against the caller's real ids.
- F7: `MSG_DONTWAIT`: do not block when the socket's send buffer or the peer's queue is full; return `EAGAIN` if no message was sent.
- F8: `MSG_NOSIGNAL`: on a broken connection return `EPIPE` without raising `SIGPIPE`; without it the caller also gets `SIGPIPE`.
- F9: other send flags, applied to every message: `MSG_MORE`, `MSG_EOR`, `MSG_OOB`, `MSG_CONFIRM`, `MSG_DONTROUTE`, `MSG_ZEROCOPY`, `MSG_FASTOPEN`.
- F10: blocking: on a blocking socket the call waits for buffer space for each message in turn; `SO_SNDTIMEO` bounds each wait. On a stream socket a message can be sent partly, with `msg_len` short.
- F11: partial failure: if an error occurs after one or more messages were sent, the call returns the count and the error is lost.
- F12: `msgvec` unreadable or a message's buffers or control data unreadable: `EFAULT`; a signal before any message was sent: `EINTR`.
- F13: per-protocol errors from the first message: `EAGAIN`, `ECONNREFUSED`, `ECONNRESET`, `EMSGSIZE`, `ENOBUFS` and the like.

## Determinism analysis

As with `sendmsg`, how many messages are sent, and whether the call blocks, depends on how full the peer's queue is, and so on the schedule. For peers inside the same deterministic container the result repeats under a fixed schedule. For external peers it depends on the network: buffer drain, ICMP errors (F13), and the kernel-chosen autobind address (F3). `SCM_RIGHTS` (F5) shares open file descriptions with another process, whose later changes the sender cannot see; `SCM_CREDENTIALS` (F6) carries host ids unless they are mapped. So `conditional`.

### Nondeterminism sources

- Peer progress, which decides partial batches, short `msg_len` values and blocking (F10, F11).
- External peers and network errors (F13), and kernel-chosen autobind addresses (F3).
- Real-time `SO_SNDTIMEO` timeouts and signal arrival (F10, F12).
- Shared state of passed descriptors (F5) and host credentials (F6).

## Hermit design

Target policy: `model`, as for `sendmsg`. Code at hermit f4055eb9cae4:

- Dispatch (`detcore/src/lib.rs:2711`): `handle_sendmmsg` (`detcore/src/syscalls/io.rs:1440`) runs the call through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/helpers.rs:202`). `Sendmmsg` uses `network_comm_syscall`, which asserts that the socket is physically nonblocking and passes the call unchanged, and counts `EAGAIN`/`EWOULDBLOCK` as "would have blocked" (`detcore/src/syscalls/helpers.rs:1348`-`1358`); its descriptor is `sockfd` (`detcore/src/syscalls/helpers.rs:822`). A logically blocking socket therefore takes the deschedule-and-retry path, so the scheduler, not the kernel, decides when a blocked send resumes (`detcore/src/syscalls/helpers.rs:254`-`267`).
- Descriptor passing (F5): when the result is positive, `handle_sendmmsg` calls `forget_flock_modes` (`detcore/src/syscalls/io.rs:1446`-`1447`, `detcore/src/tool_local.rs:2612`), so every cached open-file-description lock mode becomes unknown. The doc comment (`detcore/src/syscalls/io.rs:1436`-`1439`) says this deliberately includes descriptors named only by unsent tail messages, because the guest can change the array while the call is descheduled. `handle_sendmsg` does the same after any success (`detcore/src/syscalls/io.rs:1431`-`1432`); a `sendmmsg` that returns 0 (F2) forgets nothing, which is right because nothing was sent.
- Verification: `sendmmsg` is in the I/O-buffer hash list (`detcore/src/io_buffers.rs:358`); the hash covers each completed message's sent bytes, bounded by its `msg_len` (`detcore/src/io_buffers.rs:424`-`427`), labelled as outgoing (`detcore/src/io_buffers.rs:453`). `--verify` therefore compares what the guest sent.
- Classification: Determinized (`detcore/src/syscall_classification.rs:191`); charged `NETWORK_NS` (`detcore/src/syscall_time.rs:141`).

Differences from Linux that remain (hypotheses from reading the code, not measured):

- F10: because the socket is physically nonblocking, the kernel stops at the first message that would block and returns the count so far; only a call that sent nothing is retried. Linux on a blocking socket keeps waiting until every message is sent, so a batch can return short under Hermit where it would not natively.
- F7: on a logically blocking socket, `MSG_DONTWAIT` is not inspected, so an `EAGAIN` from a call that sent nothing is retried instead of returned. The `sendmsg` body records the same hypothesis.
- F6: the pid, uid and gid in `SCM_CREDENTIALS` reach the peer unchanged; whether they match the guest's virtual ids is not checked.
- F10: `SO_SNDTIMEO` is not modelled.
- Record mode: sockets are not treated as internal, so record/replay takes the blocking path (`detcore/src/syscalls/helpers.rs:241`-`250`).
- Backends: a comment at `hermit-cli/tests/kvm_info_log_determinism.rs:469`-`470` says the KVM backend returns `ENOSYS` for `sendmmsg`.

Why `model` and not the alternatives: `passthrough` would let a blocking send stall the sequentialized scheduler and would leave the `flock` cache stale after a descriptor transfer; `refuse` would break batching UDP clients and anything built on glibc's `sendmmsg`; `emulate` would need Detcore to own socket buffers, which it does not.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. Today `handle_sendmmsg` (`detcore/src/syscalls/io.rs:1440`) forwards an unbound socket to the kernel, which autobinds it on the first message at a random port in `inet_autobind`, called from `inet_send_prepare` (F3).

Target (owner decision 14, with decision 13 for the timeout): a guest-blocking `sendmmsg` keeps probing and waiting under the scheduler until every message is sent, as Linux keeps waiting (F10). It returns a short count only when a later message fails (F11), a signal arrives, or `SO_SNDTIMEO`, measured on virtual time, ends a wait; today only a call that sent nothing is retried. `MSG_DONTWAIT` (F7) returns the kernel's `EAGAIN` at once. Record and replay use the same probe and wait instead of the blocking path (`detcore/src/syscalls/helpers.rs:241`-`250`). `sendmmsg` stays `model`, not `refuse`, because batching UDP clients use it.

## Recommended tests

- T1 (covers F1, F2, F4): two datagrams on a `SOCK_DGRAM` socketpair in one call, with the peer checking both bodies and each `msg_len`; `vlen = 0` returns 0; a bad descriptor gives `EBADF`; run under `hermit run --verify` on ptrace.
  Existing candidate: `ptrace_verify_compares_sendmmsg_iovec_prefixes` (`hermit-cli/tests/kvm_info_log_determinism.rs:475`) runs `--verify` on ptrace and expects a mutated `sendmmsg` payload to be reported (markers `[iobuf]` and `sendmmsg out`, `hermit-cli/tests/kvm_info_log_determinism.rs:519`) while identical runs pass; the unit test `sendmmsg_hashes_only_completed_messages_and_each_reported_byte_prefix` (`detcore/src/io_buffers.rs:2095`) covers the extent calculation. Neither covers `vlen = 0` or `EBADF`.
- T2 (covers F5, F11): a batch that passes a descriptor with `SCM_RIGHTS`, and a two-message batch whose second message has an unreadable control pointer, which returns 1.
  Existing candidates: `send_fd_mmsg` (`tests/c/flock_exclusion.c:129`-`144`), run as scenario `sent-after-fork-mmsg` by `transferred_lock_state_is_unknown_to_the_sender` (`hermit-cli/tests/flock_exclusion.rs:555`, `hermit-cli/tests/flock_exclusion.rs:587`); and `scenario_partial_sendmmsg` (`tests/c/flock_exclusion.c:1069`-`1155`), which expects `sent == 1` (`tests/c/flock_exclusion.c:1125`-`1126`) and both locks invalidated, run by `partial_sendmmsg_invalidates_all_flock_state` (`hermit-cli/tests/flock_exclusion.rs:617`). Both run on ptrace (`hermit-cli/tests/flock_exclusion.rs:118`) with `--allow-unsupported-syscalls` and without `--verify`.
- T3 (covers F6): send `SCM_CREDENTIALS` and compare what the peer receives with the guest's virtual pid, uid and gid.
- T4 (covers F3): an unbound, unconnected datagram socket sends to two bound peers with per-message `msg_name`; check delivery and the autobind address across runs; a connected socket with `msg_name` set gives `EISCONN`.
- T5 (covers F8): send on a socket whose peer has closed: with `MSG_NOSIGNAL` the result is `EPIPE` and no signal; without it the guest also gets `SIGPIPE`.
- T6 (covers F9): `MSG_MORE` on UDP loopback coalesces messages, `MSG_EOR` on `SOCK_SEQPACKET` keeps record boundaries, `MSG_OOB` on TCP loopback, and `MSG_ZEROCOPY` with `SO_ZEROCOPY` posts a completion on the error queue.
- T7 (covers F7, F10): a blocking `AF_UNIX` datagram socketpair whose receive queue is full, with a peer thread draining it later. Record the count Hermit returns for `vlen = 4` against Linux (4), and check that `MSG_DONTWAIT` on the full queue returns `EAGAIN`; add a stream socket with a message larger than the free space to see a short `msg_len`.
- T8 (covers F12, F13): unreadable `msgvec` gives `EFAULT`; an oversized first datagram gives `EMSGSIZE`; a signal while blocked before the first message gives `EINTR`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (io.rs:1440 runs the batch through the scheduler-owned nonblocking path and forgets flock modes after a positive result), matching sendmsg; linux_determinism stays conditional (confirmed, TODO removed); 13 flag behaviours and 8 tests listed; hypotheses recorded: a blocking batch can return short, MSG_DONTWAIT may be retried, SCM_CREDENTIALS ids unchecked; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 12, 13 and 14 applied (Target paragraphs: the implicit bind on the first message takes an allocator port; a blocking batch waits under the scheduler until every message is sent or a virtual-time SO_SNDTIMEO deadline, in every mode); no new F/T ids; trimmed the retry-policy tail from the first open question, kept the measurement

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sendmmsg`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sendmmsg&body=Discussion+for+the+%60sendmmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsendmmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sendmmsg%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `sendmmsg`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

These need a measurement, not an owner decision, and none changes the `target_policy` choice.

- Does a blocking `sendmmsg` return fewer than `vlen` messages under Hermit when the peer's queue fills mid-batch (F10)? T7 measures it.
- Does `MSG_DONTWAIT` on a logically blocking socket wait instead of returning `EAGAIN` (F7)? T7.
- Do the `SCM_CREDENTIALS` ids match the guest's virtual ids (F6)? T3.
