---
schema: syscall-pages/v1
syscall: shutdown
nr_x86_64: 48
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/474
  - https://github.com/rrnewton/hermit/issues/480
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shutdown&body=Discussion+for+the+%60shutdown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshutdown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shutdown%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`shutdown` disables receives, sends, or both directions of a socket connection (full-duplex for stream sockets) without closing the descriptor. Hermit models it with a typed handler. The handler forwards the single call, which never blocks, through `record_or_replay`. The effects on the peer (EOF, `EPIPE`, wakeups) then reach the guest through Detcore's scheduler-ordered send and receive paths.

## Linux semantics

`shutdown(sockfd, how)` ends one or both directions of the connection on the socket behind `sockfd`; see [shutdown(2)](https://man7.org/linux/man-pages/man2/shutdown.2.html). `how` selects `SHUT_RD`, `SHUT_WR` or `SHUT_RDWR`. The call returns 0 or an error, and it never blocks.

- The effect applies to the socket, not to the descriptor, so every duplicate sees it.
- After `SHUT_WR`, the peer reads the data already queued and then gets EOF (0). Later writes on the shut-down side fail with `EPIPE` and raise `SIGPIPE`.
- After `SHUT_RD`, receives return EOF. On `AF_UNIX` stream sockets, the peer's later writes fail with `EPIPE`.
- Threads blocked in a receive, a send or an `accept` on either end are woken, and `poll` readiness changes (`POLLRDHUP`, `POLLHUP`).
- Host evidence (2026-07-24, quoted under Additional information):
  - On a socket that is not connected, the call failed with `ENOTCONN`, and did so repeatably.
  - Shutting down writes on one end of a local socket pair succeeded.

### Flags and options

- F1: `SHUT_RD`: no further receives. Reads return 0. Data still queued on TCP is discarded, and on `AF_UNIX` stream sockets the peer's writes then fail with `EPIPE`.
- F2: `SHUT_WR`: no further sends. The peer sees EOF after the queued data (TCP sends FIN). Later sends on this side fail with `EPIPE` and raise `SIGPIPE`.
- F3: `SHUT_RDWR`: both directions.
- F4: A `how` value outside the three: `EINVAL`.
- F5: Descriptor errors: `EBADF` for a bad descriptor, and `ENOTSOCK` for a descriptor that is not a socket.
- F6: `ENOTCONN`: a connection-mode socket that is not connected. This is the 2026-07-24 strace evidence.
- F7: What the peer sees: EOF on reads after the queued data, `EPIPE`/`SIGPIPE` on writes, and changed `poll`/`epoll` readiness (`POLLRDHUP`, `POLLHUP`, `POLLIN` for EOF).
- F8: Wakeups: threads blocked in `recv`, `send` or `accept` on either end, in this process or another, return with EOF or an error.
- F9: Socket type:
  - stream and `SOCK_SEQPACKET` sockets;
  - connected datagram sockets, such as an `AF_UNIX` `SOCK_DGRAM` socketpair, where `SHUT_RDWR` succeeds;
  - listening sockets, where `SHUT_RD` stops new connections from being accepted.
- F10: Scope: the change applies to the socket, so duplicates made by `dup` or inherited through `fork` see it. `close` affects only one descriptor.

## Determinism analysis

The operation is deterministic for a fixed socket state. Its result, though, depends on the connection state, which is part of the network I/O Hermit must determinize. On a connected socket it also affects a peer. Both migrated texts classified the call CONDITIONAL; they agree, and that is kept. The duplicate added the point that the state change affects EOF and wakeups seen by the peer, while errors and readiness depend on live socket and network state, so the call must take part in Hermit's deterministic socket event model.

### Nondeterminism sources

- Connection state at the moment of the call (F6), which depends on when `connect`/`accept` completed relative to this thread.
- Peer actions and queued data: what the peer has read or written decides what it observes after the shutdown (F7).
- Protocol state and network errors: for TCP to an external peer, FIN/RST timing and errors come from outside the container.
- Socket type (F9).
- Concurrent threads blocked on either end: which wakeup happens, and what each woken call returns, depends on the interleaving (F8).

`linux_determinism: conditional`.

## Hermit design

`target_policy: model`.
- Dispatch: `Syscall::Shutdown(s) => self.handle_shutdown(guest, s)` at detcore/src/lib.rs:2573. The comment at lib.rs:2569-2572 is tagged `TODO-HUMAN-REVIEW(#818)` and calls shutdown "the lone remaining socket-family syscall".
- Handler: `handle_shutdown` (detcore/src/syscalls/files.rs:3999-4011) is `Ok(self.record_or_replay(guest, call).await?)`. Its doc comment says shutdown "never blocks and returns no data; its effect is deterministic given the container's socket state".

Why `model`. `shutdown` itself does not block, so one direct call, recorded in record mode and replayed in replay mode, is the right execution strategy. It needs no nonblockable path. The nondeterminism in the table above comes from when the call happens relative to the peer, and Detcore owns that through its schedule.

The effect on the peer is realized by Detcore's other socket paths. In run mode, a peer thread blocked in a receive on a logically blocking socket waits inside `retry_nonblocking_syscall` under `InternalIOPolling` (detcore/src/syscalls/helpers.rs:254-267). Only `EAGAIN`/`EWOULDBLOCK` count as "would block" (for example, `Recvfrom` at helpers.rs:1290-1301). The EOF (0) or `EPIPE` that the shutdown causes is therefore returned on the peer's next retry, at a point the scheduler chooses. In record/replay mode, sockets go through `record_or_replay_blocking` (helpers.rs:241-250).

Reconciling the two migrated recommendations:
- https://github.com/rrnewton/hermit/issues/474 recommended NEEDS-INVESTIGATION.
- https://github.com/rrnewton/hermit/issues/480 recommended NEEDS-HANDLER.

Both were written when shutdown was handled by the generic fallback. The typed handler above supersedes both. The investigation that remains is the peer-effect coverage in Recommended tests, not the choice of strategy.

Existing ptrace coverage at f4055eb9:
- `c-programs/shutdown-socketpair` (tests/e2e/manifests/c-programs.yaml:14410, program at :14420, `verify` `ci: true` on ptrace). It runs `SHUT_RD`, `SHUT_WR` and `SHUT_RDWR` on an `AF_UNIX` stream pair, `SHUT_RDWR` on a datagram pair, and `shutdown(-1, SHUT_RDWR)` expecting `EBADF` (tests/c/shutdown_socketpair.c:41-57).
- `c-programs/syscall-quick-wins` (c-programs.yaml:9345, program at :9355, `verify` `ci: true` on ptrace): `SHUT_WR` on an `AF_UNIX` stream socketpair (tests/c/syscall_quick_wins.c:129-143).
- The cargo test `deterministic_passthrough_syscalls_verify` (hermit-cli/tests/syscall_quick_wins.rs:29-30) builds that guest and runs it with `--backend=ptrace`. It asserts that the trace contains `inbound syscall: shutdown(` (:67-76) and that the output says `Determinism verified` (:103-104).
- `tests/rust/shutdown.rs` checks EOF at the peer after `SHUT_WR` (:50-56). It skips record mode because socketpair guests hang there (:26-31). It is wired only through the Buck `rust/*.rs` glob, not through `hermit-cli/tests/hermit_modes.rs`.

## Recommended tests

- T1 (covers F1, F2, F3, F9): `SHUT_RD`, `SHUT_WR` and `SHUT_RDWR` on an `AF_UNIX` stream pair, an `AF_UNIX` `SOCK_SEQPACKET` pair, an `AF_UNIX` datagram pair, and a connected loopback TCP pair. Also `SHUT_RD` on a listening TCP socket. Run under `--verify`. Extends `tests/c/shutdown_socketpair.c`.
- T2 (covers F4, F5, F6): error returns, compared across two runs:
  - `how` = 3 gives `EINVAL`;
  - fd -1 gives `EBADF`;
  - a pipe descriptor gives `ENOTSOCK`;
  - an unconnected TCP socket gives `ENOTCONN`, matching the 2026-07-24 strace evidence.
- T3 (covers F7): what the peer observes after each direction is shut down:
  - after `SHUT_WR`, the peer reads the queued bytes and then 0;
  - a write after its own `SHUT_WR` gives `EPIPE` and a `SIGPIPE` that a handler observes;
  - on `AF_UNIX`, a write from the peer after `SHUT_RD` gives `EPIPE`;
  - `poll` on the peer reports `POLLRDHUP`.

  This is the scenario in `tests/rust/shutdown.rs:50-56`, needed in a manifest cell or a ptrace-only cargo test.
- T4 (covers F8): thread A blocks in `recv` on one end of a logically blocking socketpair, and thread B calls `shutdown(SHUT_WR)` on the other end. A must return 0 at the same logical point in every run. Repeat with a thread blocked in `accept` on a listening loopback socket that another thread shuts down. Run under `--verify`.
- T5 (covers F10): `dup` the descriptor, then fork; shut down through one copy, and the other copy and the child must observe it (`EPIPE` on write, EOF on the peer). Also run it in record and replay mode once the socketpair record hang (`tests/rust/shutdown.rs:26-31`) is fixed.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/474: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/480 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged duplicate https://github.com/rrnewton/hermit/issues/480 section by section. target_policy investigate -> model (the typed handle_shutdown forwards one non-blocking call via record_or_replay; EOF and EPIPE reach the peer through the scheduler-ordered send/receive retry paths). The legacy NEEDS-INVESTIGATION (474) and NEEDS-HANDLER (480) are both superseded by that handler. linux_determinism stays conditional (both twins agreed). 10 flags and 5 tests. Stays todo; no coverage recorded.

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `shutdown`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shutdown&body=Discussion+for+the+%60shutdown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshutdown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shutdown%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: shutdown" (https://github.com/rrnewton/hermit/issues/474), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=shutdown` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `shutdown` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
shutdown(3, SHUT_RD)            = -1 ENOTCONN (Transport endpoint is not connected. (If you are seeing this message in an Eden checkout, try running "eden doctor"))
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Duplicate https://github.com/rrnewton/hermit/issues/480

https://github.com/rrnewton/hermit/issues/480 was filed on 2026-07-24 as "Syscall classification: shutdown" (legacy variant A1) and is now retired into this issue. Its comment history (0 comments) stays on that issue, and this issue's history stays on https://github.com/rrnewton/hermit/issues/474. Its content was merged into the sections above as follows.
- Summary: its wording ("disables receives, sends, or both directions on a connected socket").
- Linux semantics: its shutdown(2) manual link, and its observation that shutting down writes on one end of a local socket pair succeeded.
- Determinism analysis: its CONDITIONAL classification, which agrees with 474. Its point that the call must take part in Hermit's deterministic socket event model.
- Nondeterminism sources: its list (peer actions, protocol state, queued data, socket type, network errors, concurrent threads).
- Hermit design: its recommended action, NEEDS-HANDLER. It is reconciled there with 474's NEEDS-INVESTIGATION: the typed handler supersedes both.

Both twins described shutdown as routed through the old temporary pass-through block. That was true when they were filed; at hermit f4055eb9 shutdown has a typed handler.

Its host evidence, kept verbatim:

> Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=shutdown`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `shutdown` in the UNCLASSIFIED temporary-passthrough block.

```text
shutdown(3, SHUT_WR)            = 0
```
