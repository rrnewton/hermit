---
schema: syscall-pages/v1
syscall: dup3
nr_x86_64: 292
linux_determinism: deterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+dup3&body=Discussion+for+the+%60dup3%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fdup3.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60dup3%60%2C+or+in+what+the+page+says%3F%0A
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

`dup3(oldfd, newfd, flags)` makes `newfd` refer to the same open file description as `oldfd`. If `newfd` is already open, it is closed first, in the same step. `flags` can only set close-on-exec. Detcore handles the call at `detcore/src/syscalls/files.rs:3552`. The kernel does the duplication through `record_or_replay`. Detcore then copies its own record of the descriptor into the `newfd` slot, and if a socket's last descriptor was overwritten, it releases the port Detcore had assigned to that socket.

Manual reference: Linux man-pages dup(2).

## Linux semantics

`dup3` acts only on the calling process's descriptor table. The new descriptor shares the open file description with `oldfd`: the file offset, the status flags (`O_APPEND`, `O_NONBLOCK`, ...), `flock` locks and socket state are all shared. The close-on-exec flag belongs to the descriptor and is not shared. It is set from `flags` alone and is not copied from `oldfd`.

If `newfd` was open, the kernel closes it inside the same table update, so no other thread can grab the slot in between. Any error from that close is thrown away. The close has the usual side effects: the process drops every POSIX record lock (`fcntl` lock) it holds on that file, and if this was the last reference to the description, the description is released, along with its `flock` lock.

`dup3` differs from `dup2` in two ways: it takes `flags`, and when `oldfd == newfd` it fails with `EINVAL` instead of returning `newfd`.

The kernel checks in this order: `flags` (`EINVAL`), then `oldfd == newfd` (`EINVAL`), then the range of `newfd` (`EBADF`), then `oldfd` (`EBADF`).

### Flags and options

- F1: `flags = O_CLOEXEC`: `newfd` is close-on-exec
- F2: `flags = 0`: `newfd` is not close-on-exec, even when `oldfd` is
- F3: any other bit in `flags`: `EINVAL`
- F4: `oldfd == newfd`: `EINVAL`. `dup2` would return `newfd` here.
- F5: `oldfd` is not an open descriptor: `EBADF`
- F6: `newfd` is negative or not below `RLIMIT_NOFILE`: `EBADF`
- F7: `newfd` is already open: it is closed atomically and replaced, and any close error is lost. The process's `fcntl` record locks on that file are released. If this was the last reference to the description, the description and its `flock` lock are released too.
- F8: description sharing: `newfd` and `oldfd` share the offset, the status flags and the lock state of one open file description
- F9: `EBUSY` when `newfd` is a slot that another thread's `open`/`dup` has reserved but not yet filled. The man page also lists `EINTR`.

## Determinism analysis

`linux_determinism: deterministic`. The result depends only on the arguments and on the calling process's descriptor table. Hermit serializes guest threads, so the table's contents at each call are fixed by the schedule. The only race on Linux is `EBUSY` (F9), which needs another thread to be inside `open` at the same moment. A sequentialized schedule rules that out.

Closing a replaced `newfd` can have effects other processes see: a pipe reader may get EOF, or another process may now win a lock. These follow deterministically from the call; when the other side observes them is a matter for that side's own syscalls.

## Hermit design

Target policy: `model`. The kernel owns the real descriptor table. Detcore keeps a mirror of it, because later calls (`fcntl`, the nonblocking-I/O dispatch, port release) read the mirror instead of asking the kernel. The handler keeps that mirror in step:

- The call goes to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:3560`, defined at `detcore/src/tool_local.rs:382`). So the order of the kernel's checks and every errno (F3 to F6, F9) come from the kernel. If the kernel fails, the `?` returns before the mirror is touched.
- `FileMetadata::dup_fd` (`detcore/src/tool_local.rs:787`) clones Detcore's entry for `oldfd`. The clone shares the open-file record, which is an `Arc` (`detcore/src/fd.rs:333`), so status flags and cached lock modes stay shared (F8). It then sets the close-on-exec bit from `flags` through `with_fd_flags` (`detcore/src/fd.rs:249`) and inserts the entry at `newfd`, overwriting the old one (F1, F2, F7).
- `dup_fd` returns the replaced description's id only when this slot held the description's last reference (`detcore/src/tool_local.rs:803`). In that case the handler sends `GlobalRequest::ReleasePort` (`detcore/src/syscalls/files.rs:3563`, `detcore/src/syscalls/files.rs:952`), so a port Detcore assigned for `bind` to port 0 is returned to the pool, just as `close` returns it.
- The `oldfd == newfd` branch in `dup_fd` (`detcore/src/tool_local.rs:793`) serves `dup2`. `dup3` never gets there, because the kernel has already returned `EINVAL` (F4).

`dup`, `dup2` and `dup3` share this pattern (`detcore/src/lib.rs:2533` to `detcore/src/lib.rs:2535`). Detcore does not refuse or emulate any part of the call, and `passthrough` would let the mirror drift from the kernel. So `model` is what the code already does.

What remains before the code fully realizes `model`:

- **An `oldfd` the kernel knows but Detcore does not.** `with_detfd` returns `EBADF` when Detcore has no entry for the descriptor (`detcore/src/tool_local.rs:740`). On ptrace, Detcore looks up a missing descriptor in the live process only when `discover_live_file_metadata` is set (`detcore/src/tool_local.rs:2634`). The comments at `detcore/src/tool_local.rs:695` and `detcore/src/tool_local.rs:2227` tie that discovery to SaBRe; otherwise the starting table holds only stdio, from `setup_stdio` (`detcore/src/tool_local.rs:2230`). If `oldfd` is valid in the kernel but missing from the mirror, for example a descriptor above 2 inherited from the launching shell, the kernel `dup3` succeeds, then Detcore returns `EBADF`, and `newfd` stays open in the guest behind that error. The same class of bug once made `dup` of an epoll descriptor fail and broke rustup's tokio runtime; the comment at `detcore/src/syscalls/io.rs:1151` records it. For `dup3` this is a hypothesis from the code, not a measurement; T4 checks it.
- **`fcntl` record locks released by F7.** Detcore does not model POSIX record locks here; the kernel releases them, and nothing in the mirror needs to change. This is a note, not a gap.

## Recommended tests

- T1 (covers F1, F2, F8): `dup3` into a free slot with `O_CLOEXEC` and with `0`, when `oldfd` is close-on-exec and when it is not. Check `F_GETFD` on the new descriptor, and check that an `lseek` through one descriptor moves the other, under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/fd_duplication.c:115` does `dup3(base, slot3, O_CLOEXEC)` into a freed slot and checks close-on-exec (cell `c-programs/fd-duplication`, `tests/e2e/manifests/c-programs.yaml:15855`, verify ci on ptrace); it does not try `flags = 0`. `detcore/tests/lit/fcntl_getfd_setfd/main.rs:21` runs a raw `dup3(2, 255, O_CLOEXEC)` and checks `F_GETFD`, but it is a Buck lit test, not a ptrace CI cell or cargo test.
- T2 (covers F3, F4, F5, F6): with a raw `syscall(SYS_dup3, ...)`, check the Linux errno for `flags = O_NONBLOCK`, for `dup3(0, 0, 0)`, for a closed `oldfd`, and for `newfd` equal to the `RLIMIT_NOFILE` soft limit. Also check that the failed calls left no new descriptor open.
  Existing partial coverage: `tests/c/errno_path_identity.c:129` attempts `dup3(0, 0, 0)` (cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1777`, verify ci). It checks that the errno is the same across runs, not that it is `EINVAL`.
- T3 (covers F7): `dup3` over an open descriptor. Cases: the last write end of a pipe (the reader must then see EOF); a file that holds a `flock` lock (a second open's `LOCK_EX|LOCK_NB` must then succeed); and a socket bound to port 0 (after the replacement, a new `bind` to port 0 must behave as it would after a `close`).
- T4 (covers F5, F8): `dup3` from a descriptor above 2 that the launching shell opened, as in `bash -c 'exec 3<file; prog'` where `prog` calls `dup3(3, 10, 0)` and reads through fd 10. Linux succeeds. Record what Hermit on ptrace returns. If it is `EBADF`, file it as a defect against the mirror's handling of inherited descriptors.
- T5 (covers F9): two threads, one looping on `open`/`close` and the other on `dup3` into the same slot number. Under Hermit, `dup3` never returns `EBUSY`, and the result is the same on every `--verify` run.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel duplicates; Detcore mirrors the descriptor, sets per-slot close-on-exec, releases the port of a replaced last socket descriptor); linux_determinism deterministic; 9 flag behaviours and 5 tests listed; open question: EBADF after a successful kernel dup3 when oldfd is missing from the ptrace mirror; no coverage token, tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `dup3`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+dup3&body=Discussion+for+the+%60dup3%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fdup3.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60dup3%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `dup3`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Does `dup3` (and `dup`, `dup2`) from a descriptor the kernel knows but Detcore's mirror does not, such as one inherited from the launching shell above fd 2, return `EBADF` on ptrace after the kernel call has already succeeded? The reasoning is in Hermit design; T4 measures it. If it does, the fix is for the ptrace backend too to discover the missing descriptor, or for the handler to undo the kernel duplication. That choice belongs to the owner.
