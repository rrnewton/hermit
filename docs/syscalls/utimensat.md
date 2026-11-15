---
schema: syscall-pages/v1
syscall: utimensat
nr_x86_64: 280
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+utimensat&body=Discussion+for+the+%60utimensat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Futimensat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60utimensat%60%2C+or+in+what+the+page+says%3F%0A
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

`utimensat(dirfd, pathname, times, flags)` sets a file's access and modification times with nanosecond precision. It can set either time to an explicit value, to "now" (`UTIME_NOW`), or leave it unchanged (`UTIME_OMIT`). glibc's `futimens(fd, times)` is this syscall with a NULL path.

Detcore lets the kernel perform the change and records the return value (`detcore/src/syscalls/files.rs:3742-3748`). The guest's view of file times comes from `determinize_stat`, which ignores what `utimensat` stored. `utime` and `utimes` substitute virtual time before forwarding here. A direct `utimensat` with "now" does not.

Manual reference: Linux man-pages utimensat(2), inode(7).

## Linux semantics

`times` is NULL or an array of two `timespec` values, `[0]` for atime and `[1]` for mtime. Each entry is an explicit time, `UTIME_NOW` or `UTIME_OMIT`, the last two given in `tv_nsec`. The kernel resolves the target, checks permission, and sets the requested times, truncated to the filesystem's granularity. Unless both entries are `UTIME_OMIT`, it also sets ctime to the current time. "Now" is the kernel's current coarse realtime clock.

Permission depends on what is set:

- Setting both times to now needs ownership, write access, or `CAP_FOWNER`.
- Any explicit value needs ownership or `CAP_FOWNER`.

### Flags and options

- F1: `times == NULL`: set both atime and mtime to the current time.
- F2: explicit `times[0]` and `times[1]`: set the given atime and mtime. Each `tv_nsec` must be in `[0, 1e9)`, or be one of the two special values (`EINVAL` otherwise).
- F3: `UTIME_NOW` in either `tv_nsec`: set that timestamp to the current time; its `tv_sec` is ignored.
- F4: `UTIME_OMIT` in either `tv_nsec`: leave that timestamp unchanged. If both are `UTIME_OMIT`, the call returns 0 without resolving the path, and ctime is not changed.
- F5: `AT_SYMLINK_NOFOLLOW`: change a symbolic link itself rather than its target.
- F6: `AT_EMPTY_PATH`: with an empty `pathname`, act on `dirfd` itself. Any flag bit other than F5 and F6 gives `EINVAL`.
- F7: `pathname == NULL` (the Linux extension behind `futimens`): act on the file `dirfd` refers to. `flags` must be 0 (`EINVAL` otherwise), and `dirfd == AT_FDCWD` gives `EFAULT`.
- F8: `dirfd`: a relative `pathname` is resolved against `dirfd`, or against the cwd for `AT_FDCWD`. A bad descriptor gives `EBADF`, and a descriptor that is not a directory gives `ENOTDIR`.
- F9: permission: explicit times without ownership or `CAP_FOWNER` give `EPERM`. "Now" without ownership or write access gives `EACCES`. Immutable, append-only and read-only cases are F12.
- F10: side effects: ctime becomes the current time whenever anything is set, and stored times are truncated to the filesystem's granularity.
- F11: path and pointer errors: `ENOENT`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG` and `EACCES` (search permission) come from path resolution. An unreadable `times` or `pathname` gives `EFAULT`.
- F12: host-config failures: an immutable or append-only file gives `EPERM`, and a read-only filesystem gives `EROFS`.

## Determinism analysis

`linux_determinism: conditional` (kept from the migration, now justified).

- An explicit time stores exactly the requested value, rounded to the filesystem's granularity, and the return value depends only on file state and credentials. That part repeats.
- F1 and F3 store the host's realtime clock. F10 sets ctime from the same clock on every successful call.
- `stat` after the call therefore reads host time unless something virtualizes it.

### Nondeterminism sources

- The current time stored by `times == NULL`, `UTIME_NOW`, and the unconditional ctime update.
- Filesystem granularity, which depends on the host filesystem, affects what is stored for an explicit nanosecond value.

### Cross-host variation

- Host inode flags and mount flags on files the guest did not create: an immutable or append-only file gives `EPERM` and a read-only mount gives `EROFS` (F12). The guest's root in a user namespace cannot set or clear these flags, so the same guest gets success on a host whose files and mounts lack them.

## Hermit design

Target policy: `model`. The kernel validates arguments, resolves the path and checks permission. Detcore owns the times the guest can observe. The code at f4055eb9cae4:

- `handle_utimensat` (`detcore/src/syscalls/files.rs:3742-3748`, dispatched at `detcore/src/lib.rs:2542`) is only `record_or_replay`. In run mode it injects the call unchanged. In record/replay it records the return value.
- `handle_utime` and `handle_utimes` (`detcore/src/syscalls/files.rs:3661-3700`, `detcore/src/syscalls/files.rs:3703-3739`) replace a NULL `times` with the thread's virtual time from `thread_observe_time` (`detcore/src/syscalls/files.rs:3670-3674`, `detcore/src/syscalls/files.rs:3710-3717`). They then forward to `handle_utimensat` (`detcore/src/syscalls/files.rs:3699`, `detcore/src/syscalls/files.rs:3738`).
- With `virtualize_metadata` on (the default; it is turned off with `--no-virtualize-metadata`, `detcore-model/src/config.rs:253`), `stat`/`fstat`/`statx` results go through `determinize_stat` (`detcore/src/syscalls/files.rs:2981-3028`; gates at `detcore/src/syscalls/files.rs:3036` and `detcore/src/syscalls/files.rs:3069`).
  - atime, ctime and btime become the configured epoch (`detcore/src/syscalls/files.rs:3021-3023`).
  - mtime becomes the per-inode logical time (`detcore/src/syscalls/files.rs:3025`). It starts at the configured epoch when Detcore first sees the inode (`detcore/src/tool_global.rs:2524-2535`) and advances only through `touch_file` (`detcore/src/tool_global.rs:3763`), which data writes call (for example `detcore/src/syscalls/files.rs:2428-2431`).
  - `utimensat` never calls `touch_file`.

Why `model` and not the alternatives:

- `passthrough` leaks host time.
- `emulate` would have to reimplement path resolution and permission checks that the kernel already does correctly.
- `refuse` would break `touch`, `cp -p`, `tar`, `rsync` and `make`.

What remains before the code fully realizes `model`:

- **Explicit times are invisible.** Under virtualized metadata, a guest that sets mtime to a fixed value reads back the logical mtime, not its request. `tests/c/utimensat_determinism.c` pins this override as intended. `tests/c/file_timestamp_identity.c:173-175` states the opposite contract ("utimensat with a fixed value must land exactly"). See Open questions.
- **`UTIME_NOW` and NULL `times` do not use virtual time.** A direct call stores the host clock. Under virtualized metadata `stat` hides it, but the call does not advance the logical mtime either.
  - Hypothesis: `touch file` therefore does not make `file` newer to the guest, so `make` rules that compare timestamps may decide differently than on Linux. Not measured.
  - The fix that matches `handle_utime` would substitute `thread_observe_time` for NULL and `UTIME_NOW`, and call `touch_file` when mtime is set.
- **`--no-virtualize-metadata`.** With virtualized metadata off, F1, F3 and the F10 ctime update leak the host clock straight to `stat`.
- **Record/replay.** Only the return value is recorded. The file system state the call changed is not part of the recording, which matches other path syscalls.

Target (owner decision 13): "now" is virtual realtime. The clock names are those defined in the `clock_gettime` body, and `guest_clock_time` (`detcore/src/syscalls/time.rs:95`) is the reading `clock_gettime(CLOCK_REALTIME)` returns.

- A NULL `times` (F1), and each `UTIME_NOW` entry (F3), are replaced with that reading before the call reaches the kernel, as `handle_utime` and `handle_utimes` already do for NULL (`detcore/src/syscalls/files.rs:3670`, `detcore/src/syscalls/files.rs:3710`).
- An entry that is `UTIME_OMIT`, or an explicit value, is passed through unchanged.
- Today `handle_utimensat` forwards both forms unchanged (`detcore/src/syscalls/files.rs:3747`), so the kernel stores the host clock.
- The ctime update (F10) is the kernel's own write and still reads the host clock. Under virtualized metadata `determinize_stat` hides it (`detcore/src/syscalls/files.rs:3021`).
- Replacing NULL with explicit values changes the kernel's permission rule from "owner or writer" to "owner only". That is the same problem the `utime` body raises as an open question, and the substitution must not land without an answer to it.
- Whether the stored virtual time becomes visible through `stat`, and whether it advances the logical mtime through `touch_file`, follows the open question on explicit times below. Decision 13 fixes only the clock source.

## Recommended tests

- T1 (covers F2, F4): explicit atime and mtime `{1111111111, 2222222222}` are accepted, then a `UTIME_OMIT` atime with a new mtime is accepted. Under Hermit, atime and mtime read back collapsed and overridden. Existing candidate: `tests/c/utimensat_determinism.c`, cell `c-programs/utimensat-determinism` (`tests/e2e/manifests/c-programs.yaml:14286`), verify ci on ptrace, kvm and liteinst. If the open question is resolved toward exact explicit times, change its expectation.
- T2 (covers F2, F3, F10): files that the guest writes get ordered, reproducible mtimes. Then `utimensat` with fixed values (`tests/c/file_timestamp_identity.c:180`) and with `UTIME_NOW` (`tests/c/file_timestamp_identity.c:191`), with the stat output printed and compared across `--verify` runs. Existing candidate: `tests/c/file_timestamp_identity.c`, cell `system-utils/file-timestamp-identity` (`tests/e2e/manifests/system-utils.yaml:1456`), verify ci on ptrace and kvm, with naked native as the negative control.
- T3 (covers F1, F3): write file A, then `utimensat(B, NULL)` and `utimensat(B, {UTIME_NOW, UTIME_NOW})`. Assert that B's mtime is not older than A's, and that the value is identical across `--verify` runs. This is the `make` ordering contract. No existing test.
- T4 (covers F4): both entries `UTIME_OMIT` on a path that does not exist returns 0. No existing test.
- T5 (covers F2, F6, F11): the error cases:
  - `tv_nsec = 1e9` gives `EINVAL`
  - an unknown flag bit gives `EINVAL`
  - a missing path gives `ENOENT`
  - `times` pointer 1 gives `EFAULT`
  No existing test.
- T6 (covers F5, F6, F7, F8): target selection:
  - `AT_SYMLINK_NOFOLLOW` changes the link and leaves its target
  - `AT_EMPTY_PATH` and `futimens` act on an open descriptor
  - a NULL path with `AT_FDCWD` gives `EFAULT`
  - a relative path against a directory fd works, and a non-directory fd gives `ENOTDIR`
  No existing test.
- T7 (covers F9): an unprivileged guest on a file it does not own:
  - explicit times give `EPERM`
  - `UTIME_NOW` without write access gives `EACCES`
  - a read-only bind mount, where the guest can see one, gives `EROFS`; this documents F12 (host-config, U1) and does not cover it
  No existing test.
- T8 (covers F1, F3, F10): under `--no-virtualize-metadata`, measure whether `stat` after `utimensat(NULL)` differs between two runs. This documents the leak until the code substitutes virtual time. No existing test.
- T9 (covers F2, F11): record and replay of a successful call and of an `ENOENT` call reproduce the return values. No existing test.
- U1 (covers F12; host-config): host inode flags and mount flags decide whether the immutable or append-only `EPERM` and `EROFS` occur, so CI cannot produce them on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscalls/files.rs:3747`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel validates and stores; guest-visible times come from determinize_stat); linux_determinism stays conditional (explicit times repeat; UTIME_NOW, NULL times and ctime read the host clock); 11 flag behaviours and 9 tests listed; open question for the owner on whether explicit times must be visible to the guest, because two existing tests state opposite contracts; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host inode flags and read-only mounts); split immutable, append-only and read-only errors out of F9 into new F12 with U1 host-config; T7 EROFS case marked as documenting F12; no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 (NULL times and UTIME_NOW take virtual realtime before forwarding, instead of the host clock stored today; visibility through stat stays with the explicit-times question); target_policy stays model; no new F/T ids; removed the open question on whether NULL and UTIME_NOW take virtual time
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/utimensat_determinism.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `utimensat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+utimensat&body=Discussion+for+the+%60utimensat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Futimensat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60utimensat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `utimensat`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The header of `tests/c/file_timestamp_identity.c` (lines 12-17) says `c-programs/utimensat-determinism` is ci=false in every mode. That is stale at f4055eb9cae4: the cell runs verify in CI on ptrace, kvm and liteinst, and it is listed in `tests/e2e/parity-selection.yaml:230`.

### Open questions

- Must a guest see the explicit times it sets with `utimensat`? `tests/c/utimensat_determinism.c` requires the override, with requested times invisible and atime collapsed to mtime. `tests/c/file_timestamp_identity.c:173-175` says a fixed value "must land exactly". Exact explicit times are deterministic on Linux, so honoring them is safe. Tools such as `tar -x`, `cp -p`, `rsync -t` and `make` depend on them, but the virtual-metadata design deliberately hides stored times. This is a policy decision for the owner, and the T1 expectation follows from it.
