---
schema: syscall-pages/v1
syscall: fstatfs
nr_x86_64: 138
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fstatfs&body=Discussion+for+the+%60fstatfs%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffstatfs.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fstatfs%60%2C+or+in+what+the+page+says%3F%0A
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

`fstatfs(fd, buf)` fills a `struct statfs` describing the filesystem that holds the file `fd` refers to. Detcore runs the real call, then overwrites the fields that change with host activity: the free-block and free-inode counts are set to fixed caps, and `f_fsid` is zeroed (`handle_fstatfs`, `detcore/src/syscalls/files.rs:3267`; `canonicalize_statfs_buf`, `detcore/src/syscalls/files.rs:3481`). The mount's geometry (type, block size, total blocks and inodes, name length, mount flags) is still the host's.

Manual reference: Linux man-pages statfs(2).

## Linux semantics

The kernel resolves `fd` without requiring read or write access, so an `O_PATH` descriptor works. It then asks the filesystem's `statfs` operation for the superblock statistics of the mount that `fd` was opened through, and copies the result to `buf`. The call has no side effects.

The result has two kinds of field:

- **Geometry**, fixed while the mount exists: `f_type` (the filesystem magic), `f_bsize`, `f_frsize`, `f_blocks`, `f_files` and `f_namelen`. `f_flags` holds `ST_VALID` plus the mount's flags. `f_fsid` identifies the filesystem; how it is derived depends on the filesystem type.
- **Usage counters**, which change whenever anything on the host allocates or frees space on that filesystem: `f_bfree`, `f_bavail` (free blocks available to an unprivileged user, which excludes the root reserve) and `f_ffree`.

`statfs(path, buf)` returns the same structure for a path. `fstatvfs`/`statvfs` in glibc are built on these two calls.

### Flags and options

- F1: `fd` on a disk-backed or tmpfs filesystem: the geometry fields `f_type`, `f_bsize`, `f_frsize`, `f_blocks`, `f_files` and `f_namelen` describe that mount
- F2: usage counters `f_bfree`, `f_bavail` and `f_ffree`: live free blocks, free blocks available to unprivileged users, and free inodes
- F3: a filesystem without inode accounting reports `f_files == 0` and `f_ffree == 0`. The Detcore handler comment names overlayfs as one such filesystem.
- F4: `f_fsid`: a filesystem identifier whose derivation depends on the filesystem type (for example from the device number)
- F5: `f_flags`: `ST_VALID` plus the mount flags of the mount `fd` was opened through: `ST_RDONLY`, `ST_NOSUID`, `ST_NODEV`, `ST_NOEXEC`, `ST_SYNCHRONOUS`, `ST_MANDLOCK`, `ST_NOATIME`, `ST_NODIRATIME`, `ST_RELATIME`, `ST_NOSYMFOLLOW`
- F6: `fd` on a pseudo-filesystem object: a pipe (pipefs), a socket (sockfs), an anonymous inode such as an eventfd or epoll descriptor, a procfs or sysfs file, or a devpts terminal. `f_type` names that filesystem and the counters are usually 0.
- F7: an `O_PATH` descriptor is accepted
- F8: `fd` is not open: `EBADF`
- F9: `buf` is not writable: `EFAULT`, after the filesystem has been queried
- F10: host-transient filesystem errors: `EIO`, and `EINTR` when a network or FUSE filesystem's request is interrupted. A FUSE server can delay the reply without bound. `ENOSYS` is F11.
- F11: host-config failure: `ENOSYS` when the filesystem behind `fd` has no `statfs` operation.

## Determinism analysis

`linux_determinism: nondeterministic`. There was no legacy classification. The usage counters (F2) change as any process on the host writes or deletes files on the same filesystem, so two runs of the same program read different values. Even the guest's own writes may not move them at a fixed point: depending on the filesystem, blocks can be allocated at writeback rather than at `write`, and writeback timing depends on the host. `f_fsid` (F4) can differ between mounts of the same filesystem.

The geometry (F1, F3, F5, F6) does not change between runs on one host unless a mount is resized or remounted. It does differ between hosts, because it describes whatever filesystem the host provides under the guest's paths. That is a host input, not run-to-run variation.

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

- The typed arm `Syscall::Fstatfs(s) => self.handle_fstatfs(guest, s).await` (`detcore/src/lib.rs:2802`) reaches `handle_fstatfs` (`detcore/src/syscalls/files.rs:3267`). It runs the real call through `record_or_replay` (`detcore/src/syscalls/files.rs:3272`). An error return leaves through `?` before the buffer is touched, so F7, F8, F9 and F10 return the kernel's answer unchanged.
- On success, `canonicalize_statfs_buf` (`detcore/src/syscalls/files.rs:3481`) reads the buffer back and rewrites three things:
  - `f_bfree` and `f_bavail` both become `min(1_000_000, f_blocks)` (`FREE_BLOCKS_CAP`, `detcore/src/syscalls/files.rs:3492`; applied at `detcore/src/syscalls/files.rs:3497`). This is F2.
  - `f_ffree` becomes 0 when `f_files == 0`, otherwise `min(500_000, f_files)` (`FREE_INODES_CAP`, `detcore/src/syscalls/files.rs:3493`; applied at `detcore/src/syscalls/files.rs:3503`). This is F2 and F3.
  - `f_fsid` is zeroed (`detcore/src/syscalls/files.rs:3510`). This is F4.
- `statfs` shares the same helper (`handle_statfs`, `detcore/src/syscalls/files.rs:3256`), so the two calls agree for the same file.
- Every other field is passed through: F1, F5 and F6 show the host's values.

Accepted differences from Linux, which are properties of the model rather than gaps:

- Free space never changes. A program that writes 1 GB and then checks free space sees the same number as before.
- `f_bavail` always equals `f_bfree`, so the root reserve is invisible.
- `f_fsid` is `{0, 0}` on every filesystem, so a program cannot tell two filesystems apart by `f_fsid`. It can still use `f_type` or `st_dev` from `fstat`.
- A filesystem with fewer than 1,000,000 blocks appears completely empty, because the cap is clamped to `f_blocks`.

Why not the alternatives:

- `passthrough` diverges under `--verify`. The handler's doc comment (`detcore/src/syscalls/files.rs:3246`) records that `tar` calls `statfs` and that a bare passthrough diverged.
- `emulate`, with fixed geometry as well, would hide which filesystem type the guest is on. Programs branch on `f_type`. For example, the guest test `tests/compat/lsof_mount_redirect.c:71` calls `fstatfs` and rejects a descriptor whose `f_type` is `PROC_SUPER_MAGIC`. Geometry is stable on one host, so keeping it costs nothing in run-to-run determinism.
- `refuse` would break `df`, `tar`, `find` and every `statvfs` user.

The cross-host difference in geometry is the one open point; see Open questions.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, call `fstatfs` on a descriptor for `/` and on a file in `/tmp`. Check that it returns 0, that `f_bfree == f_bavail == min(1000000, f_blocks)`, and that every field equals what `statfs` reports for the same path.
  Existing candidate: `tests/c/statfs_free_determinism.c` (cell `c-programs/statfs-free-determinism`, `tests/e2e/manifests/c-programs.yaml:13780`, verify ci on ptrace). Its `fstatfs` half (`tests/c/statfs_free_determinism.c:71`) asserts the free counts on `/` equal exactly 1000000. That assumes the root filesystem has at least 1,000,000 blocks, and it does not compare against `statfs` or test `/tmp`.
- T2 (covers F2, F3): write and `fsync` 64 MiB in `/tmp`, then call `fstatfs` again. The free counts must be unchanged. `f_ffree` must be 0 when `f_files` is 0, and `min(500000, f_files)` otherwise.
- T3 (covers F4, F5): `f_fsid` is `{0, 0}` for `/`, `/tmp`, `/proc` and a pipe. `f_flags` includes `ST_VALID` on each, and includes `ST_RDONLY` exactly on mounts the guest cannot write. The values must match between the two `--verify` runs.
- T4 (covers F6, F7): call `fstatfs` on a pipe end, a socketpair end, an eventfd, a descriptor for `/proc/self/status`, and an `O_PATH` descriptor for a regular file. Each must succeed, with `f_type` equal to `PIPEFS_MAGIC`, `SOCKFS_MAGIC`, `ANON_INODE_FS_MAGIC`, `PROC_SUPER_MAGIC` and the file's filesystem magic respectively.
- T5 (covers F8, F9): a closed descriptor gives `EBADF`. A valid descriptor with `buf = NULL`, and with `buf = (void *)1`, gives `EFAULT`, and Hermit must not fault while canonicalizing.
- T6 (covers none): when the CI host provides FUSE, mount a FUSE filesystem whose `statfs` handler returns `EIO` and check that the guest sees `EIO`. `ENOSYS` and `EINTR` have no practical trigger on the CI host; record them as untested if FUSE is unavailable. A FUSE server set up by the test harness makes this `EIO` an answer the test chose; it does not cover F10, which is the host filesystem's own failure, or F11.
- T7 (covers F1, F2): record and replay a guest that prints its `fstatfs` result. Replay must print the same canonicalized values. The replay mode of `c-programs/statfs-free-determinism` is disabled today (`tests/e2e/manifests/c-programs.yaml:13812`).
- U1 (covers F10; host-transient): `EIO` and the network or FUSE `EINTR` depend on storage health or an outside server's timing. The handler forwards the kernel's result (`detcore/src/syscalls/files.rs:3272`), so the error reaches the guest today. F10 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F11; host-config): which host filesystems lack a `statfs` operation is host configuration that CI cannot choose. Hermit forwards the kernel's `ENOSYS` unchanged (`detcore/src/syscalls/files.rs:3272`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> nondeterministic (host-wide space allocation and writeback timing move the free counters); target_policy investigate -> model (real call, then fixed free-space caps and a zero f_fsid, host geometry kept), which the code realizes; 10 flag behaviours and 7 tests; no coverage token recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and size, free-count caps on small filesystems, host mount flags, statfs support); split F10 into host-transient F10 and new host-config F11 (`ENOSYS`), U1 host-transient covers F10, U2 host-config covers F11, T6 now covers none; resolved the open question on hiding the host's geometry

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fstatfs`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fstatfs&body=Discussion+for+the+%60fstatfs%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffstatfs.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fstatfs%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `fstatfs`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The dispatch comment above the `statfs`/`fstatfs` arms (`detcore/src/lib.rs:2795`) gives the same reason as the handler: a bare passthrough diverged under `--verify`, for example for `tar`.

### Open questions

- `tests/c/statfs_free_determinism.c` asserts exactly 1000000 free blocks on `/`. On a host whose root filesystem has fewer than 1,000,000 blocks (about 4 GB at 4 KiB blocks), Hermit correctly reports `f_blocks` instead and the test fails. That is a host assumption of the test, not a Hermit defect.
- The compat corpus row `df` runs as a bounded diagnostic, tracked in https://github.com/rrnewton/hermit/issues/2801 (`tests/e2e/manifests/compat.yaml:52`). The tracked problem is mountinfo paths, not `statfs`, but `df` is the main real program that reads these fields.
