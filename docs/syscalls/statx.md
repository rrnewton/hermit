---
schema: syscall-pages/v1
syscall: statx
nr_x86_64: 332
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+statx&body=Discussion+for+the+%60statx%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstatx.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60statx%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 10
---

## Summary

`statx(dirfd, pathname, flags, mask, statxbuf)` describes a file in an extensible `struct statx`. Detcore has a typed handler (`detcore/src/syscalls/files.rs:3064`): with metadata virtualization on (the default), it lets the kernel run the call, then rewrites the result with a virtual inode number, a virtual device number, epoch access, change and birth times and a logical modification time. Fields the handler does not know about are written as zero.

## Linux semantics

The kernel resolves `pathname` relative to `dirfd` (or the current directory for `AT_FDCWD`, or not at all for an absolute path), applies `flags`, collects the attributes and copies a whole `struct statx` (256 bytes) to `statxbuf`. `mask` says which fields the caller wants. It is a hint: the kernel may fill more or fewer, and `stx_mask` reports which fields are valid. The call changes nothing, except that `AT_STATX_FORCE_SYNC` may make a network filesystem refresh its cached attributes.

### Flags and options

- F1: path resolution: an absolute `pathname` ignores `dirfd`; a relative one is resolved against `dirfd` or, for `AT_FDCWD`, the current directory. A bad `dirfd` with a relative path gives `EBADF`; a `dirfd` that is not a directory gives `ENOTDIR`.
- F2: `AT_EMPTY_PATH` (0x1000) with an empty `pathname`: describe `dirfd` itself, which may be any file type, including an `O_PATH` descriptor, a pipe or a socket. Since Linux 6.11 a NULL `pathname` with `AT_EMPTY_PATH` does the same. Without the flag, an empty path gives `ENOENT`.
- F3: `AT_SYMLINK_NOFOLLOW` (0x100): if the last component is a symbolic link, describe the link itself.
- F4: `AT_NO_AUTOMOUNT` (0x800): do not trigger an automount on the last component; describe the mount point instead.
- F5: synchronization type: `AT_STATX_SYNC_AS_STAT` (0, whatever `stat` does), `AT_STATX_FORCE_SYNC` (0x2000, refresh from the server on network filesystems) and `AT_STATX_DONT_SYNC` (0x4000, use cached values). Both bits together give `EINVAL`.
- F6: any other `flags` bit: `EINVAL`.
- F7: path errors: `ENOENT`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG`, `EACCES` on a search-permission failure, and `EFAULT` for an unreadable `pathname`.
- F8: `statxbuf` NULL or unwritable: `EFAULT`.
- F9: `mask` with `STATX__RESERVED` (0x80000000): `EINVAL`.
- F10: the basic fields (`STATX_TYPE`, `MODE`, `NLINK`, `UID`, `GID`, `ATIME`, `MTIME`, `CTIME`, `INO`, `SIZE`, `BLOCKS`, together `STATX_BASIC_STATS` 0x7ff): filled on most filesystems whatever the mask says. These carry `stx_mode`, `stx_nlink`, `stx_uid`, `stx_gid`, `stx_size`, `stx_blocks` and, always, `stx_blksize`.
- F11: `STATX_BTIME` (0x800): the birth time, where the filesystem records one; otherwise the bit is clear in `stx_mask`.
- F12: `STATX_MNT_ID` (0x1000, Linux 5.8) and `STATX_MNT_ID_UNIQUE` (0x4000, Linux 6.8): the mount ID in `stx_mnt_id`. The kernel reports `STATX_MNT_ID` in `stx_mask` even when it was not requested.
- F13: `STATX_DIOALIGN` (0x2000, Linux 6.1) and `STATX_DIO_READ_ALIGN` (0x20000, Linux 6.14): direct-I/O alignment limits.
- F14: `STATX_SUBVOL` (0x8000, Linux 6.10): subvolume ID on filesystems that have them, such as btrfs and bcachefs.
- F15: `STATX_WRITE_ATOMIC` (0x10000, Linux 6.11): atomic-write unit limits.
- F16: `stx_attributes` and `stx_attributes_mask`: compressed, immutable, append-only, no-dump, encrypted, automount, mount root, verity and DAX, plus newer bits such as the atomic-write attribute.
- F17: identity: `stx_ino`, the device numbers `stx_dev_major` and `stx_dev_minor`, and `stx_rdev_major` and `stx_rdev_minor` for device files.
- F18: timestamps: `stx_atime`, `stx_mtime` and `stx_ctime`, in nanoseconds.

## Determinism analysis

`linux_determinism: conditional`. The call reads state; it changes nothing. It repeats when the same object is described in the same state, but several fields do not repeat across runs even for an unchanged file:

- `stx_ino` comes from the host filesystem; a file created again, or a pipe or socket, gets a different number each run (F17).
- `stx_dev` for procfs, sysfs, tmpfs and other anonymous mounts is a host-wide counter that drifts between runs (F17).
- Access, modification, change and birth times are real time (F11, F18).
- `stx_mnt_id` and, where present, `stx_subvol` are host-assigned numbers (F12, F14).
- `stx_blocks`, `stx_blksize`, the alignment and atomic-write limits and the attribute bits depend on the host filesystem and device (F10, F13, F15, F16).
- Whether the path exists, and which error results, depend on filesystem state (F1, F7). `AT_STATX_FORCE_SYNC` and `AT_STATX_DONT_SYNC` can return different attributes on network filesystems (F5).

### Cross-host variation

- Filesystem type and block size behind the path: `stx_blksize` and `stx_blocks`, and a directory's `stx_size` and `stx_nlink`, are the host filesystem's values, which Detcore passes through (`detcore/src/stat.rs:46`) (F10).
- Host uid that launches Hermit: the user namespace maps that one uid to the guest's root, so the `stx_uid` and `stx_gid` of host files the guest can see (passed through raw) differ between hosts (F10).
- Filesystem type: whether the filesystem records a birth time decides whether `STATX_BTIME` is set in `stx_mask` (F11); Detcore replaces the time itself but keeps the kernel's mask bit.
- Filesystem type and inode flags: the `stx_attributes` and `stx_attributes_mask` bits Detcore knows (compressed, immutable, append-only, no-dump, encrypted, automount, mount root, verity, DAX) pass through from the host (F16).
- Kernel version: kernels before 5.8 do not report `STATX_MNT_ID` in `stx_mask` (F12), and kernels before 6.11 reject a NULL `pathname` with `AT_EMPTY_PATH` (F2).

## Hermit design

Target policy: `model`. The kernel resolves the path and produces the error codes; Detcore replaces the fields that leak host identity or time. What the handler at f4055eb9cae4 does:

- With `virtualize_metadata` on, which is the default (the option is `--no-virtualize-metadata`, `detcore-model/src/config.rs:252`), it injects the call unchanged (`detcore/src/syscalls/files.rs:3073`). Any kernel error returns to the guest as is, so F1 to F9 are Linux's own. The comment at `detcore/src/syscalls/files.rs:3070` gives the reason: reproducing the error cases without the kernel is impractical.
- It reads the kernel's `struct statx` back from `statxbuf` (`detcore/src/syscalls/files.rs:3076`) and converts it to `DetStat`. The conversion keeps only mask bits the pinned reverie `StatxMask` defines (`detcore/src/stat.rs:188`) and only the nine attribute bits `StatxAttributes` defines (`detcore/src/stat.rs:192` and `detcore/src/stat.rs:21`).
- `determinize_stat` (`detcore/src/syscalls/files.rs:3077`, called with no inode override) replaces the inode with a virtual inode from `determinize_inode` (`detcore/src/syscalls/files.rs:3003`), the device with `determinize_device` (`detcore/src/syscalls/files.rs:3013`), the access, change and birth times with the configured epoch (`detcore/src/syscalls/files.rs:3021` to `detcore/src/syscalls/files.rs:3023`), and the modification time with the inode's logical time (`detcore/src/syscalls/files.rs:3025`). That covers F11, F17 and F18.
- It writes the result back as a fresh, zero-filled `struct statx` (`detcore/src/stat.rs:218`) carrying only the `DetStat` fields (`detcore/src/stat.rs:219` to `detcore/src/stat.rs:237`), and returns 0.
- With `virtualize_metadata` off (`--no-virtualize-metadata`, or `--strace-only`, which turns it off at `hermit-cli/src/bin/hermit/run.rs:3757`), the call goes through `record_or_replay` (`detcore/src/syscalls/files.rs:3081`) and the guest sees the host's values.

The device remapping carries the code tag `TODO-HUMAN-REVIEW(PR-1056)` (`detcore/src/syscalls/files.rs:3008`).

Why `model` and not the alternatives: `passthrough` leaks host inode numbers, device numbers and times, which tools such as `make`, `tar` and `git` print or compare. `emulate` would have to reproduce path resolution and its error cases, which the handler's comment rules out. `refuse` would break programs that call `statx` directly, such as coreutils `ls` and `stat`.

What remains before the code fully realizes `model`:

- **Mount ID (F12).** The pinned reverie `StatxMask` defines `STATX_MNT_ID` (0x1000) (`reverie-syscalls/src/args.rs:441` at reverie d766df20b780, the revision in Hermit's `Cargo.lock`; outside the export), so the bit the kernel always sets survives the conversion. But `DetStat` has no mount-ID field, so the zero-filled output carries `stx_mnt_id == 0`. By this reading of the code, a guest sees `STATX_MNT_ID` set in `stx_mask` with mount ID 0, which disagrees with `/proc/self/mountinfo`. Not measured; T8 measures it.
- **Newer mask fields (F13, F14, F15).** `STATX_DIOALIGN`, `STATX_MNT_ID_UNIQUE`, `STATX_SUBVOL`, `STATX_WRITE_ATOMIC` and `STATX_DIO_READ_ALIGN` are not in the pinned `StatxMask`, so `from_bits_truncate` clears them and their fields are written as 0. The guest sees a consistent "not supported" answer, which is deterministic but hides direct-I/O and atomic-write limits from programs that size their I/O by them.
- **Attribute bits (F16).** Bits outside the nine known ones are cleared from both `stx_attributes` and `stx_attributes_mask`. The rest pass through from the host.
- **Host-passthrough fields (F10).** `stx_nlink`, `stx_blocks`, `stx_blksize`, `stx_size`, `stx_mode`, `stx_uid`, `stx_gid` and `stx_rdev` are the kernel's. `stx_blocks` and `stx_blksize` depend on the host filesystem.
- **Birth time without the mask bit (F11).** `determinize_stat` sets `btime` to the epoch even when the kernel did not report `STATX_BTIME`. The mask still says the field is invalid, so this is harmless unless a program reads it anyway.
- **Inherited standard streams (F2, F17).** `handle_stat_family` gives `fstat` on descriptors 0 to 2 a stable inode from `deterministic_stdio_inode_for_resource` (`detcore/src/syscalls/files.rs:3043`); `handle_statx` passes no override. By this reading of the code, `statx(0, "", AT_EMPTY_PATH, ...)` on an inherited standard stream can report a different inode from `fstat(0)`. The fixture `tests/c/fixtures/stdio_inode_identity.c:7` states that "Inherited stdio uses only fstat bookkeeping: its cross-entrypoint alias behavior is a separate repair". Not measured; T7 measures it.

`linux_determinism` stays `conditional`. With metadata virtualization on, Hermit removes the inode, device and time sources. Filesystem-dependent sizes and the passthrough fields remain.

## Recommended tests

- T1 (covers F1, F7): `statx` with an absolute path, a path relative to `AT_FDCWD` and one relative to a directory descriptor all report the same virtual inode; a bad `dirfd` with a relative path gives `EBADF`; a file `dirfd` gives `ENOTDIR`; a missing path gives `ENOENT`; a symlink loop gives `ELOOP`; an unreadable `pathname` gives `EFAULT`.
- T2 (covers F2, F17): `statx(fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS, ...)` on regular files and pipes installed at descriptors 0 to 2 and their duplicates reports the same inode, device and file type as `fstat` and `newfstatat`. A NULL `pathname` with `AT_EMPTY_PATH` matches on Linux 6.11 and later; without the flag an empty path gives `ENOENT`.
  Existing candidate: `check_descriptor_stat_routes` in `tests/c/fixtures/stdio_inode_identity.c:46`, driven by `stdio_inode_identity_guest` (`hermit-cli/tests/cli.rs:533`) in the test `run_kvm_preserves_closed_standard_input` (`hermit-cli/tests/cli.rs:4254`), which runs it on ptrace among others (`hermit-cli/tests/cli.rs:4325`).
- T3 (covers F3, F4): on a symbolic link, `statx` without flags reports the target and with `AT_SYMLINK_NOFOLLOW` reports the link (`S_IFLNK`, different virtual inode). `AT_NO_AUTOMOUNT` on an ordinary directory returns the same result as flags 0.
- T4 (covers F5, F6, F9): `AT_STATX_FORCE_SYNC` and `AT_STATX_DONT_SYNC` alone succeed on a local file with the same result as flags 0; both together give `EINVAL`; an unknown `flags` bit gives `EINVAL`; `mask` with `STATX__RESERVED` gives `EINVAL`.
- T5 (covers F8): a NULL or read-only `statxbuf` gives `EFAULT`, and Hermit does not crash while reading the result back.
- T6 (covers F10, F11, F17, F18): `statx` with `STATX_ALL` on a regular file prints the inode, link count, blocks, whether `STATX_BTIME` is set, mtime and btime, and checks that the inode matches `stat`; the output must be identical across `--verify` runs, and mtime must still advance after a write.
  Existing candidate: `tests/c/stat_metadata_identity.c:222` (cell `c-programs/stat-metadata-identity`, `tests/e2e/manifests/c-programs.yaml:16625`, verify ci on ptrace).
- T7 (covers F2, F17): for an inherited standard stream (descriptor 0 redirected from a file by the parent), `statx(0, "", AT_EMPTY_PATH, ...)` and `fstat(0)` report the same inode. Expected to fail today by code reading.
- T8 (covers F12): `statx` with `STATX_MNT_ID` on `/` and on a file under a separate mount; if `stx_mask` has `STATX_MNT_ID`, `stx_mnt_id` must match the mount ID in `/proc/self/mountinfo`. Also request `STATX_MNT_ID_UNIQUE` and record the result.
- T9 (covers F13, F14, F15, F16): `statx` with `STATX_DIOALIGN`, `STATX_SUBVOL`, `STATX_WRITE_ATOMIC` and `STATX_DIO_READ_ALIGN` either clears each bit in `stx_mask` or returns a value that is identical across `--verify` runs; `stx_attributes` is a subset of `stx_attributes_mask`; a file made append-only with `chattr +a` (where the test can) reports the append-only attribute.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel resolves the path and errors; Detcore rewrites inode, device and times via determinize_stat and zero-fills unknown fields); linux_determinism conditional confirmed; 18 flag behaviours and 9 tests listed; open questions: by code reading STATX_MNT_ID stays set with stx_mnt_id 0, newer mask bits are dropped, statx on inherited stdio skips the fstat inode override; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and block size, launching host uid, birth-time support, inode attribute flags, kernel version); no U items; no open question resolved

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `statx`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+statx&body=Discussion+for+the+%60statx%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstatx.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60statx%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `statx`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Mount ID: should Detcore virtualize `stx_mnt_id` to agree with its `/proc/self/mountinfo`, or clear `STATX_MNT_ID` from `stx_mask`? Today, by code reading, the bit stays set and the value is 0.
- Newer fields: should the reverie `StatxMask` (and `DetStat`) learn `STATX_DIOALIGN`, `STATX_MNT_ID_UNIQUE`, `STATX_SUBVOL`, `STATX_WRITE_ATOMIC` and `STATX_DIO_READ_ALIGN`, passing the host-dependent alignment and atomic-write limits through, or is reporting them unsupported the intended policy?
- Should `handle_statx` apply the same inherited-stdio inode override as `fstat` (`detcore/src/syscalls/files.rs:3043`)? The fixture header calls this "a separate repair".
