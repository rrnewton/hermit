---
schema: syscall-pages/v1
syscall: futimesat
nr_x86_64: 261
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futimesat&body=Discussion+for+the+%60futimesat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutimesat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futimesat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`futimesat(dirfd, pathname, times)` sets a file's access and modification times, with microsecond `struct timeval` values, for a path relative to a directory descriptor. It is an obsolete Solaris-derived call that `utimensat` replaced. Detcore refuses it: the dispatch returns `ENOSYS` without calling the kernel (`detcore/src/lib.rs:2545` at hermit f4055eb9cae4).

Manual reference: Linux man-pages futimesat(2).

## Linux semantics

The kernel resolves `pathname` relative to `dirfd` (or the current directory for `AT_FDCWD`, or the root for an absolute path), always following a final symlink, and sets the file's atime and mtime. `times` is NULL, meaning "now", or points to two `struct timeval` values, atime then mtime. The kernel checks each `tv_usec` against `[0, 999999]` before converting to nanoseconds. Every successful call also sets ctime to the current time. There is no flags argument.

With a NULL `pathname`, Linux operates on the file `dirfd` itself refers to. This is a Linux extension; measured on the host (Linux 7.1.3): `futimesat(file_fd, NULL, times)` returned 0, and `futimesat(AT_FDCWD, NULL, NULL)` gave `EFAULT`.

### Flags and options

- F1: `times == NULL`: set both times to the current time. Allowed for the file owner, a caller with write permission, or a caller with `CAP_FOWNER` or `CAP_DAC_OVERRIDE`.
- F2: explicit `times`: set atime and mtime to the given seconds and microseconds. Allowed only for the file owner or a caller with `CAP_FOWNER`, otherwise `EPERM`.
- F3: `tv_usec` validation: a value below 0 or above 999999 in either entry gives `EINVAL` (host probe: `tv_usec = 1000000` gave `EINVAL`).
- F4: `dirfd` and `pathname`: `AT_FDCWD` resolves a relative path through the current directory; a directory descriptor resolves it from that directory; an absolute path ignores `dirfd`. A relative path with a regular-file `dirfd` gives `ENOTDIR` (host probe confirmed), and with a closed one `EBADF`.
- F5: NULL `pathname`: operate on the object `dirfd` names; with `AT_FDCWD` this gives `EFAULT`.
- F6: symlinks: a final symlink is always followed; there is no `AT_SYMLINK_NOFOLLOW` form.
- F7: side effect: ctime is set to the current time on every successful call.
- F8: errors: `ENOENT` (missing path; host probe confirmed), `EACCES` (search permission, or no write permission with NULL `times`), `EPERM` (explicit times on a file the caller does not own, or an immutable or append-only file), `EROFS`, `ELOOP`, `ENAMETOOLONG`, `ENOTDIR`, `EBADF`, `EFAULT` (bad `pathname` or `times` pointer).

## Determinism analysis

`linux_determinism: conditional`. With explicit `times` (F2), the result and the stored atime and mtime depend only on the arguments, permissions and filesystem state, so the call repeats given the same files. With NULL `times` (F1), the stored times come from the host clock, and on every successful call ctime does too (F7), so a later `stat` shows different values each run. Errors depend on which paths exist and on the caller's credentials (F8).

## Hermit design

Target policy: `refuse`. The dispatch answers every call with `ENOSYS` before the kernel sees it: `Syscall::Futimesat(_s) => Err(Error::Errno(Errno::ENOSYS))` at `detcore/src/lib.rs:2545`, with the comment at `detcore/src/lib.rs:2543-2544` that `futimesat` is obsolete and refused "for simplicity". The classification agrees: `futimesat` is in the Determinized list (`detcore/src/syscall_classification.rs:108`) and in the deterministic-refusal aggregate `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1316`, member at `detcore/src/syscall_classification.rs:1340`), which the unit test `deterministic_refusal_aggregate_includes_fixed_error_families` checks (`detcore/src/syscall_classification.rs:2710`, member at `detcore/src/syscall_classification.rs:2734`). The syscall is subscribed (`detcore/src/lib.rs:1168`), so the refusal also applies under the explicit performance opt-in.

Why `refuse` is acceptable today: `ENOSYS` is the answer a kernel without the call would give, it is identical in every run, and callers of an obsolete interface are expected to fall back to `utimensat` or `utimes`, which Detcore handles (`detcore/src/lib.rs:2539-2542`). The refusal removes the host-clock leak of F1 and F7 entirely. No file's times change, so nothing nondeterministic reaches later `stat` calls.

Why not the alternatives:

- `passthrough` would store host time for F1 and F7, which the stat family then has to hide.
- `model`, by converting the call to `utimensat(dirfd, pathname, times_as_timespec, 0)` as `handle_utimes` already does for `utimes`, would give programs the real behaviour. It needs a separate path for NULL `pathname` (F5), which `utimensat` spells differently. Whether that is worth doing for an obsolete call is an owner decision, recorded below.

Every flag behaviour F1 to F8 is answered today by the same `ENOSYS`. That realizes `refuse`, but a guest that does not fall back fails where Linux succeeds. No test at f4055eb9cae4 issues `futimesat`.

## Recommended tests

- T1 (covers F1, F2, F3): raw `syscall(SYS_futimesat, AT_FDCWD, path, NULL)`, with explicit valid times, and with `tv_usec = 1000000`; under `hermit run --verify` on ptrace each must give `-1` with `ENOSYS`, the file's `stat` times must be unchanged, and output must be identical across the two runs.
- T2 (covers F4, F5, F6): call through a directory descriptor, with a regular-file `dirfd`, with a NULL `pathname` on a file descriptor, and on a symlink; each must give `ENOSYS` and leave the target's and the link's times unchanged.
- T3 (covers F7, F8): call on a missing path, on a read-only file the guest does not own, and with a `times` pointer of `(void *)1`; each must give `ENOSYS` (not `ENOENT`, `EPERM` or `EFAULT`), and the file's ctime, read with `stat`, must not change.
- T4 (covers F1, F2): a fallback check: a program that calls `futimesat`, sees `ENOSYS`, then calls `utimensat` with the same times, must end with the same file times in both verify runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (dispatch returns ENOSYS before the kernel; in the deterministic-refusal aggregate); linux_determinism stays conditional (NULL times and ctime come from the host clock); 8 flag behaviours and 4 tests listed; no test issues futimesat, so tracking_status stays todo; open question recorded: emulate through utimensat instead of refusing

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `futimesat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+futimesat&body=Discussion+for+the+%60futimesat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffutimesat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60futimesat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `futimesat`: the 2026-07-24 classification campaign filed issues only for syscalls that Detcore did not yet handle, and Detcore already handled this one.

Host probes (2026-10-03, Linux 7.1.3, a Python `ctypes` harness under `a scratch directory`): `futimesat(dirfd, "file", times)` = 0 and set atime and mtime; `futimesat(file_fd, NULL, times)` = 0; `tv_usec = 1000000` gave `EINVAL`; `futimesat(AT_FDCWD, NULL, NULL)` gave `EFAULT`; NULL `times` set the current time; `futimesat(file_fd, "x", NULL)` gave `ENOTDIR`; `futimesat(AT_FDCWD, "/nonexistent-x", NULL)` gave `ENOENT`.

### Open questions

- Should Detcore keep refusing `futimesat` with `ENOSYS`, or convert it to `utimensat` the way `handle_utimes` converts `utimes`, so programs that do not fall back still work? Converting means handling the NULL `pathname` form and choosing virtual time for NULL `times`.
