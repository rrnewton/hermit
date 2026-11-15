---
schema: syscall-pages/v1
syscall: sendfile
nr_x86_64: 40
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/437
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sendfile&body=Discussion+for+the+%60sendfile%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsendfile.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sendfile%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

Copies up to `count` bytes from `in_fd` to `out_fd` inside the kernel, with no user-space buffer. Hermit mediates copies between regular files and memfds: it orders the write to the destination file through the scheduler and records the result. It returns `ENOSYS` for socket, pipe and procfs endpoints, where Linux would succeed.

## Linux semantics

`sendfile(out_fd, in_fd, offset, count)` reads from `in_fd` and writes to `out_fd`, and returns the number of bytes copied. `in_fd` must support mmap-like reads, so it cannot be a socket. Since Linux 2.6.33 `out_fd` can be any file, not only a socket. Since Linux 5.12, when `out_fd` is a pipe, the call is carried out as a splice and `in_fd` may be any file that supports splice reads. A host probe on 2026-07-24 copied 8 bytes from `/etc/hostname` to a temporary file and got 8 (see Additional information).

The copy is not atomic with respect to the destination's readers. A copy to a socket or pipe can block or be partial, like `write(2)`. The number of bytes moved per call is capped at `MAX_RW_COUNT` (0x7ffff000).

### Flags and options

- F1: `offset == NULL`: read from `in_fd`'s file offset and advance it by the bytes copied.
- F2: `offset != NULL`: read from `*offset`, store the updated position in `*offset`, and leave `in_fd`'s file offset unchanged. `ESPIPE` if `in_fd` is not seekable.
- F3: `count`: at most `MAX_RW_COUNT` bytes per call. A short count at end of file, 0 at end of file, and 0 for `count == 0`.
- F4: `in_fd` type: must be a regular file (or another file with mmap-like reads); a socket or pipe input gives `EINVAL`, except for the Linux 5.12 splice path when `out_fd` is a pipe.
- F5: `out_fd` is a regular file or memfd.
- F6: `out_fd` is a socket, the classic use: the copy can block on a full send buffer and can be partial.
- F7: `out_fd` is a pipe, including stdout when it is a pipe: the copy can block on a full pipe and can be partial.
- F8: `out_fd` opened with `O_APPEND` gives `EINVAL`.
- F9: `out_fd` with `O_NONBLOCK` gives `EAGAIN` when the copy would block.
- F10: errors: `EBADF` (an fd not open, `in_fd` not readable or `out_fd` not writable), `EFAULT` (bad `offset` pointer), `EINVAL` (descriptor locked or unusable, negative offset), `EOVERFLOW` (`count` too large for the offset), `EIO`, `ENOMEM`.
- F11: a signal while the copy is blocked gives `EINTR` if nothing was copied, and a short count otherwise.
- F12: `in_fd` on procfs or sysfs: the kernel copies live kernel-generated content.

## Determinism analysis

`linux_determinism: conditional`. A copy between regular files, or memfds, is deterministic for fixed file contents and offsets: the count and the bytes written depend only on file state (F1-F5, F8, F10). It stops being deterministic when:

- the destination is a socket or pipe (F6, F7, F9, F11), because how many bytes fit before the call blocks or returns short depends on how fast the reader drains the other end;
- the source is a procfs or sysfs file (F12), because its contents are live kernel state;
- another process writes the same destination file at the same time, because the order of the two writes is decided by the host scheduler.

### Nondeterminism sources

- Socket and pipe destinations: partial counts, `EAGAIN` and `EINTR` depend on reader timing.
- Live procfs and sysfs content in the source.
- Concurrent writers to the destination file.
- The offset update must stay ordered with other reads, writes and seeks on the same open file description, or a replay sees different positions.

## Hermit design

Target policy: `model`. Detcore decides which endpoint types the call may touch and orders every allowed copy with the other file operations. The dispatch arm at `detcore/src/lib.rs:2374` calls `handle_sendfile` at `detcore/src/syscalls/files.rs:2155`:

- It looks up both descriptors' types, and the destination's resource and inode, in Detcore's fd table.
- A procfs source returns `ENOSYS` (`detcore/src/syscalls/files.rs:2183`). The comment at `detcore/src/syscalls/files.rs:2173` explains why: a procfs descriptor is classified as a regular file, so without this check the kernel would copy live bytes that the mediated `read` path sanitizes (F12).
- Any source or destination that is not a regular file or memfd returns `ENOSYS` (`detcore/src/syscalls/files.rs:2187`). This covers socket and pipe destinations (F6, F7) and non-regular sources (F4).
- For an allowed copy it requests write permission on the destination's file contents, with a determinized inode, and waits for it (`detcore/src/syscalls/files.rs:2210`). It then runs the call through `record_or_replay` (`detcore/src/syscalls/files.rs:2214`), so the count and both offset updates are recorded and replayed in order (F1, F2, F3, F5).
- With `virtualize_metadata`, a copy of more than zero bytes updates the destination's virtual timestamps (`detcore/src/syscalls/files.rs:2219`).
- Kernel errors for the allowed types (F8, F10) come back unchanged from the kernel.

Why `model` and not the alternatives:

- `passthrough` would let a copy to a socket or pipe block inside the kernel, where the deterministic scheduler cannot see it, and would let a procfs source leak live kernel state.
- `refuse` for every case would break the common regular-file copy that the current code already handles deterministically.
- `emulate` with `read` and `write` inside Detcore is a possible way to support socket and pipe destinations later, but it is a design for the gap below, not a different policy.

What remains before the code fully realizes `model`:

- **F6, F7, F9, F11:** socket and pipe destinations return `ENOSYS`. The doc comment at `detcore/src/syscalls/files.rs:2143` says this makes libc or the application fall back to `read` and `write`. Python's `socket.sendfile` and some servers do fall back. glibc's `sendfile(3)` is, as far as I know, a plain syscall wrapper with no fallback, so the claim in the `TODO-HUMAN-REVIEW(PR-973)` comment (https://github.com/rrnewton/hermit/issues/973, an issue despite the PR- prefix) that "glibc's sendfile does exactly this" looks wrong. A program that calls `sendfile` to a socket without its own fallback (for example a static file server) fails under Hermit where it works natively. See Open questions.
- **F4:** a pipe or socket source returns `ENOSYS`, while Linux returns `EINVAL` (or, since 5.12 with a pipe destination, succeeds).
- **F12:** a procfs source returns `ENOSYS`; Linux succeeds. Refusing is the deterministic choice until the copy can go through the procfs snapshot.
- `ENOSYS` is also the errno for a type Hermit simply does not support, so a caller cannot tell "this kernel lacks sendfile" from "Hermit refused this endpoint".

`linux_determinism` stays `conditional`, as explained above.

Target (owner decision 14): a socket or pipe destination (F6, F7) takes the same nonblocking probe plus scheduler wait that `write` uses through `execute_nonblockable_fd_syscall` (`detcore/src/syscalls/helpers.rs:202`), with `SO_SNDTIMEO` measured on virtual time for a socket (decision 13). In run mode those descriptors are already physically nonblocking (see the `socket` body), so the probe is the plain kernel call, and an `EAGAIN` on a guest-blocking destination is the point to wait and retry; a guest `O_NONBLOCK` destination returns `EAGAIN` (F9). Until that is built, the refusal stays: the guest sees `ENOSYS` (`detcore/src/syscalls/files.rs:2190`). Programs with their own fallback recover: Python's `socket.sendfile` falls back to `send` when `os.sendfile` fails before sending anything. A program that calls `sendfile` with no fallback still fails under Hermit.

## Recommended tests

- T1 (covers F1, F3, F5): copy between two regular files with a NULL offset under `hermit run --verify` on ptrace. Check that the source offset advances, that a request past end of file returns a short count and then 0, and that `count == 0` returns 0.
  Existing candidate: `tests/c/sendfile_copy.c` (cell `c-programs/sendfile-copy`, `tests/e2e/manifests/c-programs.yaml:14731`, verify ci on ptrace). It covers the NULL-offset advance; it does not test `count == 0`.
- T2 (covers F2, F5): copy with an explicit offset. `*offset` must be updated and the source's own file offset must not move.
  Existing candidates: `tests/c/sendfile_copy.c`; `tests/c/syscall_quick_wins.c:74` (cell `c-programs/syscall-quick-wins`, `tests/e2e/manifests/c-programs.yaml:9345`, verify ci on ptrace; raw `syscall(SYS_sendfile, ...)` with an offset that becomes 1); `tests/c/record_replay_file_state_regular_sink.c:56` (cell `c-programs/record-replay-file-state-regular-sink`, `tests/e2e/manifests/c-programs.yaml:7038`, verify ci on ptrace).
- T3 (covers F6, F9): copy a file to a connected AF_UNIX socket and to a loopback TCP socket, blocking and with `O_NONBLOCK` on a full send buffer. Linux copies the bytes, or returns `EAGAIN` when nonblocking; at this SHA Hermit returns `ENOSYS`. The test records that gap until the open question is settled.
- T4 (covers F7): copy a file to a pipe and to stdout when stdout is a pipe. Linux succeeds; Hermit returns `ENOSYS` at this SHA.
  Related: `tests/c/record_replay_file_state.c:117` copies to `STDOUT_FILENO`; its cell `c-programs/record-replay-file-state` (`tests/e2e/manifests/c-programs.yaml:6977`) has verify ci on ptrace set to false.
- T5 (covers F4, F8, F10): a raw `syscall(SYS_sendfile, ...)` error matrix, compared with native results: a pipe or socket `in_fd` (`EINVAL` natively), an `O_APPEND` `out_fd` (`EINVAL`), a closed fd and a read-only `out_fd` (`EBADF`), an unmapped `offset` (`EFAULT`), a negative `*offset` (`EINVAL`), a non-NULL offset on a non-seekable input (`ESPIPE`), and a `count` larger than `MAX_RW_COUNT` (clamped).
- T6 (covers F11): send a signal to a thread blocked in a copy to a pipe whose reader is stalled. Expect `EINTR` when nothing was copied and a short count otherwise. Depends on F7 being supported; until then, assert that a regular-file copy never returns `EINTR`.
- T7 (covers F12): a copy from `/proc/self/stat` returns `ENOSYS`, and the destination stays empty.
  Existing candidate: `check_sendfile_refused` at `tests/c/procfs_positioned_probe.c:90` (cell `c-programs/procfs-positioned-probe`, `tests/e2e/manifests/c-programs.yaml:6208`, verify ci on ptrace).
- T8 (covers F1, F2, F5): record and replay a guest that copies regular files with both offset modes. Replay must return the same counts and leave the same offsets.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/437: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (regular/memfd copies mediated and recorded); linux_determinism stays conditional; socket, pipe and procfs endpoints return ENOSYS where Linux succeeds; left todo with an open question on socket/pipe destinations and the glibc fallback claim.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 14 applied (Target paragraph: socket and pipe destinations use the nonblocking probe plus scheduler wait, SO_SNDTIMEO on virtual time; ENOSYS stays until then); no new F/T ids; removed the open question on supporting socket and pipe destinations
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/sendfile_copy.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sendfile`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sendfile&body=Discussion+for+the+%60sendfile%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsendfile.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sendfile%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sendfile" (https://github.com/rrnewton/hermit/issues/437), legacy variant A1. Its comment history stays on that issue. The fallback-block statement in the evidence below was true when it was written; at f4055eb9 the call is dispatched to `handle_sendfile`.

A stale comment in `tests/c/short_io_split_identity.c:46` points at `files.rs:840-844` for the socket `ENOSYS` path; at this SHA the check is at `detcore/src/syscalls/files.rs:2187`.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sendfile` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `sendfile` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
sendfile(4, 3, NULL, 8)         = 8
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Open questions

- The claim in the comments at `detcore/src/syscalls/files.rs:2143` and `detcore/src/syscalls/files.rs:2173` that libc falls back to `read` and `write` on `ENOSYS` should be checked against glibc's source. If it is wrong, the comments should be corrected when the code is next touched.
