---
schema: syscall-pages/v1
syscall: stat
nr_x86_64: 4
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+stat&body=Discussion+for+the+%60stat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60stat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 11
---

## Summary

`stat(pathname, statbuf)` resolves a path, following symlinks, and fills a `struct stat` with the file's metadata. Detcore handles it with the stat-family handler shared with `lstat`, `fstat` and `newfstatat` (`detcore/src/lib.rs:2410`, `handle_stat_family` at `detcore/src/syscalls/files.rs:3031` at hermit f4055eb9cae4). With metadata virtualization on, the handler lets the kernel run the call, then replaces the inode number, device number and all time stamps with deterministic values.

Manual reference: Linux man-pages stat(2).

## Linux semantics

`stat` takes no flags. The kernel resolves `pathname` from the current directory, follows every symlink including the last component, and copies the target's metadata to `statbuf`. The caller needs search permission on every directory in the path, but no permission on the file itself. On x86-64, glibc's `stat()` issues `newfstatat(AT_FDCWD, path, buf, 0)`, measured on this host with strace. Syscall number 4 is reached only by raw `syscall(SYS_stat, ...)`, static or nolibc programs, and older runtimes.

The result carries host facts: inode and device numbers assigned by the filesystem and the kernel, time stamps from the wall clock at the moment of each change, and sizes and block counts that depend on the filesystem.

### Flags and options

- F1: symlinks: every symlink in the path is followed, including the last component, so the result describes the target. `lstat` is the variant that does not follow the last one.
- F2: `st_ino` and `st_dev`: the inode number assigned by the filesystem, and the device number of the mount. Together they identify the file, and programs compare them to detect the same file or a crossed mount.
- F3: time stamps: `st_atime`, `st_mtime` and `st_ctime`, each with nanosecond precision, set by the kernel from the wall clock when the file was last read, last modified, and last changed in content or metadata.
- F4: other fields: `st_mode` (type and permission bits), `st_nlink`, `st_uid`, `st_gid`, `st_size`, `st_blksize` and `st_blocks`, reported as the filesystem holds them. `st_size` and `st_blocks` for directories and sparse files depend on the filesystem type.
- F5: device files: `st_rdev` holds the device number for character and block devices, and 0 otherwise.
- F6: path errors: `ENOENT` for a missing component or an empty `pathname`, `ENOTDIR`, `EACCES` for missing search permission, `ELOOP` for too many symlinks, `ENAMETOOLONG`, and `EFAULT` for a bad `pathname` pointer.
- F7: `statbuf` errors: an unwritable `statbuf` gives `EFAULT`, but only after the path resolves. A missing path with a null `statbuf` gives `ENOENT`, not `EFAULT`.
- F8: `EOVERFLOW`: on x86-64 every field fits, so this occurs only for 32-bit callers. It is listed for completeness.
- F9: generated files: procfs, sysfs, pipes reached through `/proc/self/fd/N`, and sockets report kernel-generated sizes (usually 0), inode numbers and time stamps that change between boots or between runs.

## Determinism analysis

`linux_determinism: conditional`. A `stat` of a file that nobody changed between runs, on the same host, repeats exactly. It varies in these cases:

- A file created or rewritten during the run gets a new inode number, possibly reused from a deleted file, and time stamps from the wall clock (F2, F3). That affects almost every program that writes a file and then checks it, and build tools such as make that compare mtimes.
- Reads update `st_atime`, depending on mount options such as `relatime` (F3).
- Device numbers for procfs, sysfs, tmpfs and other anonymous mounts are assigned at mount time and differ between boots and containers (F2, F9).
- Sizes and block counts depend on the filesystem and on when write-back allocated blocks (F4).
- Files created or removed by host processes change which errors occur (F6).

## Hermit design

Target policy: `model`. The kernel resolves the path, checks permissions and reports errors. Detcore rewrites the identity and time fields so the guest sees deterministic values. At hermit f4055eb9cae4:

- The dispatch converts `Syscall::Stat` into the stat family and calls `handle_stat_family`, as it does for `lstat`, `fstat` and `newfstatat` (`detcore/src/lib.rs:2410-2413`).
- With metadata virtualization on (the default, turned off by `--no-virtualize-metadata`; `detcore-model/src/config.rs:247-253`), it injects the real call first, so the kernel decides every error, including the F7 rule that path errors come before a bad `statbuf` (`detcore/src/syscalls/files.rs:3036-3041`). It then reads the result from guest memory, calls `determinize_stat`, and writes the result back (`detcore/src/syscalls/files.rs:3053-3056`).
- `determinize_stat` (`detcore/src/syscalls/files.rs:2981`):
  - It replaces `st_ino` with a virtual inode that Detcore assigns in the order it first sees each host inode, and never reuses (`detcore/src/syscalls/files.rs:3003-3005`, `determinize_inode` at `detcore/src/tool_global.rs:3682`, `recv_determinize_inode` at `detcore/src/tool_global.rs:2522`).
  - It replaces `st_dev` with a deterministic device number (`detcore/src/syscalls/files.rs:3013`, carrying TODO-HUMAN-REVIEW(PR-1056)).
  - It sets `st_atime` and `st_ctime` to the configured epoch (`detcore/src/syscalls/files.rs:3021-3023`).
  - It sets `st_mtime` to the inode's logical mtime (`detcore/src/syscalls/files.rs:3025`). That is the epoch when Detcore first sees the inode (`detcore/src/tool_global.rs:2524-2535`), and the global logical clock after each write through `write` (see the write body; `touch_file` at `detcore/src/tool_global.rs:3763`).
  - It passes through every other field, as the kernel reported it: `st_mode`, `st_nlink`, `st_uid`, `st_gid`, `st_size`, `st_blksize`, `st_blocks` and `st_rdev` (fields at `detcore/src/stat.rs:46-80`). That leaves F4 and F5 to the host.
- The `fstat` stdio inode override (`detcore/src/syscalls/files.rs:3043-3052`) does not apply to `stat`.
- With metadata virtualization off, the call runs through `record_or_replay` and the host values reach the guest (`detcore/src/syscalls/files.rs:3058-3060`). Record and replay turn metadata virtualization off (`hermit-cli/src/metadata.rs:406`), so a recording captures host values and replay reproduces them.

Why `model`: `passthrough` would expose inode numbers, device numbers and wall-clock time stamps, which are the main sources of variation for a program that writes and then checks files. `emulate` would mean reimplementing path resolution and permissions over a real filesystem. `refuse` is impossible: static programs and older runtimes issue `stat` directly.

Gaps against `model`, by reading the code:

- The virtual inode table is keyed by the host inode number alone (`RawInode` is `u64`, `detcore-model/src/fd.rs:19`), without the device. Two files on different filesystems that share a host inode number would get the same virtual inode and the same logical mtime. Not measured.
- `st_size`, `st_blocks` and `st_nlink` come from the host filesystem, so they agree between runs on one host but can differ between hosts or filesystem types (F4).
- Every recommended test below must issue raw `syscall(SYS_stat, ...)`. glibc's `stat()` exercises `newfstatat`, which shares this handler but not this syscall number.

## Recommended tests

- T1 (covers F1, F2): raw `SYS_stat` of a symlink and of its target gives the same `st_ino` and `st_dev`. Two different files give different inode numbers. All values are identical across `hermit run --verify` on ptrace.
  Existing partial candidate (through `newfstatat` and `statx`, not syscall number 4): `tests/c/stat_metadata_identity.c` (cell `c-programs/stat-metadata-identity`, verify ci on ptrace) pins inode, device, time stamp, link count and block values through the same `determinize_stat` path.
- T2 (covers F3): create a file, raw-stat it, write to it, and raw-stat it again. `st_mtime` must advance by an amount identical across runs, and `st_atime` and `st_ctime` must equal the configured epoch.
  Existing partial candidate (through glibc wrappers): `tests/c/file_timestamp_identity.c` (cell `system-utils/file-timestamp-identity`, verify ci on ptrace).
- T3 (covers F4, F5): raw-stat a regular file, a directory, a sparse file, `/dev/null` and `/dev/urandom`. Check `st_mode`, `st_size`, `st_nlink` and `st_rdev` against known values, and require identical output across runs.
- T4 (covers F6): raw `SYS_stat` with a null path (`EFAULT`), an empty path (`ENOENT`), a missing path (`ENOENT`), a path through a regular file (`ENOTDIR`), a symlink loop (`ELOOP`), a path longer than `PATH_MAX` (`ENAMETOOLONG`), and a directory without search permission as a non-root virtual user (`EACCES`).
  Existing partial candidate (through glibc wrappers): `tests/c/errno_path_identity.c` (cell `system-utils/errno-path-identity`, verify ci on ptrace).
- T5 (covers F7, F8): raw `SYS_stat` of an existing path with a null `statbuf` gives `EFAULT`, and of a missing path with a null `statbuf` gives `ENOENT`. `EOVERFLOW` cannot occur on x86-64, so the test records that a 64-bit call on a large sparse file succeeds.
  The same checks exist as `stat_with_nullptr_errno_sanity` in `detcore/tests/stats/mod.rs:163-174`, but that file is not a cargo test target at this SHA (`detcore/Cargo.toml` declares only `tests/misc`, `tests/parallelism` and `tests/time`), so it runs nowhere.
- T6 (covers F9): raw-stat `/proc/self`, `/proc/self/status`, a pipe through `/proc/self/fd/N`, and a socket through `/proc/self/fd/N`. Require identical inode, device and time values across runs.
  Existing partial candidate: `tests/c/procfs_identity_agreement.c` (cell `c-programs/procfs-identity-agreement`, verify ci on ptrace).
- T7 (covers F2, F3): record and replay a program that raw-stats a file it created. Replay must reproduce the recorded values.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_stat_family lets the kernel decide errors, then determinize_stat replaces st_ino, st_dev and all time stamps; mode, nlink, uid, gid, size, blocks and rdev pass through); linux_determinism stays conditional (unchanged files repeat; created or written files get new inodes and wall-clock times); 9 flag behaviours and 7 tests listed, all needing raw SYS_stat because glibc uses newfstatat; gap noted: virtual inodes are keyed by host inode number without the device; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `stat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+stat&body=Discussion+for+the+%60stat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60stat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `stat`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
