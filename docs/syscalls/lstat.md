---
schema: syscall-pages/v1
syscall: lstat
nr_x86_64: 6
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lstat&body=Discussion+for+the+%60lstat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flstat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lstat%60%2C+or+in+what+the+page+says%3F%0A
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

`lstat(path, statbuf)` is `stat` without following a symbolic link in the last path component: for a symlink it describes the link itself. Detcore handles it in `handle_stat_family` (`detcore/src/syscalls/files.rs:3031`), the same handler as `stat`, `fstat` and `newfstatat`: with `virtualize_metadata` on (the `hermit run` default) the kernel runs the call and Detcore rewrites the identity and time fields; in record/replay the result is recorded and replayed.

Manual reference: Linux man-pages stat(2).

## Linux semantics

`lstat` resolves `path` relative to the current directory (or from `/` when absolute), following every symlink except the final component. It cannot block on its own, although resolution through a slow or FUSE filesystem can stall. It has no side effects.

For a symlink, `st_mode` is `S_IFLNK` and `st_size` is the length of the link target. For anything else the result is the same as `stat`. As with `fstat`, `st_ino`, `st_dev` and the timestamps are host-assigned, and `st_size`, `st_blocks`, `st_blksize`, `st_nlink`, `st_uid`, `st_gid`, `st_mode` and `st_rdev` reflect the host filesystem.

Errors come from path resolution: `ENOENT`, `ENOTDIR`, `ELOOP` (too many links in the intermediate components), `ENAMETOOLONG`, `EACCES` (search permission denied on a directory), and `EFAULT` for an unreadable `path` or an unwritable `statbuf`. `EOVERFLOW` exists only for 32-bit callers.

### Flags and options

- F1: final component is a symbolic link: the link is described (`S_IFLNK`, `st_size` = target length) and not followed, including dangling links and `/proc` magic links such as `/proc/self/exe`
- F2: final component is not a symlink: same result as `stat`, including symlinks in intermediate components, which are followed
- F3: relative path: resolved against the calling thread's current directory
- F4: path resolution errors: `ENOENT`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG`, `EACCES`
- F5: `path` or `statbuf` not accessible: `EFAULT`
- F6: procfs, sysfs and device paths: host-assigned `st_dev`, `st_ino` and, for devices, `st_rdev`

## Determinism analysis

`linux_determinism: conditional`. For the same path in the same filesystem state, `lstat` returns the same bytes except for the host-assigned fields: inode and device numbers from global allocators and timestamps from the real clock. Those differ between runs, so the call repeats only for paths whose inodes are stable on the host and whose timestamps are not observed. Whether a path exists at all, and what it points to, is filesystem state outside the program.

### Nondeterminism sources

- `st_ino` and `st_dev` from host-wide allocators, notably for procfs, sysfs and tmpfs (the `TODO-HUMAN-REVIEW(PR-1056)` comment at `detcore/src/syscalls/files.rs:3008` records `st_dev` drift between the two runs of `--verify`)
- `st_atime`, `st_mtime` and `st_ctime` from the real clock
- Host filesystem content and layout: which paths exist, symlink targets, `st_blocks`, `st_nlink`, `st_uid`, `st_gid`
- Other processes that create, rename or delete the path concurrently

### Cross-host variation

- Filesystem type and block size behind the file: `st_blksize` and `st_blocks`, and a directory's `st_size` and `st_nlink`, are the host filesystem's values, which Detcore passes through (`detcore/src/stat.rs:46`); btrfs, ext4 and tmpfs report different values for the same guest-created file or directory (F2).
- Host uid that launches Hermit: the user namespace maps that one uid to the guest's root, so the `st_uid` and `st_gid` of host files the guest can see (passed through raw) differ between hosts (F1, F2, F6).

## Hermit design

Target policy: `model`, the same as `stat` and `fstat`. The kernel resolves the path, follows or does not follow the link, and produces every error; Detcore replaces the host-assigned identity and time fields. At hermit f4055eb9cae4:

- The dispatch arm is `detcore/src/lib.rs:2411`, which calls `handle_stat_family` (`detcore/src/syscalls/files.rs:3031`).
- With `virtualize_metadata`, the call is injected first (`detcore/src/syscalls/files.rs:3041`), so every resolution error (F4, F5) and the symlink rule (F1) are the kernel's own. The NB comment there explains that resolving paths in the tracer can hang on some FUSE filesystems.
- `determinize_stat` (`detcore/src/syscalls/files.rs:2981`) then sets `st_ino` from `determinize_inode` (`detcore/src/tool_global.rs:3682`), `st_dev` from `determinize_device` (`detcore/src/syscalls/files.rs:3013`), `st_atime` and `st_ctime` to the configured epoch, and `st_mtime` to Detcore's logical mtime, which starts at the epoch when the inode is first seen (`detcore/src/tool_global.rs:2522`). A symlink's own inode is determinized like any other.
- The `fstat`-only standard-stream inode override (`detcore/src/syscalls/files.rs:3043`) does not apply to `lstat`.
- Without `virtualize_metadata` (record/replay sets it to `false` at `hermit-cli/src/metadata.rs:406`), the call goes through `record_or_replay`. The recorder routes `Lstat` to its stat handler (`hermit-cli/src/recorder.rs:547`) and the replayer substitutes the recorded buffer (`hermit-cli/src/replayer.rs:450`), so replay does not need the path to exist.

Why `model`: `passthrough` leaks host inode and device numbers and real timestamps; `emulate` would mean re-implementing path resolution in the tracer, which the NB comment rejects; `refuse` would break `ls -l`, `find`, `tar` and most build tools.

What the model does not cover yet: `st_size`, `st_blocks`, `st_blksize`, `st_nlink`, `st_uid`, `st_gid`, `st_mode` and `st_rdev` pass through raw (`DetStat`, `detcore/src/stat.rs:46`). No test at this SHA names `SYS_lstat`. The detcore test `stat_lstat_symlink` (`detcore/tests/stats/mod.rs:205`), which checks that `lstat` of `/proc/self/exe` reports `S_IFLNK`, is in a file that is not a cargo test target (see Additional information).

## Recommended tests

- T1 (covers F1, F2, F6): with raw `syscall(SYS_lstat, ...)`, lstat a regular file, a directory, a symlink to that file, a dangling symlink, `/proc/self/exe` and a path that reaches the file through an intermediate symlink. Check `S_IFLNK` and `st_size` for the links, `stat`-equivalent results for the rest, and that `st_ino`, `st_dev` and the timestamps match between the two runs of `hermit run --verify` on ptrace.
  Existing partial candidate: `tests/c/stat_metadata_identity.c` (cell `c-programs/stat-metadata-identity`, `tests/e2e/manifests/c-programs.yaml:16625`, verify ci on ptrace) calls the glibc `lstat` wrapper at `tests/c/stat_metadata_identity.c:165`. Modern glibc implements that wrapper with `newfstatat(AT_FDCWD, path, buf, AT_SYMLINK_NOFOLLOW)`, so the fixture may never issue `SYS_lstat`.
- T2 (covers F3): `chdir` into a directory the guest created and lstat a relative path; then `fchdir` elsewhere and check the same relative name now gives `ENOENT`.
- T3 (covers F4, F5): raw `SYS_lstat` on a missing path (`ENOENT`), a path through a regular file (`ENOTDIR`), a symlink loop in an intermediate component (`ELOOP`), a 4096-byte component (`ENAMETOOLONG`), a NULL `path` and a NULL `statbuf` (`EFAULT`).
- T4 (covers F1): record a guest that lstats a symlink it then deletes, and replay it; the replayed output must show the recorded `S_IFLNK` result.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (same handler as stat/fstat: the kernel resolves the path and keeps the symlink rule, Detcore rewrites st_ino, st_dev and the timestamps); linux_determinism stays conditional; 6 flag behaviours and 4 tests listed; open questions: no test issues SYS_lstat and raw st_blocks/st_nlink/st_uid; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and block size, launching host uid); no U items; resolved the open question on virtualizing st_blocks, st_blksize, st_nlink, st_uid and st_gid

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lstat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lstat&body=Discussion+for+the+%60lstat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flstat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lstat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `lstat`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- No test at this SHA names `SYS_lstat`, and the glibc `lstat` wrapper is usually implemented with `newfstatat`, so the dispatch arm at `detcore/src/lib.rs:2411` may have no live coverage at all. T1 and T3 must use raw `syscall(SYS_lstat, ...)`.
- `stat_lstat_symlink` (`detcore/tests/stats/mod.rs:205`) is in a file that cargo does not build and whose BUCK target is commented out (`detcore/BUCK:145`). Revive or delete?
