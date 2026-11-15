---
schema: syscall-pages/v1
syscall: newfstatat
nr_x86_64: 262
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+newfstatat&body=Discussion+for+the+%60newfstatat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fnewfstatat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60newfstatat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 8
---

## Summary

`newfstatat(dirfd, pathname, statbuf, flags)` returns a file's metadata (`struct stat`) for a path relative to a directory descriptor, or for the descriptor itself with `AT_EMPTY_PATH`. It is the call glibc's `stat()`, `lstat()`, `fstatat()` and, on recent glibc, `fstat()` issue on x86-64. Detcore lets the kernel fill the buffer, then rewrites the inode, device and timestamps with virtual values (`handle_stat_family` at `detcore/src/syscalls/files.rs:3031`, at hermit f4055eb9cae4).

Manual reference: Linux man-pages fstatat(2).

## Linux semantics

The kernel resolves `pathname` like `openat` does (relative to `dirfd`, to the current directory for `AT_FDCWD`, or from the root when absolute), follows a final symlink unless told not to, and writes a `struct stat` to `statbuf`. The fields are the device (`st_dev`), inode (`st_ino`), type and mode bits (`st_mode`), link count, owner uid and gid, special-device number (`st_rdev`), size, preferred block size, allocated 512-byte blocks, and access, modification and status-change times with nanoseconds. It needs search permission on every directory in the path, but no permission on the file itself. It returns 0 or an error.

The kernel copies `pathname` before it writes `statbuf`, so the two may overlap.

### Flags and options

- F1: no flags with a path: stat the object the path names, following a final symlink, like `stat()`.
- F2: `AT_SYMLINK_NOFOLLOW` (0x100): if the final component is a symlink, describe the symlink itself, like `lstat()`. Magic links such as `/proc/self/fd/N` then report a symlink.
- F3: `AT_EMPTY_PATH` (0x1000) with an empty `pathname`: describe the object `dirfd` refers to, which may be any descriptor, including an `O_PATH` one, a pipe or a socket; with `AT_FDCWD` it describes the current directory. A non-empty `pathname` ignores the flag.
- F4: `AT_NO_AUTOMOUNT` (0x800): do not trigger an automount on the final component; describe the mount point instead. It has no effect where there is no automount point.
- F5: `dirfd`: `AT_FDCWD` resolves a relative path through the current directory; a directory descriptor resolves it from that directory; an absolute path ignores `dirfd`. A relative path with a closed `dirfd` gives `EBADF`, and with a non-directory `dirfd` gives `ENOTDIR`.
- F6: output fields: `st_ino` and `st_dev` identify the object, `st_atime`, `st_mtime` and `st_ctime` record its history, and the rest (`st_mode`, `st_nlink`, `st_uid`, `st_gid`, `st_rdev`, `st_size`, `st_blksize`, `st_blocks`) describe it now.
- F7: errors: an empty `pathname` without `AT_EMPTY_PATH` gives `ENOENT`; any other unknown flag bit gives `EINVAL`; `EFAULT` for a bad `pathname` or `statbuf` pointer; `ENOENT`, `ENOTDIR`, `EACCES` (search permission), `ELOOP`, `ENAMETOOLONG` and `EOVERFLOW` from path resolution and copy-out. Host `ENOMEM` is F8.
- F8: host-transient failure: `ENOMEM` when the kernel cannot allocate memory during path resolution, which depends on host memory pressure shared with other processes.

## Determinism analysis

`linux_determinism: conditional`. The call reads state; it changes nothing. It repeats when the same object is described in the same state, but several fields do not repeat across runs even for an unchanged file:

- `st_ino` comes from the host filesystem. A file created again in a fresh directory, or a pipe or socket, gets a different number each run (F3, F6).
- `st_dev` for procfs, sysfs, tmpfs and other anonymous mounts is a host-wide counter that drifts between runs (F6).
- `st_atime`, `st_mtime` and `st_ctime` are real time at the last access or change, so any file the run creates or touches carries times from that run (F6).
- `st_size`, `st_nlink` and `st_blocks` reflect the current contents, which other processes or earlier runs in the same directory may change; `st_blocks` also depends on the host filesystem's allocation (F6).
- Whether the path exists at all, and which error results, depend on filesystem state (F1, F7). `hermit-cli/tests/app_strict_verify.rs:65` records an example: a build's early `newfstatat("app")` gives `ENOENT` in one run and `S_IFREG` in the next, because the first run created the file.

### Cross-host variation

- Filesystem type and block size behind the path: `st_blksize` and `st_blocks`, and a directory's `st_size` and `st_nlink`, are the host filesystem's values, which Detcore passes through (`detcore/src/stat.rs:46`); btrfs, ext4 and tmpfs report different values for the same guest-created file or directory (F6).
- Host uid that launches Hermit: the user namespace maps that one uid to the guest's root, so the `st_uid` and `st_gid` of host files the guest can see (passed through raw) differ between hosts (F6).

## Hermit design

Target policy: `model`. The kernel resolves the path, checks permissions and reports errors. Detcore replaces the fields that would differ between runs with virtual values. At hermit f4055eb9cae4:

- The dispatch sends `Syscall::Newfstatat` to `handle_stat_family` (`detcore/src/lib.rs:2413`), which also serves `stat`, `lstat` and `fstat`. `newfstatat` is in the Determinized list (`detcore/src/syscall_classification.rs:137`).
- With metadata virtualization on (the default; `--no-virtualize-metadata` turns it off, `detcore-model/src/config.rs:247-253`), Detcore injects the call unchanged and lets the kernel produce every error (`detcore/src/syscalls/files.rs:3037-3041`), so F1 to F5 and F7 come from the kernel. It then reads the kernel's `struct stat` back from guest memory, rewrites it with `determinize_stat`, and writes it back (`detcore/src/syscalls/files.rs:3042-3056`).
- `determinize_stat` (`detcore/src/syscalls/files.rs:2981-3028`) maps the host inode to a virtual one that is assigned in first-seen order and never reused (`determinize_inode`, `detcore/src/tool_global.rs:3682`); maps `st_dev` to a virtual device (`determinize_device`, `detcore/src/tool_global.rs:3697`; tagged `TODO-HUMAN-REVIEW(PR-1056)` at `detcore/src/syscalls/files.rs:3008`); sets `st_atime` and `st_ctime` to the configured epoch; and sets `st_mtime` to a logical time that advances when Detcore sees the file change, so tools such as `make` still see modification order (`detcore/src/syscalls/files.rs:3021-3025`). `st_mode`, `st_nlink`, `st_uid`, `st_gid`, `st_rdev`, `st_size`, `st_blksize` and `st_blocks` keep the kernel's values.
- With metadata virtualization off, the call runs through `record_or_replay` and the guest sees host metadata (`detcore/src/syscalls/files.rs:3058-3059`).
- Detcore also synthesizes `newfstatat` itself, to read a file's state for `chown`, `lchown` and `fchownat` (`detcore/src/syscalls/files.rs:3403-3418`).

Why `model`: `passthrough` leaks host inodes, device numbers and real timestamps, which break `--verify` for any program that prints or hashes metadata. `emulate` would require Detcore to keep a full shadow filesystem. `refuse` is impossible: almost every program calls `stat`.

Gaps against `model`, by reading the code:

- The fixed inodes for inherited stdin, stdout and stderr (`deterministic_stdio_inode_for_resource`, `detcore/src/syscalls.rs:48-58`) apply only to `fstat` (`detcore/src/syscalls/files.rs:3043-3052`). `newfstatat(0, "", AT_EMPTY_PATH)` on an inherited stream gets a pooled virtual inode, so it can disagree with `fstat(0)` on the same object. The fixture comment at `tests/c/fixtures/stdio_inode_identity.c:7-8` calls this "a separate repair".
- `st_size`, `st_nlink` and `st_blocks` pass through, so allocation differences between host filesystems and directory sizes show up. `tests/e2e/manifests/language-runtimes.yaml:975` notes two cells that diverge on `newfstatat` directory metadata.
- The virtualized branch uses `guest.inject` rather than `record_or_replay` (`detcore/src/syscalls/files.rs:3041`), so it bypasses the record and replay path that the unvirtualized branch takes. Whether a replay then returns the recorded metadata is not established by reading this handler.

## Recommended tests

- T1 (covers F1, F6): create a file, write to it, `newfstatat(AT_FDCWD, path, &st, 0)`, write again and stat again. Print every field. Output must be identical across `hermit run --verify` on ptrace, `st_ino` and `st_dev` must be stable for the same file, and `st_mtime` must advance between the two stats.
- T2 (covers F2): stat a symlink with and without `AT_SYMLINK_NOFOLLOW`; the first gives `S_IFLNK` with the link's own virtual inode and the second the target's. Repeat for `/proc/self/fd/N`.
  Existing partial candidate: `tests/c/fixtures/stdio_inode_identity.c:108-116`, in the `complete-stat-routes` mode, which the CLI test `run_kvm_preserves_closed_standard_input` in `hermit-cli/tests/cli.rs` does not select.
- T3 (covers F3): `newfstatat(fd, "", &st, AT_EMPTY_PATH)` on a regular file, an `O_PATH` descriptor, a pipe, a socket, inherited stdin, and `AT_FDCWD`. Each must equal `fstat` on the same object (for `AT_FDCWD`, `stat(".")`), including for inherited stdio, which tests the gap above. Also check that a non-empty absolute path with `AT_EMPTY_PATH` describes the path, not `dirfd`, and that `pathname` overlapping `statbuf` works.
  Existing partial candidate: `tests/c/fixtures/stdio_inode_identity.c:46-72` and `tests/c/fixtures/stdio_inode_identity.c:235-249`, driven by `run_kvm_preserves_closed_standard_input` in `hermit-cli/tests/cli.rs` (mode `descriptor-reuse`, on ptrace and KVM, so not a ptrace-only target). It compares only files and pipes that replaced fds 0 to 2, not inherited streams.
- T4 (covers F4): `newfstatat(AT_FDCWD, "/", &st, AT_NO_AUTOMOUNT)` and on a path with no automount point; both succeed and match the call without the flag.
- T5 (covers F5): stat a relative name through `AT_FDCWD`, through a directory descriptor and through an `O_PATH | O_DIRECTORY` descriptor, and an absolute path with a bogus `dirfd`. A relative name with a closed `dirfd` gives `EBADF`, and with a regular-file `dirfd` gives `ENOTDIR`.
- T6 (covers F7): an empty path without `AT_EMPTY_PATH` (`ENOENT`), `dirfd = -1` with `AT_EMPTY_PATH` (`EBADF`), flag bit `0x40000000` (`EINVAL`), a `pathname` of `(char *)1` (`EFAULT`), a null `statbuf` (`EFAULT`), a missing path (`ENOENT`) and a path through a regular file (`ENOTDIR`).
  Existing partial candidate: `tests/c/fixtures/stdio_inode_identity.c:130-186` checks the first four, through the same CLI test.
- T7 (covers F6): run T1 with `--no-virtualize-metadata` and require that it still exits 0, recording that the metadata is then host values (a negative control for the virtualization).
- U1 (covers F8; host-transient): host `ENOMEM` depends on memory pressure shared with other processes. Detcore lets the kernel produce the error and hands it to the guest today (`detcore/src/syscalls/files.rs:3041`). F8 counts toward done only once Hermit hides this error or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel resolves and reports errors; determinize_stat rewrites inode, device and timestamps); linux_determinism stays conditional; 7 flag behaviours and 7 tests listed; gaps noted: fixed stdio inodes apply only to fstat, size/nlink/blocks pass through, virtualized branch bypasses record_or_replay; the existing stdio_inode_identity fixture runs on ptrace and KVM, so no ptrace-only coverage is recorded; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and block size, launching host uid); split host ENOMEM out of F7 into new F8 with U1 host-transient; no open questions section

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `newfstatat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+newfstatat&body=Discussion+for+the+%60newfstatat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fnewfstatat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60newfstatat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `newfstatat`: the 2026-07-24 classification campaign filed issues only for syscalls that Detcore did not yet handle, and Detcore already handled this one.
