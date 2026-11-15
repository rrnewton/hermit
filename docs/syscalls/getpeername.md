---
schema: syscall-pages/v1
syscall: getpeername
nr_x86_64: 52
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/373
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpeername&body=Discussion+for+the+%60getpeername%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpeername.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpeername%60%2C+or+in+what+the+page+says%3F%0A
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

`getpeername(2)` returns the address of the socket's connected peer. Detcore forwards it to the kernel. The result is deterministic when the peer's address was itself chosen deterministically, which holds for peers bound through Detcore's `bind(2)` handler. A client port the kernel autobinds during `connect` is still host-chosen.

## Linux semantics

`getpeername(fd, addr, addrlen)` asks the protocol for the remote address of a connected socket and copies it to `addr`. `*addrlen` is the buffer size on entry and the address's full size on return. A short buffer truncates the copy, and the returned length is larger than the buffer. The call does not change socket state.

The peer address exists only while the socket is connected. A TCP socket keeps it through an orderly shutdown by the peer, but a reset moves the socket to a closed state, after which the call returns `ENOTCONN`. A datagram socket has a peer only after `connect`. An AF_UNIX socket reports the name its peer was bound to, which is empty for an unnamed peer such as the other end of a `socketpair`.

### Flags and options

- F1: `*addrlen` in/out: the input is the buffer size; the output is the real address length. A short buffer truncates the copy, and the returned length exceeds the buffer.
- F2: connected AF_INET/AF_INET6 stream socket: the peer's address and port. On a socket returned by `accept`, that is the client's port, which the kernel chose if the client did not bind.
- F3: AF_INET/AF_INET6 datagram socket after `connect`: the default destination. After `connect` with `AF_UNSPEC` there is no peer, giving `ENOTCONN`.
- F4: connected AF_UNIX socket, including either end of a `socketpair`: the peer's pathname, abstract or autobind name. An unnamed peer gives length `sizeof(sa_family_t)`.
- F5: AF_NETLINK: the destination port ID and groups set by `connect`, or zeros when not connected. Netlink does not return `ENOTCONN`.
- F6: socket not connected (never connected, listening, or after a TCP reset): `ENOTCONN`.
- F7: errors: `EBADF` for a closed fd, `ENOTSOCK` for a non-socket fd, `EFAULT` for an unreadable `addrlen` or unwritable `addr`, `EINVAL` for a negative `*addrlen`.
- F8: other families (AF_VSOCK, ...): family-specific peer addresses, returned by the kernel.

## Determinism analysis

`linux_determinism: nondeterministic -> conditional`. The call reports state the socket already has, so it repeats whenever the peer's address repeats and the connection was set up, shut down or reset at the same point relative to the call. Inside one deterministic run with explicitly bound peers, both hold. The legacy sources still apply as conditions, not as reasons the call can never repeat:

### Nondeterminism sources

- External peer and network state: a peer outside the guest, possible only with `--network=host`, has an address and port the guest does not control
- Namespace-assigned addresses: kernel-chosen client ports from an implicit autobind during `connect`, and AF_UNIX autobind names
- Concurrent connect, disconnect and reset timing, which decides between a peer address and `ENOTCONN` (F6)

## Hermit design

Target policy: `model`. The kernel reports the peer, and Detcore makes peer addresses deterministic where they are assigned and schedules the threads that connect and disconnect.

What the code does now:

- The typed dispatch arm at `detcore/src/lib.rs:2565` calls `handle_getpeername` (`detcore/src/syscalls/files.rs:3901`), which forwards the call through `record_or_replay` (`detcore/src/syscalls/files.rs:3906`). Both carry an autonomous-bot "needs human review" tag.
- `handle_bind` (`detcore/src/syscalls/files.rs:4014`) makes port-0 ports and AF_UNIX autobind names deterministic, so a peer that bound through it has a repeatable address. See the `bind` body.
- By default the guest has a private network namespace with only loopback (`hermit-cli/src/bin/hermit/global_opts.rs:53`), so every peer is another guest process and its IP address repeats.
- Record mode captures the output buffer: the recorder sends `Getpeername` to `handle_sockopt_family` (`hermit-cli/src/recorder.rs:628`), and the replayer returns the recorded bytes (`hermit-cli/src/replayer.rs:535`).

Why `model`: code facts rule out `passthrough` for a determinized syscall. `emulate` would need Detcore to track every connection's peer. `refuse` would break servers that log client addresses.

Gap between the code and `model`:

- **Kernel-chosen client ports (F2).** `connect` (`detcore/src/syscalls/io.rs:1336`) forwards an unbound socket to the kernel without asking the port allocator; only `handle_bind` sends `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119`). A server's `getpeername` on an accepted connection then reports a random client port. Like `getsockname`'s F4 gap, the fix belongs in `connect`.

Target (owner decision 12): every local port the kernel would choose for an unbound AF_INET or AF_INET6 socket comes from Detcore's global port allocator, the one an explicit `bind` to port 0 already uses. `handle_bind` requests the port with `GlobalRequest::RequestPort` (`detcore/src/syscalls/files.rs:4119` for AF_INET, `detcore/src/syscalls/files.rs:4158` for AF_INET6); the global tool serves the request at `detcore/src/tool_global.rs:1731` from the range in the host's `ip_local_port_range` (`detcore/src/tool_global.rs:2783`); and `release_port_for_open_file` (`detcore/src/syscalls/files.rs:952`) frees the port when the socket's last descriptor closes. An implicit bind, meaning `connect`, `listen`, `sendto`, `sendmsg` or `sendmmsg` on an unbound socket, must take its port the same way and bind the socket to it before the call reaches the kernel, so that `getsockname`, the peer's `getpeername` and the peer's `accept` report the same port in every run. `getpeername` itself needs no change: the client port it reports for F2 becomes the allocator's once `connect` takes one. Today `handle_connect` (`detcore/src/syscalls/io.rs:1336`) forwards an unbound socket to the kernel, which picks a random port.

## Recommended tests

- T1 (covers F4): `getpeername` on one end of an AF_UNIX `socketpair` returns AF_UNIX with the family-only length. Add a connected pair where the listener has an abstract name.
  Existing candidate: `tests/c/sockname_unnamed.c` (getpeername at line 53; cell `c-programs/sockname-unnamed`, `tests/e2e/manifests/c-programs.yaml:14472`, verify ci on ptrace). It checks the family but not the length.
- T2 (covers F2): a loopback TCP server and a client that binds port 0 before `connect`. Both `getpeername` results must be identical across `--verify` runs on ptrace. Repeat with a client that does not bind; that part is expected to fail until `connect` uses the allocator.
- T3 (covers F3, F6): a UDP socket reports `ENOTCONN`, then its peer after `connect`, then `ENOTCONN` after `connect(AF_UNSPEC)`. A listening TCP socket reports `ENOTCONN`. After the peer closes with `SO_LINGER` 0 (a reset), the call returns `ENOTCONN` in every run.
- T4 (covers F5): netlink socket: zeros before `connect`, then the destination after `connect` to the kernel (`nl_pid` 0).
- T5 (covers F1): a 4-byte buffer for an AF_INET6 peer: the returned length is 28, and bytes past the fourth are untouched.
- T6 (covers F7, F8): `EBADF` for fd -1, `ENOTSOCK` for a pipe, `EFAULT` for a bad `addrlen`, `EINVAL` for `*addrlen == -1`. An AF_VSOCK peer passes through where the host allows it.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/373: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model; linux_determinism nondeterministic -> conditional (the call reports existing state; repeats when peer addresses and connect/reset order repeat); F1-F8 inventoried, T1-T6 proposed; gap: kernel-chosen client ports from connect without bind.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 12 applied (Target paragraph: connect on an unbound socket takes an allocator port, so the F2 client port repeats with no change here); no new F/T ids; removed the open question on the connect allocator and the empty heading

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getpeername`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getpeername&body=Discussion+for+the+%60getpeername%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetpeername.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getpeername%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getpeername" (https://github.com/rrnewton/hermit/issues/373), legacy variant A2.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=getpeername -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(52, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
getpeername(-1, 0xffffffffffffffff, 0xffffffffffffffff) = -1 EBADF (Bad file descriptor)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
