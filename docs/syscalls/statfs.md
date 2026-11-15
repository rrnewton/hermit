---
schema: syscall-pages/v1
syscall: statfs
nr_x86_64: 137
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+statfs&body=Discussion+for+the+%60statfs%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstatfs.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60statfs%60%2C+or+in+what+the+page+says%3F%0A
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

`statfs(path, buf)` fills a `struct statfs` describing the filesystem that contains `path`. Detcore runs the real call, then overwrites the fields that change with host activity: the free-block and free-inode counts are set to fixed caps, and `f_fsid` is zeroed (`handle_statfs`, `detcore/src/syscalls/files.rs:3256`; `canonicalize_statfs_buf`, `detcore/src/syscalls/files.rs:3481`). The mount's geometry (type, block size, total blocks and inodes, name length, mount flags) is still the host's. `fstatfs` takes the same path through Detcore.

Manual reference: Linux man-pages statfs(2).

## Linux semantics

The kernel resolves `path` in the caller's mount namespace, following symbolic links, including a trailing one. Resolution needs search permission on every directory in the path prefix; the file itself needs no read or write permission. Reaching an automount point triggers the mount. The kernel then asks the filesystem's `statfs` operation for the superblock statistics of the mount that holds the resolved file, and copies the result to `buf`. The call has no side effects on the filesystem.

The result has two kinds of field:

- **Geometry**, fixed while the mount exists: `f_type` (the filesystem magic), `f_bsize`, `f_frsize`, `f_blocks`, `f_files` and `f_namelen`. `f_flags` holds `ST_VALID` plus the mount's flags. `f_fsid` identifies the filesystem; how it is derived depends on the filesystem type.
- **Usage counters**, which change whenever anything on the host allocates or frees space on that filesystem: `f_bfree`, `f_bavail` (free blocks available to an unprivileged user, which excludes the root reserve) and `f_ffree`.

`fstatfs(fd, buf)` returns the same structure for an open descriptor. glibc's `statvfs` is built on `statfs`, so `df` and most free-space checks reach this call.

### Flags and options

- F1: `path` on a disk-backed or tmpfs filesystem: the geometry fields `f_type`, `f_bsize`, `f_frsize`, `f_blocks`, `f_files` and `f_namelen` describe that mount
- F2: usage counters `f_bfree`, `f_bavail` and `f_ffree`: live free blocks, free blocks available to unprivileged users, and free inodes
- F3: a filesystem without inode accounting reports `f_files == 0` and `f_ffree == 0`. The Detcore helper comment names overlayfs as one such filesystem.
- F4: `f_fsid`: a filesystem identifier whose derivation depends on the filesystem type (for example from the device number)
- F5: `f_flags`: `ST_VALID` plus the mount flags of the mount that holds `path`: `ST_RDONLY`, `ST_NOSUID`, `ST_NODEV`, `ST_NOEXEC`, `ST_SYNCHRONOUS`, `ST_MANDLOCK`, `ST_NOATIME`, `ST_NODIRATIME`, `ST_RELATIME`, `ST_NOSYMFOLLOW`
- F6: `path` on a pseudo-filesystem such as `/proc`, `/sys` or `/dev/pts`: `f_type` names that filesystem and the counters are usually 0
- F7: symbolic links are followed, including a trailing one, so the result describes the link target's filesystem. Reaching an automount point mounts it first.
- F8: path resolution errors: `ENOENT` (a component does not exist, or `path` is empty), `ENOTDIR` (a prefix component is not a directory), `EACCES` (search permission denied on a prefix directory), `ELOOP` (too many symbolic links), `ENAMETOOLONG`
- F9: `path` or `buf` is not accessible: `EFAULT`. For `buf`, this happens after the filesystem has been queried.
- F10: host-transient filesystem errors: `EIO`, `ENOMEM`, and `EINTR` when a network or FUSE filesystem's request is interrupted. A FUSE server can delay the reply without bound. `ENOSYS` is F11.
- F11: host-config failure: `ENOSYS` when the filesystem behind `path` has no `statfs` operation.

## Determinism analysis

`linux_determinism: nondeterministic`, changed from the rendered `conditional` to agree with the `fstatfs` body, which covers the same kernel structure. The usage counters (F2) change as any process on the host writes or deletes files on the same filesystem, so two runs of the same program read different values. Even the guest's own writes may not move them at a fixed point: depending on the filesystem, blocks can be allocated at writeback rather than at `write`, and writeback timing depends on the host. `f_fsid` (F4) can differ between mounts of the same filesystem.

The geometry (F1, F3, F5, F6) does not change between runs on one host unless a mount is resized or remounted. It does differ between hosts, because it describes whatever filesystem the host provides under the guest's paths. That is a host input, not run-to-run variation. Path resolution results (F7, F8) depend only on the directory tree, which a deterministic guest controls inside its own container.

The call repeats only if nothing else on the host touches the filesystem during the run and the guest's own writeback finishes at the same points, which cannot be arranged in general.

### Nondeterminism sources

- Space and inode allocation by every process on the host that shares the filesystem
- Writeback timing for the guest's own writes, on filesystems that allocate at writeback
- `f_fsid` values tied to device numbers or per-mount identifiers
- Host filesystem choice and size, which set the geometry (a cross-host difference only)

### Cross-host variation

- Filesystem type and size behind the guest's paths: `f_type`, `f_bsize`, `f_frsize`, `f_blocks`, `f_files` and `f_namelen` (F1, F3) pass through unchanged, so they describe whatever filesystem the host provides.
- Filesystem size relative to Detcore's caps: the free counts are clamped to `min(1000000, f_blocks)` and `min(500000, f_files)`, so on a host filesystem smaller than the cap the guest reads the host's `f_blocks` or `f_files` instead of the constant (F2).
- Host mount flags: `f_flags` carries the host mount's `ST_RDONLY`, `ST_NOSUID`, `ST_NOEXEC` and the other flags (F5).
- Filesystem type: whether the filesystem has a `statfs` operation decides `ENOSYS` (F11).

## Hermit design

Target policy: `model`. The guest sees each filesystem with the host's geometry and a fixed, plentiful amount of free space that never changes. The code at hermit f4055eb9cae4 realizes this:

- The typed arm `Syscall::Statfs(s) => self.handle_statfs(guest, s).await` (`detcore/src/lib.rs:2801`) reaches `handle_statfs` (`detcore/src/syscalls/files.rs:3256`). The comment above the arm (`detcore/src/lib.rs:2795`) states the reason: a bare passthrough diverged under `--verify`, for example for `tar`.
- `handle_statfs` runs the real call through `record_or_replay` (`detcore/src/syscalls/files.rs:3261`). The kernel resolves the guest's path in the guest's own mount namespace, so F7 and F8 are the kernel's behaviour. An error return leaves through `?` before the buffer is touched, so F8, F9 and F10 return the kernel's answer unchanged.
- On success, `canonicalize_statfs_buf` (`detcore/src/syscalls/files.rs:3481`) reads the buffer back and rewrites three things:
  - `f_bfree` and `f_bavail` both become `min(1_000_000, f_blocks)` (`FREE_BLOCKS_CAP`, `detcore/src/syscalls/files.rs:3492`; applied at `detcore/src/syscalls/files.rs:3497`). This is F2.
  - `f_ffree` becomes 0 when `f_files == 0`, otherwise `min(500_000, f_files)` (`FREE_INODES_CAP`, `detcore/src/syscalls/files.rs:3493`; applied at `detcore/src/syscalls/files.rs:3503`). This is F2 and F3.
  - `f_fsid` is zeroed (`detcore/src/syscalls/files.rs:3510`). This is F4.
- `fstatfs` shares the same helper (`handle_fstatfs`, `detcore/src/syscalls/files.rs:3267`), so the two calls agree for the same file.
- Every other field is passed through: F1, F5 and F6 show the host's values.

Accepted differences from Linux, which are properties of the model rather than gaps:

- Free space never changes. A program that writes 1 GB and then checks free space sees the same number as before.
- `f_bavail` always equals `f_bfree`, so the root reserve is invisible.
- `f_fsid` is `{0, 0}` on every filesystem, so a program cannot tell two filesystems apart by `f_fsid`. It can still use `f_type`, or `st_dev` from `stat`.
- A filesystem with fewer than 1,000,000 blocks appears completely empty, because the cap is clamped to `f_blocks`.
- A program that checks for more free space than 1,000,000 blocks (about 4 GB at 4 KiB blocks) before writing sees too little, even when the host has more. It can still run out with `ENOSPC` when the host is fuller than the guest is told; that failure comes from the host, not from `statfs`.

Why not the alternatives:

- `passthrough` diverges under `--verify`; the handler's doc comment (`detcore/src/syscalls/files.rs:3246`) records the `tar` case.
- `emulate`, with fixed geometry as well, would hide which filesystem type the guest is on, and programs branch on `f_type`. Geometry is stable on one host, so keeping it costs nothing in run-to-run determinism.
- `refuse` would break `df`, `tar` and every `statvfs` user.

The cross-host difference in geometry is the one open point; see Open questions.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, call `statfs` on `/` and on a directory in `/tmp`. Check that it returns 0, that `f_bfree == f_bavail == min(1000000, f_blocks)`, and that every field equals what `fstatfs` reports on a descriptor for the same path.
  Existing candidate: `tests/c/statfs_free_determinism.c` (cell `c-programs/statfs-free-determinism`, `tests/e2e/manifests/c-programs.yaml:13780`, verify ci on ptrace). Its `statfs` half (`tests/c/statfs_free_determinism.c:59`) asserts the free counts on `/` equal exactly 1000000. That assumes the root filesystem has at least 1,000,000 blocks, and it does not compare against `fstatfs` or test `/tmp`.
- T2 (covers F2, F3): write and `fsync` 64 MiB in `/tmp`, then call `statfs` again. The free counts must be unchanged. `f_ffree` must be 0 when `f_files` is 0, and `min(500000, f_files)` otherwise.
- T3 (covers F4, F5, F6): `f_fsid` is `{0, 0}` for `/`, `/tmp`, `/proc` and `/dev/pts`. `f_flags` includes `ST_VALID` on each, and includes `ST_RDONLY` exactly on mounts the guest cannot write. `f_type` is `PROC_SUPER_MAGIC` for `/proc` and `DEVPTS_SUPER_MAGIC` for `/dev/pts`. The values must match between the two `--verify` runs.
- T4 (covers F7): create a symbolic link in `/tmp` that points at `/proc`. `statfs` on the link must report `PROC_SUPER_MAGIC`, the target's filesystem, not the filesystem holding the link.
- T5 (covers F8): `ENOENT` for a missing file and for `""`, `ENOTDIR` for `/etc/passwd/x`, `ELOOP` for a self-referencing link, `ENAMETOOLONG` for a 5000-byte path. `EACCES` for a file under a mode 0000 directory, run as a guest user without `CAP_DAC_READ_SEARCH`.
- T6 (covers F9): `path = (char *)1` gives `EFAULT`. A valid path with `buf = NULL`, and with `buf = (void *)1`, gives `EFAULT`, and Hermit must not fault while canonicalizing.
- T7 (covers none): when the CI host provides FUSE, mount a FUSE filesystem whose `statfs` handler returns `EIO` and check that the guest sees `EIO`. `ENOSYS`, `ENOMEM` and `EINTR` have no practical trigger on the CI host; record them as untested if FUSE is unavailable. A FUSE server set up by the test harness makes this `EIO` an answer the test chose; it does not cover F10, which is the host filesystem's own failure, or F11.
- T8 (covers F1, F2): record and replay a guest that prints its `statfs` result. Replay must print the same canonicalized values. The replay mode of `c-programs/statfs-free-determinism` is disabled today (`tests/e2e/manifests/c-programs.yaml:13812`).
- U1 (covers F10; host-transient): `EIO`, `ENOMEM` and the network or FUSE `EINTR` depend on storage health, host memory or an outside server's timing. The handler forwards the kernel's result (`detcore/src/syscalls/files.rs:3261`), so the error reaches the guest today. F10 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F11; host-config): which host filesystems lack a `statfs` operation is host configuration that CI cannot choose. Hermit forwards the kernel's `ENOSYS` unchanged (`detcore/src/syscalls/files.rs:3261`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> nondeterministic (host-wide space allocation and writeback timing move the free counters; matches fstatfs); target_policy investigate -> model (real call, then fixed free-space caps and a zero f_fsid, host geometry kept), which the code realizes; 10 flag behaviours and 8 tests; no coverage token recorded, so tracking_status stays todo; cross-host geometry left as an open question shared with fstatfs
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and size, free-count caps on small filesystems, host mount flags, statfs support); split F10 into host-transient F10 and new host-config F11 (`ENOSYS`), U1 host-transient covers F10, U2 host-config covers F11, T7 now covers none; resolved the open question on hiding the host's geometry

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `statfs`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+statfs&body=Discussion+for+the+%60statfs%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstatfs.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60statfs%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `statfs`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The test file the manifest lists for this syscall, `hermit-cli/tests/cli.rs`, names `SYS_statfs` only in `namespace_only_readonly_proc_warning_tolerates_denied_statfs` (`hermit-cli/tests/cli.rs:1387`). That test denies Hermit's own diagnostic `statfs` probe during container setup (`hermit-cli/tests/cli.rs:1398`) in `--namespace-only` mode, where Detcore does not run. It does not exercise `handle_statfs`.

### Open questions

- `tests/c/statfs_free_determinism.c` asserts exactly 1000000 free blocks on `/`. On a host whose root filesystem has fewer than 1,000,000 blocks, Hermit correctly reports `f_blocks` instead and the test fails. That is a host assumption of the test, not a Hermit defect.
