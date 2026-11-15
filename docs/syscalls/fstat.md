---
schema: syscall-pages/v1
syscall: fstat
nr_x86_64: 5
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fstat&body=Discussion+for+the+%60fstat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffstat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fstat%60%2C+or+in+what+the+page+says%3F%0A
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

`fstat(fd, statbuf)` fills a `struct stat` for the file an open descriptor refers to. Detcore handles it in `handle_stat_family` (`detcore/src/syscalls/files.rs:3031`): when `virtualize_metadata` is on (the `hermit run` default), the kernel runs the call and Detcore then rewrites the identity and time fields with deterministic values. In record/replay, which turns `virtualize_metadata` off, the result buffer is recorded and replayed.

Manual reference: Linux man-pages stat(2).

## Linux semantics

`fstat` cannot block and has no side effects. It copies the inode's attributes into the caller's buffer. Several fields are host state that the guest never chose:

- `st_ino` and `st_dev` name the inode and the filesystem. Pipes, sockets, anonymous inodes, procfs, sysfs and tmpfs get numbers from host-wide allocators, so they change between runs.
- `st_atime`, `st_mtime`, `st_ctime` (and the birth time, which `fstat` does not report) come from the real clock when the file was touched.
- `st_size`, `st_blocks`, `st_blksize`, `st_nlink`, `st_uid`, `st_gid`, `st_mode` and `st_rdev` describe the file as the host filesystem and the user namespace present it. For files the guest created on the same filesystem they follow from the guest's own actions; for host files they are whatever the host holds.

Errors: `EBADF` for a descriptor that is not open, and `EFAULT` when `statbuf` is not writable. An `O_PATH` descriptor is accepted. `EOVERFLOW` exists only for 32-bit callers and cannot occur on x86_64.

### Flags and options

- F1: regular file or directory descriptor: returns the inode's attributes, including size, link count and the three timestamps
- F2: pipe, FIFO or socket descriptor: `S_IFIFO` or `S_IFSOCK` in `st_mode`, with `st_ino` and `st_dev` from the kernel's pseudo-filesystems (pipefs, sockfs)
- F3: inherited standard streams (fd 0, 1, 2 as the container provides them): attributes of whatever the outer runner connected, such as a tty, a pipe or a log file
- F4: `O_PATH` descriptor: accepted, and returns the attributes of the referenced inode
- F5: procfs, sysfs and device-node descriptors: host-assigned `st_dev`, `st_ino` and, for devices, `st_rdev`
- F6: descriptor not open: `EBADF`
- F7: `statbuf` NULL or not writable: `EFAULT`

## Determinism analysis

`linux_determinism: conditional`. Given the same file in the same state, `fstat` returns the same bytes, except for the fields the host assigns: inode and device numbers from global allocators and timestamps from the real clock. Those differ between two runs of the same program. The call repeats only when every inode it is asked about is stable on the host and was not touched by the clock between runs, which is rarely true for pipes, sockets, procfs, tmpfs or any file the guest writes.

### Nondeterminism sources

- `st_ino` and `st_dev` from host-wide allocators (pipefs, sockfs, anonymous inodes, procfs, tmpfs). The `TODO-HUMAN-REVIEW(PR-1056)` comment at `detcore/src/syscalls/files.rs:3008` records that the raw `st_dev` of procfs, sysfs and tmpfs drifted between the two runs of `--verify`.
- `st_atime`, `st_mtime` and `st_ctime` from the real clock
- Host filesystem layout: `st_blocks` and `st_blksize` depend on the filesystem; `st_size`, `st_nlink`, `st_uid` and `st_gid` of host files depend on the host
- What the outer runner connected to fd 0, 1 and 2

### Cross-host variation

- Filesystem type and block size behind the file: `st_blksize` and `st_blocks`, and a directory's `st_size` and `st_nlink`, are the host filesystem's values, which Detcore passes through (`detcore/src/stat.rs:46`); btrfs, ext4 and tmpfs report different values for the same guest-created file or directory (F1).
- Host uid that launches Hermit: the user namespace maps that one uid to the guest's root, so the `st_uid` and `st_gid` of host files the guest can see (passed through raw) differ between hosts (F1, F5).

## Hermit design

Target policy: `model`. The kernel remains the source of the stat record and of every error, and Detcore replaces the host-assigned identity and time fields with values from its own model. At hermit f4055eb9cae4:

- The dispatch arm is `detcore/src/lib.rs:2412`, which calls `handle_stat_family` (`detcore/src/syscalls/files.rs:3031`), shared with `stat`, `lstat` and `newfstatat`.
- With `virtualize_metadata`, the call is injected first (`detcore/src/syscalls/files.rs:3041`), so `EBADF` and `EFAULT` (F6, F7) come from the kernel unchanged. The NB comment there gives the reason: reproducing every kernel corner case in the tracer is impractical.
- `determinize_stat` (`detcore/src/syscalls/files.rs:2981`) then rewrites the buffer:
  - `st_ino` becomes a deterministic inode from `determinize_inode` (`detcore/src/tool_global.rs:3682`), which hands out run-local numbers in first-seen order.
  - `st_dev` becomes a deterministic device from `determinize_device` (`detcore/src/syscalls/files.rs:3013`, `detcore/src/tool_global.rs:3697`).
  - `st_atime` and `st_ctime` (and the birth time used by `statx`) become the configured epoch; `st_mtime` becomes Detcore's logical mtime for that inode. The logical mtime starts at the epoch when the inode is first seen (`detcore/src/tool_global.rs:2522`) and is advanced when the guest writes through a tracked descriptor (`touch_file` in `handle_write`, `detcore/src/syscalls/files.rs:2255`).
- For `fstat` only, descriptors still bound to the container's standard streams get fixed inodes 1000, 1001 and 1002 (`deterministic_stdio_inode_for_resource`, `detcore/src/syscalls.rs:48`; unit test `stdio_inode_namespace_is_fixed` at `detcore/src/syscalls.rs:65`). A file the guest installs on fd 1 with `dup2` loses that override and gets an ordinary pooled inode (F3).
- Without `virtualize_metadata` (record/replay sets it to `false` at `hermit-cli/src/metadata.rs:406`; `--strace-only` clears it at `hermit-cli/src/bin/hermit/run.rs:3757`), the call goes through `record_or_replay`. The recorder routes `Fstat` to its own stat handler (`hermit-cli/src/recorder.rs:546`) and the replayer substitutes the recorded buffer (`hermit-cli/src/replayer.rs:449`).

Why `model` and not the alternatives: `passthrough` would leak host inode numbers, device numbers and real timestamps into every `--verify` comparison. `emulate` would have to reproduce kernel error precedence and every file type, which the NB comment explicitly rejects. `refuse` would break almost every program.

What the model does not cover yet:

- `st_size`, `st_blocks`, `st_blksize`, `st_nlink`, `st_uid`, `st_gid`, `st_mode` and `st_rdev` pass through raw (`DetStat` in `detcore/src/stat.rs:46` carries them unchanged). For files the guest created they are a function of the guest's own writes on a given filesystem; for host files, procfs files and devices they are host state. `st_blocks` in particular depends on the filesystem's allocation policy.
- The detcore stat tests in `detcore/tests/stats/mod.rs`, including `fstat_with_nullptr_errno_sanity` (`detcore/tests/stats/mod.rs:177`) and `stat_fstat_is_consistent` (`detcore/tests/stats/mod.rs:224`), are not a cargo test target: `detcore/Cargo.toml` declares only `tests_misc`, `tests_parallelism` and `tests_time`, and the BUCK target `tests_stats` is commented out ("too brittle", `detcore/BUCK:145`). They do not run.

## Recommended tests

- T1 (covers F1, F2, F5): with raw `syscall(SYS_fstat, ...)`, fstat a regular file the guest creates, a directory, a pipe, a socketpair end and a `/proc` file, and print `st_ino`, `st_dev`, the three timestamps, `st_nlink` and `st_blocks`. Under `hermit run --verify` on ptrace every printed value must match between runs, and a write followed by a second fstat must show `st_mtime` advancing.
  Existing candidate: `tests/c/stat_metadata_identity.c` (cell `c-programs/stat-metadata-identity`, `tests/e2e/manifests/c-programs.yaml:16625`, verify ci on ptrace). It calls the glibc `fstat` wrapper (`tests/c/stat_metadata_identity.c:103`, `tests/c/stat_metadata_identity.c:195`), which modern glibc may implement with `newfstatat`, so it may not reach `SYS_fstat` at all.
- T2 (covers F3): fstat fd 0, 1 and 2 as inherited, then after `dup2` of a regular file onto fd 1, and check that the inherited streams report inodes 1000, 1001 and 1002 while the replaced slot reports an ordinary inode.
  Existing candidate: `tests/c/fixtures/stdio_inode_identity.c`, which uses raw `syscall(SYS_fstat, ...)` (`tests/c/fixtures/stdio_inode_identity.c:91`) and is run by `run_kvm_preserves_closed_standard_input` (`hermit-cli/tests/cli.rs:4254`) for both `ptrace` and `kvm`.
- T3 (covers F4): open a file with `O_PATH`, fstat it, and check that the result succeeds and matches a path `stat` of the same file in `st_ino`, `st_dev` and `st_mtime`.
- T4 (covers F6, F7): raw `SYS_fstat` on a closed descriptor gives `EBADF`; on a valid descriptor with a NULL buffer gives `EFAULT`. Existing logic in `fstat_with_nullptr_errno_sanity` (`detcore/tests/stats/mod.rs:177`) is not wired as a test target, so a live test is still needed.
- T5 (covers F1): record and replay a guest that fstats a regular file and prints the fields; replay must print the recorded values even after the file is deleted between record and replay.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel produces the stat record and every error; Detcore rewrites st_ino, st_dev and the timestamps, with fixed inodes for inherited stdio); linux_determinism stays conditional; 7 flag behaviours and 5 tests listed; open questions on raw st_blocks/st_nlink/st_uid and the unbuilt detcore/tests/stats; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and block size, launching host uid); no U items; resolved the open question on virtualizing st_blocks, st_blksize, st_nlink, st_uid and st_gid

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fstat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fstat&body=Discussion+for+the+%60fstat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffstat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fstat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `fstat`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- `detcore/tests/stats/mod.rs` is not built by cargo, and its BUCK target is commented out as too brittle (`detcore/BUCK:145`). Should it be revived as a test target or deleted? Until then `fstat_with_nullptr_errno_sanity` and `stat_fstat_is_consistent` give no coverage.
- `detcore/tests/lit/fstat/main.rs:13` carries a FIXME saying `hermit run --strict --verify` fails because of an `st_mtime` nondeterminism bug. Whether that is still true at this SHA has not been measured.
- The glibc `fstat` wrapper may be implemented with `newfstatat`, so a test that wants to exercise `SYS_fstat` should call it with raw `syscall(SYS_fstat, ...)`, as `tests/c/fixtures/stdio_inode_identity.c:91` does.
