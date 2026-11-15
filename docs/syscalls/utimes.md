---
schema: syscall-pages/v1
syscall: utimes
nr_x86_64: 235
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+utimes&body=Discussion+for+the+%60utimes%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Futimes.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60utimes%60%2C+or+in+what+the+page+says%3F%0A
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

`utimes(filename, times)` sets a file's access and modification times from two `struct timeval`s, or to the current time when `times` is NULL. Detcore handles it (`detcore/src/syscalls/files.rs:3703`): it replaces the "current time" with Detcore's virtual time, converts explicit timevals to timespecs, and forwards the call to the kernel as `utimensat(AT_FDCWD, filename, times, 0)`, which goes through record/replay.

Manual reference: Linux man-pages utimes(2) and utime(2).

## Linux semantics

The kernel resolves `filename` relative to the working directory and follows symlinks. With an explicit `times`, it sets atime to `times[0]` and mtime to `times[1]`, converting microseconds to nanoseconds. With `times == NULL`, it sets both to the current host time. In both cases ctime becomes the current host time. The filesystem truncates every timestamp to its own granularity.

Permission rules differ by form. NULL is an `ATTR_TOUCH` request: the caller must own the file, have write permission on it, or hold `CAP_FOWNER`. Explicit times are an `ATTR_TIMES_SET` request: only the owner or a `CAP_FOWNER` holder may set them, and write permission is not enough.

The kernel only reads the `times` buffer. It never writes it.

Modern glibc's `utimes()` wrapper does not issue `SYS_utimes` at all: on this host (glibc 2.34), a host strace of `utimes(path, NULL)` showed `utimensat(AT_FDCWD, path, NULL, 0)`. `SYS_utimes` is reached by raw `syscall()` callers, static or older C libraries, and runtimes that issue syscalls directly.

### Flags and options

- F1: `times == NULL`: set atime and mtime to the current time. Allowed for the owner, a caller with write permission, or a `CAP_FOWNER` holder; otherwise `EACCES`.
- F2: explicit `times`: set atime to `times[0]` and mtime to `times[1]`, with `tv_usec` scaled to nanoseconds. Allowed only for the owner or a `CAP_FOWNER` holder; otherwise `EPERM`.
- F3: `tv_usec` outside `[0, 999999]` in either element: `EINVAL`, and no timestamp changes.
- F4: ctime side effect: every successful call sets ctime to the current time, whichever form was used.
- F5: path resolution: symlinks are followed; `ENOENT`, `ENOTDIR`, `EACCES` on a search component, `ELOOP`, `ENAMETOOLONG`.
- F6: bad pointers: an unreadable `filename` or `times` gives `EFAULT`.
- F7: file flags and mounts: explicit times on an append-only or immutable file give `EPERM`; NULL is allowed on an append-only file but refused on an immutable one; any change on a read-only mount gives `EROFS`.

## Determinism analysis

`linux_determinism: conditional`. With explicit times (F2), the result is a function of the arguments and the file's state: owner, mode, flags, mount. With NULL (F1), the stored atime and mtime come from the host clock, so two runs write different timestamps. The ctime side effect (F4) reads the host clock in both forms. The call repeats when the caller passes explicit times and the guest sees the same filesystem state.

### Nondeterminism sources

- The host clock, read for the NULL form (F1) and for ctime in every form (F4)
- File ownership, mode, attributes and mount flags, which decide `EACCES`, `EPERM` and `EROFS`
- Filesystem timestamp granularity, which truncates the stored values

## Hermit design

Target policy: `model`. The kernel holds the file's timestamps, and Detcore owns the one nondeterministic input it can supply, the current time. The handler at `detcore/src/syscalls/files.rs:3703` does this:

- For NULL (F1), it reads Detcore's virtual clock with `thread_observe_time` (`detcore/src/syscalls/files.rs:3712`; the function at `detcore/src/tool_global.rs:3791` returns the global time lower bound), writes `[now, now]` as two timespecs into a reserved guest stack slot, and passes that slot on.
- For explicit times (F2, F3), it reads the two timevals and writes them back over the guest's own buffer as two timespecs (`detcore/src/syscalls/files.rs:3719` to `:3730`), using reverie's `From<libc::timeval> for Timespec` conversion, which multiplies `tv_usec` by 1000.
- It then builds `utimensat(AT_FDCWD, filename, tp, 0)` (`detcore/src/syscalls/files.rs:3733`) and sends it through `handle_utimensat`, which is only `record_or_replay` (`detcore/src/syscalls/files.rs:3747`). The kernel therefore does path resolution, permission checks, `EINVAL` for an out-of-range `tv_nsec`, and the write; replay returns the recorded result.
- Classification: `utimes` is in the Determinized list (`detcore/src/syscall_classification.rs:219`); dispatch is at `detcore/src/lib.rs:2540`.

Why `model` and not the alternatives: `passthrough` would let the NULL form stamp host time into files the guest later reads back with `stat`; `emulate` would mean Detcore keeping its own timestamp table instead of the filesystem; `refuse` would break `touch`-style callers.

What remains before the code fully realizes `model`. These are readings of the code, not measurements:

- **NULL becomes explicit times, which changes the permission rule.** The kernel sees `utimensat` with a non-NULL `times`, so it applies the `ATTR_TIMES_SET` rule of F2 instead of the `ATTR_TOUCH` rule of F1. A caller with write permission that does not own the file, which Linux allows, would get `EPERM`. NULL on an append-only file would also give `EPERM` instead of success (F7). A fix has to keep the touch permission rule while still storing virtual time; how to do that is an open question below.
- **The guest's `times` buffer is overwritten.** Linux never writes it. Detcore rewrites it in place as timespecs, before the kernel has validated anything, so even a call that later fails with `EINVAL` leaves the guest's array changed. The code's own safety comment (`detcore/src/syscalls/files.rs:3724`) notes that a read-only buffer, such as a `static const struct timeval[2]`, may make the write fail, which would give `EFAULT` where Linux succeeds.
- **No range check before the multiply.** Reverie's conversion (`reverie-syscalls/src/args/time.rs:66` at the reverie pin `d766df20b780`) computes `1000 * tv_usec` with no check. Ordinary out-of-range values still reach the kernel as out-of-range nanoseconds and give `EINVAL`, as F3 requires. A huge `tv_usec` overflows the multiply: a debug build panics, and a release build wraps, which can land on a valid nanosecond value and turn Linux's `EINVAL` into success. The wrapped value is always a multiple of 8, so it can never equal `UTIME_NOW` or `UTIME_OMIT`.
- **ctime still comes from the host clock** (F4). The kernel stamps it during `utimensat`. Whether a later `stat` hides that value depends on the stat handlers, not on this one.

`linux_determinism` stays `conditional`: Hermit removes the clock dependence of F1, and the filesystem-state dependence remains as on Linux.

## Recommended tests

- T1 (covers F1, F2, F4): with a raw `syscall(SYS_utimes, ...)`, run the NULL form and then an explicit form on a file the guest owns, and `stat` after each. Under `hermit run --verify` on ptrace, both runs must print identical atime and mtime; the explicit form must store exactly the given seconds and microseconds.
  Existing candidate: `detcore/tests/lit/utimes/main.rs`, which calls `libc::utimes` with NULL (line 33) and with explicit times (line 62). It runs only through LLVM lit under Buck (`detcore/tests/lit/README.md:28`), not on ptrace in CI, and because glibc 2.34's `utimes()` issues `utimensat`, it probably never reaches `SYS_utimes`. A raw-syscall C fixture with an e2e cell is needed.
- T2 (covers F3): `tv_usec` of `-1` and `1000000` in each element must give `EINVAL` and leave the timestamps unchanged. Also call it with `tv_usec = 1L << 62` and record whether Hermit returns `EINVAL` like Linux or panics or succeeds.
- T3 (covers F2, F6): pass `times` pointing at a `static const struct timeval[2]` in read-only memory. Linux succeeds and the array is unchanged. Record Hermit's result and whether the array's bytes changed. Also check `EFAULT` for an unmapped `filename` and an unmapped `times`.
- T4 (covers F1, F2, F7): as a non-owner with write permission, the NULL form must succeed and the explicit form must give `EPERM`. Under Hermit, the code suggests the NULL form also gives `EPERM`; if so, file it as a defect against the NULL-to-explicit conversion. Where the test can set `chattr +a`, NULL must succeed on an append-only file and explicit times must give `EPERM`.
- T5 (covers F5): `ENOENT` for a missing path, `ENOTDIR` for a file used as a directory component, and symlink following: the target's times change, not the link's.
- T6 (covers F1, F2): record a guest that runs both forms and `stat`s the file, then replay it; replay must reproduce the stat output.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (NULL times become Detcore virtual time; the call is forwarded as utimensat through record_or_replay); linux_determinism stays conditional; 7 flag behaviours and 6 tests listed; gaps found in the code: NULL is sent as explicit times (stricter permission rule), the guest's timeval buffer is overwritten in place, and tv_usec is not range-checked before the multiply; no coverage token because no test has been run against this list, so tracking_status stays todo; one open question recorded

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `utimes`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+utimes&body=Discussion+for+the+%60utimes%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Futimes.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60utimes%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `utimes`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Should the NULL form keep Linux's touch permission rule (F1)? The current conversion to explicit times makes it stricter. Fixing it needs a design choice in `handle_utimes`; this body only records the gap.
