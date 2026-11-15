---
schema: syscall-pages/v1
syscall: utime
nr_x86_64: 132
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+utime&body=Discussion+for+the+%60utime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Futime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60utime%60%2C+or+in+what+the+page+says%3F%0A
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

`utime(filename, times)` sets a file's access and modification times, in whole seconds, or to "now" when `times` is NULL. Detcore handles it (`handle_utime`, `detcore/src/syscalls/files.rs:3661`) by turning it into `utimensat(AT_FDCWD, filename, ts, 0)` with an explicit timestamp pair: the caller's seconds, or Hermit's virtual time when `times` is NULL. That replaces the host clock, but it also changes which permission check the kernel applies to the NULL case.

Manual reference: Linux man-pages utime(2).

## Linux semantics

`times` points to a `struct utimbuf { time_t actime; time_t modtime; }`. The kernel reads both fields before it resolves `filename`, so a bad `times` pointer gives `EFAULT` even for a missing file. It then resolves `filename`, following symbolic links, and updates the inode:

- With `times == NULL`, both timestamps become the current time from the host's real-time clock. The caller must own the file, have write permission on it, or hold `CAP_FOWNER` or `CAP_DAC_OVERRIDE`; otherwise the call fails with `EACCES`.
- With explicit times, the timestamps become `actime` and `modtime` with zero nanoseconds. Write permission is not enough: the caller must own the file or hold `CAP_FOWNER`; otherwise the call fails with `EPERM`.
- On success the inode's change time (ctime) is always set to the current host time.
- The filesystem may truncate the stored values to its timestamp granularity and clamp them to its representable range.

An immutable file refuses every timestamp change with `EPERM`. An append-only file accepts only "set both to now", so it accepts `times == NULL` and refuses explicit times with `EPERM`. A file on a read-only mount gives `EROFS`.

The other entry points to the same kernel operation are `utimes` (microseconds), `futimesat` and `utimensat` (nanoseconds, `UTIME_NOW`, `UTIME_OMIT`, `AT_SYMLINK_NOFOLLOW`).

### Flags and options

- F1: `times == NULL`: set atime and mtime to the current time. Allowed for the owner, for a caller with write permission, or with `CAP_FOWNER`/`CAP_DAC_OVERRIDE`; otherwise `EACCES`.
- F2: `times != NULL`: set atime to `actime` and mtime to `modtime`, each with zero nanoseconds. Allowed only for the owner or with `CAP_FOWNER`; otherwise `EPERM`.
- F3: on success, ctime is set to the current time. Stored values are truncated to the filesystem's granularity and clamped to its range.
- F4: `filename` is resolved following symbolic links, including a trailing one. Resolution errors: `ENOENT`, `ENOTDIR`, `EACCES` (search permission denied on a prefix directory), `ELOOP`, `ENAMETOOLONG`.
- F5: immutable file: `EPERM` for every request. Append-only file: `EPERM` for explicit times; `times == NULL` succeeds.
- F6: file on a read-only filesystem: `EROFS`
- F7: `times` not readable: `EFAULT`, checked before `filename` is resolved. `filename` not readable: `EFAULT`.

## Determinism analysis

`linux_determinism: conditional`, kept from the rendered body. With explicit times (F2) the return value and the stored atime and mtime are a function of the arguments and of the file's ownership and mode, which a guest controls inside its own container. Two inputs come from the host clock: the timestamps written for `times == NULL` (F1), and the ctime written on every success (F3). Whether either is observable depends on what later reads the file's metadata. The call also depends on the file's owner and mode, and on mount flags (F5, F6), which are host inputs for files the guest did not create.

The call repeats when `times` is non-NULL and nothing reads ctime back, or when a layer between the guest and the kernel substitutes a deterministic clock for "now" and a deterministic ctime for `stat`.

### Nondeterminism sources

- The host real-time clock, read for `times == NULL` and for ctime on every success
- Filesystem timestamp granularity and range, which differ between host filesystems
- Ownership, mode, attribute flags and mount flags of host files the guest can see

### Cross-host variation

- Host inode flags and mount flags on files the guest did not create: an immutable or append-only file gives `EPERM` (F5) and a read-only mount gives `EROFS` (F6). The guest's root in a user namespace cannot set or clear these flags, so the same guest gets success on a host whose files and mounts lack them.

## Hermit design

Target policy: `model`. Detcore keeps the kernel as the store of file timestamps but chooses every timestamp value itself. The code at hermit f4055eb9cae4:

- The typed arm `Syscall::Utime(s) => self.handle_utime(guest, s).await.map_err(Into::into)` (`detcore/src/lib.rs:2539`) reaches `handle_utime` (`detcore/src/syscalls/files.rs:3661`).
- The handler reserves a two-element `Timespec` array on the guest's scratch stack (`detcore/src/syscalls/files.rs:3666`).
  - For `times == NULL` (F1) it fills both entries with `thread_observe_time` (`detcore/src/syscalls/files.rs:3670`), which is Hermit's virtual time (`detcore/src/tool_global.rs:3791`). The doc comment in `detcore/src/syscall_time.rs:206` lists `utime` without times among the calls that stamp a file with the virtual time.
  - For explicit times (F2) it reads the `utimbuf` from guest memory (`detcore/src/syscalls/files.rs:3677`) and copies `actime` and `modtime` with `tv_nsec: 0` (`detcore/src/syscalls/files.rs:3678`). A bad `times` pointer fails this read before any path is resolved, the same order as Linux (F7).
- It then builds `utimensat(AT_FDCWD, filename, ts, 0)` (`detcore/src/syscalls/files.rs:3694`) and hands it to `handle_utimensat`, which runs it through `record_or_replay` (`detcore/src/syscalls/files.rs:3747`). Path resolution, symbolic-link following, the filesystem's truncation and clamping, immutable and append-only checks and `EROFS` (F3 to F7) are the kernel's.
- ctime (F3) is still written by the kernel from the host clock. It does not reach the guest under the default configuration: with `virtualize_metadata` on (it is cleared by `--no-virtualize-metadata`, `detcore-model/src/config.rs:253`), `determinize_stat` reports atime and ctime as the configured epoch (`detcore/src/syscalls/files.rs:3021`) and mtime from Detcore's per-inode logical time (`detcore/src/syscalls/files.rs:3003`, `detcore/src/syscalls/files.rs:3025`).

A consequence of that `stat` policy: under the default configuration, the times `utime` writes are not what the guest reads back. `tests/c/utimensat_determinism.c:58` asserts exactly this for `utimensat`, the call `utime` becomes. That is the stat family's choice and is tracked there.

What remains before the code fully realizes `model`:

- **The NULL case takes the explicit-times permission check.** Linux allows `utime(f, NULL)` for any caller with write permission (F1). Detcore always passes an explicit timestamp pair, so the kernel applies the owner-or-`CAP_FOWNER` rule of F2. By this reading of the code, three cases differ from Linux: a caller with write permission who does not own the file gets `EPERM` where Linux succeeds; a caller with neither gets `EPERM` where Linux gives `EACCES`; and an append-only file refuses `times == NULL` with `EPERM` where Linux succeeds (F5). This is a hypothesis from the code, not a measurement; T3 and T5 confirm or refute it. See Open questions.
- `utime` has no test of its own. The closest existing test exercises `utimensat` directly.

Target (owner decision 13): `times == NULL` takes virtual realtime, as defined in the `clock_gettime` body: the same reading `clock_gettime(CLOCK_REALTIME)` returns, from `guest_clock_time` (`detcore/src/syscalls/time.rs:95`). Today the handler uses `thread_observe_time` (`detcore/src/syscalls/files.rs:3670`), the raw global reading. `guest_clock_time` passes that reading through the per-thread clock, which never goes backwards (`observe_guest_clock`, `detcore/src/tool_local.rs:2054`; `GuestClock::observe`, `detcore/src/tool_local.rs:1700`). By this reading of the code, a stamp taken today could be earlier than a `CLOCK_REALTIME` value the thread has already read; that was not measured. Explicit times are not a clock and do not change. The permission question below is separate, and decision 13 does not answer it.

Why `model` and not the alternatives:

- `passthrough` would write the host clock for `times == NULL`, readable through `stat` whenever metadata virtualization is off.
- `emulate`, without the kernel, would have to reproduce path resolution, permissions, attribute flags and read-only mounts; and the timestamps must really be stored for tools such as `make` that run outside the guest afterwards.
- `refuse` would break `touch` from older C libraries, `tar`, `cp -p` and other programs that preserve times.

## Recommended tests

- T1 (covers F1, F3): on a file the guest owns, `utime(path, NULL)` returns 0 under `hermit run --verify` on ptrace, and `stat` gives the same atime, mtime and ctime in both runs. With `--no-virtualize-metadata`, atime equals mtime and both equal a virtual time, not the host clock.
- T2 (covers F2, F3): `utime(path, &(struct utimbuf){1111111111, 2222222222})` returns 0. With `--no-virtualize-metadata`, `stat` reports those seconds with zero nanoseconds. Under the default configuration, record what `stat` reports, as `tests/c/utimensat_determinism.c` does for `utimensat`.
  Related existing test: `tests/c/utimensat_determinism.c` (cell `c-programs/utimensat-determinism`, `tests/e2e/manifests/c-programs.yaml:14286`, verify ci on ptrace). It calls `utimensat`, which shares the downstream path from `detcore/src/syscalls/files.rs:3742`, but never `utime`.
- T3 (covers F1, F2): permission matrix, run as a guest user who does not own the files. On a mode 0666 file, `utime(f, NULL)` must succeed and explicit times must give `EPERM`. On a mode 0644 file, `utime(f, NULL)` must give `EACCES`. Record what Hermit returns; if it returns `EPERM` for both NULL cases, as the code suggests, file it as a compatibility defect.
- T4 (covers F4): `utime` on a symbolic link updates the target, not the link (check with `--no-virtualize-metadata` and `lstat`). `ENOENT` for a missing file, `ENOTDIR` for `/etc/passwd/x`, `ELOOP` for a self-referencing link, `ENAMETOOLONG` for a 5000-byte path.
- T5 (covers none): where the CI host grants `CAP_LINUX_IMMUTABLE` to the guest, an immutable file refuses both forms with `EPERM`, and an append-only file accepts `times == NULL` and refuses explicit times. A file on a read-only mount the guest can see gives `EROFS`. Record any case the host cannot set up as untested. These errors come from host inode and mount flags (host-config, U1), so this test documents them but does not cover F5 or F6.
- T6 (covers F7): `times = (void *)1` gives `EFAULT`, also for a path that does not exist, which shows the read happens first. `filename = NULL` gives `EFAULT`.
- U1 (covers F5, F6; host-config): host inode flags and mount flags decide whether the immutable or append-only `EPERM` (F5) and `EROFS` (F6) occur, so CI cannot produce them on every host. Detcore rewrites the call as `utimensat` and forwards the kernel's answer unchanged (`detcore/src/syscalls/files.rs:3747`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (Detcore rewrites utime to utimensat with explicit seconds or virtual time; the kernel stores them); linux_determinism stays conditional (host clock only for NULL times and ctime); 7 flag behaviours and 6 tests; open question on the NULL case taking the owner-only permission check; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host inode flags and read-only mounts); U1 host-config covers F5 and F6, T5 now covers none; no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 (times == NULL takes virtual realtime through guest_clock_time, the reading clock_gettime(CLOCK_REALTIME) returns, instead of the raw thread_observe_time without the per-thread non-decreasing clamp); target_policy stays model; no new F/T ids; no questions removed (the NULL permission question is not decided)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `utime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+utime&body=Discussion+for+the+%60utime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Futime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60utime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `utime`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`utimes` uses the same design (`handle_utimes`, `detcore/src/syscalls/files.rs:3703`): a NULL `times` becomes the virtual time and is passed to `utimensat` explicitly, so the permission question below applies to it too.

### Open questions

- How should `times == NULL` be passed to the kernel? Today Detcore passes the virtual time as explicit values (`detcore/src/syscalls/files.rs:3670`), which changes the permission rule from "owner or writer" to "owner only", and makes append-only files refuse the call. Two possible directions, each with a cost: check write permission and ownership in Detcore and return the Linux errno, which duplicates kernel policy; or pass `UTIME_NOW` and let the kernel apply the right rule, which stores the host clock in the file and relies on `stat` virtualization to hide it. This is a design decision for the owner, so the body leaves it open.
