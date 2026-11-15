---
schema: syscall-pages/v1
syscall: recvmmsg
nr_x86_64: 299
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/397
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+recvmmsg&body=Discussion+for+the+%60recvmmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frecvmmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60recvmmsg%60%2C+or+in+what+the+page+says%3F%0A
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

Receives multiple messages from a socket in one call, with an optional timeout. Hermit runs it through the same nonblocking-socket machinery as `recvmsg`, gives every message in a batch the same logical receive time, rewrites `SCM_TIMESTAMP*` control messages to that time, and sanitizes netlink socket-diagnostic replies per message.

## Linux semantics

`recvmmsg(sockfd, msgvec, vlen, flags, timeout)` receives up to `vlen` datagrams into the `struct mmsghdr` array `msgvec`, filling each `msg_hdr` as `recvmsg` would and writing the byte count of each datagram to its `msg_len`. It returns the number of messages received. Verified: a bad socket fd fails EBADF deterministically.

### Flags and options

- F1: `sockfd`: a socket; `EBADF` for a bad descriptor, `ENOTSOCK` for a non-socket.
- F2: `vlen`: values above `UIO_MAXIOV` (1024) are clamped to 1024, not rejected; `vlen == 0` returns 0.
- F3: per-message `msg_name`/`msg_namelen`: the sender's address, written back with its length.
- F4: per-message `msg_iov` scatter list and `msg_len` output; a datagram longer than the buffers is truncated and `MSG_TRUNC` is set in that message's `msg_flags`.
- F5: per-message control buffer: `SCM_RIGHTS` (installs descriptors), `SCM_CREDENTIALS` (sender pid, uid, gid); `msg_controllen` is written back, with `MSG_CTRUNC` when it was too small.
- F6: receive timestamps: with `SO_TIMESTAMP`, `SO_TIMESTAMPNS` or `SO_TIMESTAMPING` enabled, each message carries the host arrival time as an `SCM_TIMESTAMP*` control message (old and new layouts).
- F7: `MSG_DONTWAIT`: do not block for any datagram.
- F8: `MSG_WAITFORONE`: block for the first datagram only, then behave as `MSG_DONTWAIT`.
- F9: other receive flags, applied to every message: `MSG_PEEK`, `MSG_TRUNC` (report real length), `MSG_ERRQUEUE`, `MSG_CMSG_CLOEXEC`, `MSG_OOB`, `MSG_WAITALL`.
- F10: `timeout`: NULL means no limit. Otherwise it is checked only after each datagram (a call can still block indefinitely for the first one), and on success the kernel writes the remaining time back to the caller's `timespec`. An invalid `timespec` gives `EINVAL`.
- F11: blocking: on a blocking socket without F7, F8 or a timeout, the call waits until `vlen` datagrams have arrived. `SO_RCVTIMEO` bounds each wait.
- F12: partial failure: if an error occurs after one or more datagrams, the call returns the count and the error is reported by the next receive on the socket.
- F13: `msgvec` or `timeout` unreadable, or a message buffer unwritable: `EFAULT`.
- F14: netlink sockets (`NETLINK_SOCK_DIAG`, `NETLINK_ROUTE`): each message is one reply datagram, carrying host socket inode numbers and interface data.

## Determinism analysis

What arrives, and how many messages a call returns, depends on the peer and on the schedule. For a peer inside the same deterministic container, both are fixed once the schedule is fixed; for an external network peer they depend on network arrival timing. On top of the data, the call returns host values even under a fixed schedule: arrival timestamps (F6), the remaining timeout (F10), descriptor numbers and credentials (F5), and host identities in netlink replies (F14). So `conditional`: deterministic for internal peers once those values are virtualized, nondeterministic for external ones.

### Nondeterminism sources

- Message count and contents: depend on network arrival for external peers (F2, F11).
- Timeout against the real clock: whether it cuts a batch short, and the remaining time written back (F10).
- Host arrival timestamps (F6).
- Host socket identities in netlink replies (F14).
- Must be captured for record/replay.

## Hermit design

Target policy: `model`, as for `recvmsg`. Code at hermit f4055eb9cae4:

- Dispatch (`detcore/src/lib.rs:2721`-`2724`): a netlink socket-diagnostic or route descriptor goes to `handle_sock_diag_recvmmsg` (`detcore/src/syscalls/io.rs:1718`); everything else goes to `handle_recvmmsg` (`detcore/src/syscalls/io.rs:1805`). The predicate `sock_diag_reply_fd` requires `virtualize_metadata` (`detcore/src/syscalls/io.rs:1529`). The comments carry the tags `TODO-HUMAN-REVIEW(#788)` and `TODO-HUMAN-REVIEW(PR-901)` (`detcore/src/lib.rs:2714`, `detcore/src/lib.rs:2720`).
- Execution: `handle_recvmmsg` runs the call through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/helpers.rs:202`). `Recvmmsg` uses `network_comm_syscall`, which asserts the socket is physically nonblocking and passes the call unchanged, and counts `EAGAIN`/`EWOULDBLOCK` as "would have blocked" (`detcore/src/syscalls/helpers.rs:1318`-`1330`). A guest-blocking socket therefore takes the deschedule-and-retry path and the Detcore scheduler owns the wait (`detcore/src/syscalls/helpers.rs:254`-`267`); the timeout argument is "ignored" in the sense that Detcore does not interpret it (`detcore/src/syscalls/helpers.rs:1319`-`1320`).
- Timestamps (F6): with `virtualize_time`, `handle_recvmmsg` snapshots each message's control pointer and length before the call, clamped to `MAX_CONTROL_BYTES` (64 KiB, `detcore/src/syscalls/io.rs:221`, `detcore/src/syscalls/io.rs:1817`-`1840`). After a call that delivered messages it takes one logical time for the whole batch through `observe_socket_receive` (`detcore/src/syscalls/io.rs:1847`), which also stores it as the descriptor's receive time for `SIOCGSTAMP` (`detcore/src/syscalls/io.rs:1460`), and rewrites every `SCM_TIMESTAMP*` message in the delivered controls to that time (`detcore/src/syscalls/io.rs:1849`-`1864`, using `socket_timestamp_messages` at `detcore/src/syscalls/io.rs:272` and `canonicalize_socket_timestamps` at `detcore/src/syscalls/io.rs:330`).
- Netlink replies (F14): `handle_sock_diag_recvmmsg` snapshots every message's iovec geometry before the call (`detcore/src/syscalls/io.rs:1738`-`1756`), re-reads each `msg_len` afterwards (`detcore/src/syscalls/io.rs:1767`-`1776`) and sanitizes each delivered message separately (`detcore/src/syscalls/io.rs:1777`-`1779`).
- Verification: `recvmmsg` is in the I/O-buffer hash list (`detcore/src/io_buffers.rs:349`), and the hash covers each completed message's received bytes (`detcore/src/io_buffers.rs:413`-`414`), so `--verify` compares the data.
- Classification: Determinized (`detcore/src/syscall_classification.rs:182`).

Differences from Linux, and gaps, that remain (by the code; none measured here):

- F2: `vlen > UIO_MAXIOV` skips all of the above: `handle_recvmmsg` falls back to a plain call (`detcore/src/syscalls/io.rs:1810`-`1811`) and `handle_sock_diag_recvmmsg` falls back to `handle_recvmmsg` (`detcore/src/syscalls/io.rs:1728`-`1729`). Linux clamps such a `vlen` to 1024 and delivers messages, so a guest that passes 1025 receives host timestamps and unsanitized netlink replies, and no logical receive time is stored.
- F10: the guest's `timeout` reaches the kernel unchanged, so on success the kernel writes back a remaining time computed from the host clock. That value is not in the hashed extents, so `--verify` would not see it.
- F11: because the socket is physically nonblocking, a blocking call returns as soon as at least one datagram has arrived and the queue is empty, as if `MSG_WAITFORONE` (F8) were set; Linux keeps waiting for `vlen` datagrams.
- F6: all messages in a batch get the same logical time, where Linux reports each datagram's own arrival time. The rewrite reads each control buffer to its pre-call length, not to the `msg_controllen` the kernel returned (`detcore/src/syscalls/io.rs:1836`, `detcore/src/syscalls/io.rs:1852`).
- F5: `SCM_CREDENTIALS` values come from the host. The `recvmsg` body records the same question.
- Cost model: `recvmmsg` is charged the default `FAST_NS` (`detcore/src/syscall_time.rs:156`), while `recvmsg` and `sendmmsg` are charged `NETWORK_NS` (`detcore/src/syscall_time.rs:138`-`141`).
- Record mode: sockets are not treated as internal, so record/replay takes the blocking path (`detcore/src/syscalls/helpers.rs:241`-`250`).

Why `model` and not the alternatives: `passthrough` leaks host timestamps and netlink identities; `refuse` would break batching UDP servers and the netlink clients that use `recvmmsg`; `emulate` would need Detcore to own socket buffers, which it does not.

Target (owner decision 13 for `timeout`, decision 14 for the wait): Detcore interprets `timeout` on virtual time. It reads the `timespec` before the call (`EFAULT` if unreadable, `EINVAL` if invalid, as Linux checks), does not pass it to the kernel, checks the virtual-time deadline after each datagram as Linux does, and on success writes back the remaining virtual time (F10). Today the guest's `timeout` reaches the kernel unchanged, and Detcore does not interpret it (`detcore/src/syscalls/helpers.rs:1319`-`1320`). A guest-blocking call keeps probing and waiting under the scheduler until `vlen` datagrams have arrived, the deadline passes, or `SO_RCVTIMEO`, also measured on virtual time, ends a wait (F11); today it returns with whatever the nonblocking kernel call delivered. `MSG_DONTWAIT` and `MSG_WAITFORONE` (F7, F8) still stop early, as on Linux. Record and replay use the same probe and wait instead of the blocking path (`detcore/src/syscalls/helpers.rs:241`-`250`). `recvmmsg` stays `model`, not `refuse`, because batching UDP servers and netlink clients use it.

## Recommended tests

- T1 (covers F1, F2, F4, F7): two datagrams queued on a `SOCK_DGRAM` socketpair, received in one `MSG_DONTWAIT` call, with both bodies and lengths checked, under `hermit run --verify` on ptrace; add `vlen = 0` (returns 0), a truncated datagram (`MSG_TRUNC` in `msg_flags`), and a bad descriptor (`EBADF`).
  Existing candidate: `tests/rust/recvmmsg.rs` (Cargo target `rustbin_recvmmsg`, `tests/Cargo.toml:152`-`153`) covers the two-datagram case and skips record mode (`tests/rust/recvmmsg.rs:27`). No integration test in `hermit-cli/tests` names `rustbin_recvmmsg` at this SHA, so it does not run.
- T2 (covers F6): a batch of two timestamped datagrams on a socket with `SO_TIMESTAMPNS`; both timestamps equal the logical value, identically across runs. Repeat with `vlen = 1025` to show the F2 gap.
  Existing candidate: `tests/c/socket_timestamp_edge_cases.c:117`-`155` (cell `c-programs/socket-timestamp-edge-cases`, `tests/e2e/manifests/c-programs.yaml:8901`), whose verify `ci` has `ptrace: false` (`tests/e2e/manifests/c-programs.yaml:8920`) with a recorded crash-error ("verify exited with status 125", `tests/e2e/manifests/c-programs.yaml:8930`).
- T3 (covers F3, F5): per-message sender addresses on a bound `AF_UNIX` datagram socket, and one message carrying `SCM_RIGHTS` and one carrying `SCM_CREDENTIALS`; compare credentials with the guest's virtual ids.
- T4 (covers F8, F11): a blocking socket with `vlen = 2`, one datagram queued and a second sent later by another guest thread. Record what Hermit returns without flags (1 by the code) and with `MSG_WAITFORONE` (1), against Linux (2 and 1).
- T5 (covers F9): `MSG_PEEK` leaves the queue intact, `MSG_TRUNC` reports the real length, `MSG_CMSG_CLOEXEC` sets close-on-exec on received descriptors, and `MSG_ERRQUEUE` on an empty error queue gives `EAGAIN`.
- T6 (covers F10): a non-NULL timeout; under `hermit run --verify` the written-back `timespec` is the same across runs, and an invalid `timespec` gives `EINVAL`.
- T7 (covers F12, F13): unreadable `msgvec` and `timeout` give `EFAULT`; a batch whose second message has an unwritable buffer returns 1, and the next receive reports the error.
- T8 (covers F14): a socket-diagnostic dump read with `recvmmsg` returns sanitized inode numbers, as with `recvmsg`.
  Existing candidate: `via_recvmmsg` in `tests/compat/real_compat_workload.sh:1058`, listed among the receive paths at `tests/compat/real_compat_workload.sh:1069`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/397: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (io.rs:1805 handle_recvmmsg rewrites batch timestamps to one logical time, io.rs:1718 sanitizes netlink replies, scheduler owns blocking), matching recvmsg; linux_determinism nondeterministic -> conditional (deterministic for internal peers, nondeterministic for external ones), matching recvmsg; 14 flag behaviours and 8 tests listed; gaps found by reading: vlen above 1024 bypasses timestamp and netlink virtualization, the timeout write-back carries host time, blocking calls return after one datagram; open questions recorded; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 13 and 14 applied (Target paragraph: timeout interpreted on virtual time with a virtual remaining time written back; a blocking call waits under the scheduler until vlen datagrams or a deadline, in every mode); no new F/T ids; removed the open questions on interpreting timeout and on the early return after one datagram

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `recvmmsg`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+recvmmsg&body=Discussion+for+the+%60recvmmsg%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frecvmmsg.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60recvmmsg%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: recvmmsg" (https://github.com/rrnewton/hermit/issues/397), legacy variant A1.

### Open questions

- Should `vlen > UIO_MAXIOV` be clamped to 1024 before the timestamp and netlink handling, as Linux clamps it, instead of falling back to an unvirtualized call (`detcore/src/syscalls/io.rs:1810`, `detcore/src/syscalls/io.rs:1728`)? This looks like a determinism gap and may deserve its own issue.
- Why does `c-programs/socket-timestamp-edge-cases` crash on ptrace (`tests/e2e/manifests/c-programs.yaml:8930`)? It holds the only batch-timestamp check.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=recvmmsg` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `recvmmsg` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
recvmmsg(-1, NULL, 0, 0, NULL)  = -1 EBADF (Bad file descriptor)
```

That routing claim is history: at hermit f4055eb9cae4 `recvmmsg` is Determinized (`detcore/src/syscall_classification.rs:182`) and handled as described under Hermit design.

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
