---
schema: syscall-pages/v1
syscall: fadvise64
nr_x86_64: 221
linux_determinism: deterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fadvise64&body=Discussion+for+the+%60fadvise64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffadvise64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fadvise64%60%2C+or+in+what+the+page+says%3F%0A
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

`fadvise64(fd, offset, len, advice)` tells the kernel how the program expects to access a byte range of an open file, so the page cache can read ahead or drop pages; glibc's `posix_fadvise` wraps it and returns the error number directly. Detcore answers every call itself with 0 and never forwards it (`detcore/src/lib.rs:2402`), so the hint has no effect on the host and none of the kernel's error results (`EBADF`, `ESPIPE`, `EINVAL`) reach the guest.

Manual reference: Linux man-pages posix_fadvise(2).

## Linux semantics

The call never changes file contents or the file position. Its effects are on the host page cache and readahead state of the open file description: `WILLNEED` starts asynchronous readahead of the range, `DONTNEED` starts writeback of dirty pages in the range and drops the clean ones, `SEQUENTIAL` enlarges the readahead window, `RANDOM` turns readahead off, and `NORMAL` restores the default. `NOREUSE` was a no-op before Linux 6.3 and since then marks the file so its pages age out of the cache sooner. A `len` of 0 means "to the end of the file", and a range past the end is accepted.

Argument checking: the fd must refer to an open file that is not an `O_PATH` descriptor (`EBADF`); a pipe or FIFO gives `ESPIPE`; a negative `len` or an unknown advice value gives `EINVAL`. On filesystems with no page cache to steer (DAX, and those backed by the no-op backing device such as tmpfs and procfs) every valid advice value returns 0 and does nothing, and an unknown one is still `EINVAL`.

The page-cache effects are visible to a program only indirectly: through `mincore` on a mapping, `cachestat`, `preadv2` with `RWF_NOWAIT`, or I/O timing.

### Flags and options

- F1: `POSIX_FADV_NORMAL` (0): restore default readahead for the open file description.
- F2: `POSIX_FADV_RANDOM` (1): disable readahead.
- F3: `POSIX_FADV_SEQUENTIAL` (2): enlarge the readahead window.
- F4: `POSIX_FADV_WILLNEED` (3): start asynchronous readahead of the range into the page cache.
- F5: `POSIX_FADV_DONTNEED` (4): start writeback of dirty pages in the range and drop clean cached pages; file contents are unchanged.
- F6: `POSIX_FADV_NOREUSE` (5): a no-op before Linux 6.3; since then pages of the file are aged out of the cache sooner.
- F7: range arguments: `len` 0 means to the end of the file, a range past the end is accepted, and a negative `len` gives `EINVAL`.
- F8: unknown advice value: `EINVAL`.
- F9: `fd` not open, or an `O_PATH` descriptor: `EBADF`.
- F10: `fd` refers to a pipe or FIFO: `ESPIPE`.
- F11: files with no page cache to steer (DAX, tmpfs, procfs and other no-op backing devices): every valid advice returns 0 with no effect.

## Determinism analysis

`linux_determinism: deterministic`. The return value depends only on the arguments and on the kind of file `fd` refers to, all of which are guest state. The call changes no guest-visible data: the page cache it steers is host state, and the only way to observe it (`mincore`, `cachestat`, `RWF_NOWAIT` reads, timing) goes through other syscalls whose own tracking bodies handle that exposure. Repeating the call with the same arguments gives the same result.

### Nondeterminism sources

- None in the return value.
- Indirect only: the page-cache residency the hint changes (F4, F5, F6) is shared host state that other host processes also change, and it is visible through other syscalls and through timing, not through this call.

## Hermit design

Target policy: `emulate`. Detcore owns the answer and does not forward the hint. The dispatch arm at `detcore/src/lib.rs:2402` returns `Ok(0)` for every call, with the comment at `detcore/src/lib.rs:2401` stating that the call is advisory and that fixed success keeps its API contract. The arm was added in commit 5c5c73f662 (2026-07-22). The handler table still knows the call's fd (`detcore/src/syscalls/helpers.rs:860`), and its virtual cost is the I/O constant (`detcore/src/syscall_time.rs:82`).

Why `emulate` and not the alternatives: dropping the hint keeps the guest from changing host page-cache state that it cannot observe deterministically anyway, and costs nothing; `refuse` would break programs such as `cp`, `tar` and databases that issue hints as a matter of course; `passthrough` would be faithful for errors, but it would let the guest's hints change host cache state that other runs and other host processes then see through timing.

What remains before the code fully realizes `emulate`:

- **Error results are not reproduced (F8, F9, F10, and negative `len` in F7).** The arm returns 0 before looking at any argument, so `posix_fadvise` on a closed fd, on a pipe, with advice 99 or with a negative `len` succeeds under Hermit while Linux fails. These results depend only on guest state, so an emulation can reproduce them.
- **F1 to F6 and F11** are realized as far as the guest can tell: Linux returns 0 for each valid advice on a regular file, and Hermit returns 0.

Target (owner decision 6): an emulated call owes Linux's argument checks in kernel order, as `handle_sched_setattr` (`detcore/src/syscalls/threads.rs:2318`) does, so Detcore validates the arguments itself and still forwards nothing (the host page cache stays untouched). The order is that of `ksys_fadvise64_64()` and `generic_fadvise()` (`mm/fadvise.c`): the descriptor must be open and not `O_PATH`, because the kernel looks it up with `fdget()` (`EBADF`, F9); a pipe or FIFO gives `ESPIPE` (F10); a negative `len` gives `EINVAL` (F7); an advice value outside 0 to 5 gives `EINVAL` (F8), also on files with no page cache to steer (F11); everything else returns 0. So a pipe with advice 99 gets `ESPIPE`, not `EINVAL`. The checks need only the guest's descriptor table and the file type behind the descriptor, both container state. Today the arm at `detcore/src/lib.rs:2402` returns `Ok(0)` before reading any argument, so the change is a handler in place of that arm.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): issue the five standard hints on a temporary regular file and require each to return 0, under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/fadvise_hints.c` (cell `c-programs/fadvise-hints`, `tests/e2e/manifests/c-programs.yaml:11632`, verify ci on ptrace with `bitwise_parity: true`). Its header says it deliberately avoids invalid advice (`tests/c/fadvise_hints.c:9-11`).
- T2 (covers F6, F7): `POSIX_FADV_NOREUSE` returns 0; `len` 0 and a range starting past the end of the file return 0; a negative `len` returns `EINVAL`.
- T3 (covers F8, F9, F10): with `posix_fadvise` and with raw `syscall(SYS_fadvise64, ...)`, check `EINVAL` for advice 99, `EBADF` for a closed fd and for an `O_PATH` fd, and `ESPIPE` for the read end of a pipe. Also check the order: the read end of a pipe with advice 99 gives `ESPIPE`, and an `O_PATH` fd with advice 99 gives `EBADF`. These are Linux's answers, which the target reproduces (owner decision 6); Hermit at f4055eb9cae4 returns 0 for each, so the test fails until the emulation checks arguments.
- T4 (covers F5, F11): write a buffer to a file on the guest's tmpfs and to a regular file, issue `DONTNEED` over the dirty range, and read the bytes back unchanged; issue each valid advice on a `/proc/self/status` fd and require 0.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy emulate (Detcore returns 0 without forwarding, lib.rs:2402); linux_determinism deterministic; gap: EBADF, ESPIPE and EINVAL are replaced by 0; 11 flag behaviours and 4 tests listed; tracking_status stays todo with an open question on how to reproduce the errors
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied: target is Detcore-side validation in kernel order (EBADF incl. O_PATH, ESPIPE for a pipe or FIFO, EINVAL for negative len, EINVAL for unknown advice), still never forwarded; code today returns 0 before reading arguments (detcore/src/lib.rs:2402); T3 now asserts Linux's errnos plus an order case; target_policy stays emulate; open question on how to reproduce errors removed
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/fadvise_hints.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fadvise64`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fadvise64&body=Discussion+for+the+%60fadvise64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffadvise64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fadvise64%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `fadvise64`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`hermit-cli/tests/rr_suite.rs:250` registers `rr_fadvise`, which is ignored at this SHA (`hermit-cli/tests/fail_closed_allowed_ignores.tsv:47`). The advice values come from `linux/fadvise.h` (on s390x, `DONTNEED` and `NOREUSE` are 6 and 7; x86_64 uses 4 and 5).
