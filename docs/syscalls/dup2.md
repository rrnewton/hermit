---
schema: syscall-pages/v1
syscall: dup2
nr_x86_64: 33
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+dup2&body=Discussion+for+the+%60dup2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fdup2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60dup2%60%2C+or+in+what+the+page+says%3F%0A
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

`dup2(oldfd, newfd)` makes `newfd` refer to the same open file description as `oldfd`. If `newfd` is already open, it is closed first, silently. Detcore handles the call in `handle_dup2` (`detcore/src/syscalls/files.rs:3534`). The kernel performs the duplication, and Detcore then mirrors the result in its own descriptor table (`dup_fd`, `detcore/src/tool_local.rs:787`). If that replaced the last alias of an open file, Detcore also releases any port that file held.

Manual reference: Linux man-pages dup(2).

## Linux semantics

`dup2` installs a copy of `oldfd` at the exact slot `newfd`. Linux does the close and the install as one atomic step, so no other thread can take the slot in between. The new descriptor shares the open file description with `oldfd`: file offset, status flags (`O_APPEND`, `O_NONBLOCK`, ...), and the `flock` lock all belong to the description. Descriptor flags do not: `FD_CLOEXEC` is always clear on `newfd`.

When `newfd` was open, its old file is closed as if by `close(2)`, but any error from that close is discarded. If `newfd` held the last reference to its open file description, that release happens here: a pipe write end can produce EOF for its reader, `flock` locks drop, a socket shuts down.

There is no flags argument. `dup3` is the variant that takes `O_CLOEXEC` and that refuses `oldfd == newfd`.

### Flags and options

- F1: `oldfd == newfd` and `oldfd` is open: returns `newfd` and does nothing. In particular `FD_CLOEXEC` on that descriptor is left as it was.
- F2: `oldfd == newfd` and `oldfd` is not open: `EBADF`
- F3: `oldfd` is not open (with `oldfd != newfd`): `EBADF`, and `newfd` is left untouched
- F4: `newfd` is negative or not below the `RLIMIT_NOFILE` soft limit: `EBADF`
- F5: `newfd` is a free slot: the copy is installed there and `newfd` is returned
- F6: `newfd` is already open: its old file is closed first, with any close error discarded. If that was the last reference to its open file description, the description is released, with all the side effects of a last close.
- F7: `FD_CLOEXEC` is clear on `newfd` after a successful `oldfd != newfd` call, whatever it was on `oldfd`
- F8: `oldfd` and `newfd` share one open file description: one offset, one set of status flags, one `flock` lock
- F9: `EBUSY`: on Linux, returned when `newfd` is a slot that another thread is in the middle of filling with `open` or `dup`

## Determinism analysis

`linux_determinism: conditional`. Given a fixed descriptor table, the result is a pure function of the arguments. The call repeats unless one of these applies:

- Another thread in the same process is opening or closing descriptors at the same time. Whether `dup2` sees the slot free, open, or half-filled (`EBUSY`, F9) depends on host scheduling.
- The implicit close of `newfd` (F6) is the last reference to an open file description that another process is watching. When that process sees EOF, or when a lock becomes free, depends on scheduling between the two processes.

## Hermit design

Target policy: `model`. The kernel stays the authority for the descriptor table, and Detcore keeps a mirror of it. The handler at `detcore/src/syscalls/files.rs:3534` does this:

- It sends the call to the kernel unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:3541`, delegating at `detcore/src/tool_local.rs:382`). Any kernel error (F2, F3, F4, F9) returns at that point, before the mirror changes.
- After a success it calls `dup_fd(old_fd, new_fd, OFlag::empty())` (`detcore/src/syscalls/files.rs:3542`). When live descriptor discovery is on, the thread-state wrapper first learns `oldfd` from the running process (`detcore/src/tool_local.rs:2627`).
- In the table itself (`detcore/src/tool_local.rs:787`), `oldfd == newfd` only checks that `oldfd` exists and changes nothing (F1). Otherwise it clones the old entry under the new number with empty descriptor flags, so `FD_CLOEXEC` is clear (F7). The clone keeps the same open file identity, so later lock and offset tracking is shared (F8).
- The entry that was at `newfd` is replaced. Its open file id is returned only if it was the last alias (`open_file_alias_count() == 1`, `detcore/src/tool_local.rs:801`). In that case the handler sends `GlobalRequest::ReleasePort` (`detcore/src/syscalls/files.rs:3545`, `release_port_for_open_file` at `detcore/src/syscalls/files.rs:952`), so a socket bound through the replaced file gives up its virtual port (F6).

Why `model`: the kernel already does the right thing for the guest. What Hermit must own is its own view of the table, because that view drives `flock` state, port release and close-on-exec handling later in the run. `passthrough` would let that view go stale. `emulate` would duplicate the kernel's fd table for no benefit.

The scheduling sources above are removed by sequentialized execution (`config.sequentialize_threads`, `hermit-cli/src/bin/hermit/run.rs:3667`): only one guest thread makes a descriptor-table change at a time, so the `EBUSY` window (F9) should not be reachable under Hermit. This is a reading of the design, not a measurement.

## Recommended tests

- T1 (covers F1): `dup2(fd, fd)` on an open `O_CLOEXEC` descriptor returns `fd` and leaves `FD_CLOEXEC` set.
  Existing candidate: `dup2_same_fd_preserves_cloexec` at `detcore/tests/misc/mod.rs:1695`, a Detcore test run under `det_test_fn_sequential_without_pmu`. It is a Detcore harness test, not a `hermit run` cell.
- T2 (covers F2, F3, F4): with a raw `syscall(SYS_dup2, ...)`, check `EBADF` for `dup2(closed, closed)`, `dup2(closed, open)` (and that the open `newfd` still works afterwards), `dup2(fd, -1)`, and `dup2(fd, RLIMIT_NOFILE soft limit)`.
- T3 (covers F5, F7): `dup2` from a `FD_CLOEXEC` descriptor into a free slot returns that slot, and the slot has `FD_CLOEXEC` clear.
  Existing candidate: `tests/c/fd_duplication.c:106` (cell `c-programs/fd-duplication`, `tests/e2e/manifests/c-programs.yaml:15855`, verify ci on ptrace).
- T4 (covers F6, F8): `dup2` a regular file over the write end of a pipe whose read end another process is reading; the reader must see EOF at the same point in every run under `hermit run --verify`. Also check that a write through `newfd` advances the offset seen through `oldfd`, and that `O_APPEND` set with `F_SETFL` on one is visible on the other.
  Related existing test: `detcore/tests/lit/dup2_existing_fd/main.rs` replaces stdin with `dup2` and reads the pipe afterwards. Its FIXME at `detcore/tests/lit/dup2_existing_fd/main.rs:36` notes a scheduler timeout around its `alarm(5)` guard.
- T5 (covers F6): bind a socket, `dup2` another descriptor over it so the replaced entry was its last alias, then bind a new socket to the same virtual port. It must succeed, showing that `ReleasePort` ran.
- T6 (covers F9): two threads, one looping `open`/`close` on a slot and one calling `dup2` into that slot. Under Hermit no `EBUSY` must appear, and the outcome must be the same in every run with the same seed.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel performs the dup; Detcore mirrors it in its fd table, clears FD_CLOEXEC on the copy and releases the virtual port of a replaced last alias); linux_determinism conditional; 9 flag behaviours and 6 tests listed; no coverage token yet because no test has been run against this list, so tracking_status stays todo; one open question on mirror failure after a kernel success
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T3 covered by tests/c/fd_duplication.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `dup2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+dup2&body=Discussion+for+the+%60dup2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fdup2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60dup2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `dup2`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- The handler updates its mirror only after the kernel has already performed the dup (`detcore/src/syscalls/files.rs:3541` then `detcore/src/syscalls/files.rs:3542`). If Detcore's table does not know `oldfd`, for example a descriptor inherited from the host while live discovery is off, `with_detfd` returns `EBADF` (`detcore/src/tool_local.rs:740`). The guest would then see `EBADF` for a dup that did happen. This is a hypothesis from the code and has not been measured. Should the mirror failure be fatal, or fall back to discovering the descriptor?
