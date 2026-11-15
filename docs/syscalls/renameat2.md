---
schema: syscall-pages/v1
syscall: renameat2
nr_x86_64: 316
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/402
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+renameat2&body=Discussion+for+the+%60renameat2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frenameat2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60renameat2%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 3
---

## Summary

`renameat2(olddirfd, oldpath, newdirfd, newpath, flags)` renames or moves a directory entry. The flags select no-replace, atomic exchange or whiteout behaviour. Hermit forwards it to the host kernel unchanged. Given the same filesystem state inside the container, the outcome repeats.

## Linux semantics

The call was added in Linux 3.15. Each path is resolved relative to its own dirfd: `AT_FDCWD` means the current directory, and an absolute path ignores the dirfd. A symlink in the final component is renamed itself, not followed. The rename is atomic with respect to other lookups on the same filesystem. It changes no file data, updates ctime on the moved inode and mtime/ctime on the parent directories, and writes no memory in the caller.

### Flags and options

- F1: `flags == 0`: behaves like `renameat`. An existing `newpath` is replaced atomically. A directory can replace only an empty directory (`ENOTEMPTY` or `EEXIST` otherwise), and a non-directory cannot replace a directory (`EISDIR`) or the reverse (`ENOTDIR`). If both names refer to the same inode, the call does nothing and returns 0.
- F2: `RENAME_NOREPLACE` (1): fail with `EEXIST` if `newpath` exists. F9 holds filesystems without support.
- F3: `RENAME_EXCHANGE` (2): atomically swap the two entries. Both must exist (`ENOENT` otherwise), and they may be of different types, such as a file and a directory. F9 holds filesystems without support.
- F4: `RENAME_WHITEOUT` (4): after the move, leave a whiteout (a 0/0 character device) at `oldpath`. It is meant for overlay and union filesystems. F9 holds filesystems without support and older kernels.
- F5: invalid flag combinations: unknown bits, `NOREPLACE|EXCHANGE`, or `WHITEOUT|EXCHANGE` give `EINVAL`
- F6: path resolution: each dirfd is used for its relative path only. An absolute path ignores its dirfd. A dirfd that is not a directory gives `ENOTDIR`, and a bad dirfd gives `EBADF`. A final-component symlink is not followed. `ENOENT`, `ENAMETOOLONG`, `ELOOP` and `EACCES` follow the usual lookup rules.
- F7: structural errors: moving a directory into its own subtree, or naming `.` or `..` as the last component, gives `EINVAL` (or `EBUSY` for a mount point or `.`/`..` on some paths). Different mounts give `EXDEV`. A read-only filesystem gives `EROFS`. Sticky-directory rules give `EPERM`; F10 holds immutable and append-only entries.
- F8: host-transient failures: `ENOSPC` or `EDQUOT` when the target directory must grow and the host filesystem is full or over quota, `EIO` from failing storage, and `ENOMEM` from host memory pressure. F11 holds `EMLINK`.
- F9: a host filesystem that does not implement `RENAME_NOREPLACE`, `RENAME_EXCHANGE` or `RENAME_WHITEOUT` gives `EINVAL` for that flag. Before Linux 5.8 `RENAME_WHITEOUT` also needed `CAP_MKNOD` in the initial user namespace (`EPERM`).
- F10: the entry or a parent carries the immutable or append-only attribute, which only the host can set: `EPERM`.
- F11: `EMLINK` when a directory move would exceed the host filesystem's link limit.

## Determinism analysis

The outcome is a function of the guest's arguments and the directory tree visible to the guest, plus whether the host filesystem supports F2 to F4. With stable guest-visible filesystem state and no mutation from outside the container, which is Hermit's stated assumption for this group, each call repeats. Concurrent renames by guest threads are ordered by Hermit's scheduler, so their interleaving repeats too.

### Nondeterminism sources

- Mutation of the same directories from outside the container. Hermit's model excludes it.
- Filesystem support for `NOREPLACE`, `EXCHANGE` and `WHITEOUT` (F9), which is fixed for a given host configuration.
- Host storage failures (F8), which this set treats as nondeterminism sources without changing the rating (see mkdir).
- Inode numbers and timestamps are not a source here: the rename does not allocate an inode, and the stat family virtualizes the times (see Hermit design).

### Cross-host variation

- Filesystem type behind the guest's paths: whether `RENAME_NOREPLACE`, `RENAME_EXCHANGE` and `RENAME_WHITEOUT` work or give `EINVAL` (F2 to F4, F9), and the directory link limit that decides `EMLINK` (F11).
- Kernel version: before Linux 5.8 `RENAME_WHITEOUT` needs `CAP_MKNOD` and gives `EPERM` to the guest (F9).
- Host mount layout visible in the guest: which paths are mount points (`EBUSY`) and which pairs of paths lie on different mounts (`EXDEV`) (F7).
- Inode attributes set on the host: immutable or append-only entries give `EPERM` (F10).

## Hermit design

Target policy: `passthrough`. The code already does this. `renameat2` is classified `PassThrough` at `detcore/src/syscall_classification.rs:732`, under the group comment at `detcore/src/syscall_classification.rs:725`, which says these pathname operations "repeat given stable guest-visible filesystem state with no external mutation". Its neighbours `rename` (`detcore/src/syscall_classification.rs:762`) and `renameat` (`detcore/src/syscall_classification.rs:764`) are also passthrough. The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards the call, and `representative_policies_stay_in_their_reviewed_sections` asserts the classification at `detcore/src/syscall_classification.rs:2082`.

What the guest observes afterwards stays deterministic through other handlers:

- Virtual inode numbers are keyed by the raw host inode (`recv_determinize_inode`, `detcore/src/tool_global.rs:2522`). A renamed or exchanged file keeps its virtual inode, as it keeps its raw one on Linux.
- Directory listings are sorted before they reach the guest (`detcore/src/syscalls/files.rs:4582` for `getdents`, `detcore/src/syscalls/files.rs:4622` for `getdents64`), so the host's on-disk order after a rename does not leak.
- `determinize_stat` sets ctime and atime to the epoch (`detcore/src/syscalls/files.rs:3021`) and takes mtime from the logical per-inode clock (`detcore/src/syscalls/files.rs:3025`). The passthrough rename therefore does not change the guest-visible ctime of the moved inode or the mtime of the parent directories, where Linux would. The values are deterministic but not faithful; this is a stat-family concern shared by every passthrough pathname operation, not something to fix in `renameat2`.

`model` or `emulate` would mean reimplementing directory atomicity and the per-filesystem flag support the kernel already provides deterministically. `refuse` would break `mv`, editors, package managers and atomic-replace patterns.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, use raw `syscall(SYS_renameat2, ...)` to avoid glibc's wrapper rewriting `flags == 0` into `renameat`. Check that plain rename replaces an existing file, that `NOREPLACE` onto an existing target gives `EEXIST` with both files unchanged, that `NOREPLACE` onto a missing target succeeds, that `EXCHANGE` swaps a file and a directory, and that `EXCHANGE` with a missing target gives `ENOENT`. Also check that a rename onto the same inode returns 0, that a directory onto a non-empty directory gives `ENOTEMPTY`, and that a file onto a directory gives `EISDIR`.
  Existing partial coverage: `tests/c/renameat2_flags.c:67` (`NOREPLACE` gives `EEXIST`), `tests/c/renameat2_flags.c:73` (`NOREPLACE` to a missing target), `tests/c/renameat2_flags.c:79` (`EXCHANGE`) and `tests/c/renameat2_flags.c:85` (flags 0), through glibc's `renameat2()` (cell `c-programs/renameat2-flags`, `tests/e2e/manifests/c-programs.yaml:13470`, verify ci on ptrace). Related plain-rename coverage: `tests/c/rename_ops.c` (cell `c-programs/rename-ops`, `tests/e2e/manifests/c-programs.yaml:13408`) and `tests/c/syscall_file_io.c:157` (`SYS_renameat`).
- T2 (covers F4, F5): assert `EINVAL` for flags `8`, `NOREPLACE|EXCHANGE` and `WHITEOUT|EXCHANGE`. Try `RENAME_WHITEOUT` on the scratch filesystem and assert either success with a 0/0 character device left at `oldpath` (checked with `stat`) or the exact errno, `EINVAL` or `EPERM`, recorded alongside the filesystem type (those errnos are F9, recorded by U2).
- T3 (covers F6): rename between two `O_DIRECTORY` dirfds with relative paths. Use an absolute `newpath` with a bogus `newdirfd` (`-1`) and assert success. Assert `EBADF` for a bad dirfd with a relative path and `ENOTDIR` for a regular-file dirfd. Rename a dangling symlink and assert that the link moved, not its target.
- T4 (covers F7): assert `EINVAL` when moving a directory into its own child, `EXDEV` from the scratch filesystem to `/proc` or another mount visible in the container, and `EROFS` on a read-only bind mount if the harness provides one. This test no longer claims F8; F8 and F11 are recorded by U1 and U2.
- U1 (covers F8; host-transient): `ENOSPC`, `EDQUOT`, `EIO` and `ENOMEM` depend on the host's free space, quotas, storage health and memory, shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F8 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F9, F10, F11; host-config): flag support and the link limit belong to the host filesystem, the `CAP_MKNOD` rule to the host kernel version, and the immutable and append-only attributes need `CAP_LINUX_IMMUTABLE` in the host's initial user namespace, so CI cannot produce these errors from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:732`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/402: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (classified at detcore/src/syscall_classification.rs:732; virtual inodes follow the raw inode, getdents is sorted, stat times are virtualized); 8 flag behaviours and 4 tests listed; existing partial coverage is tests/c/renameat2_flags.c; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem flag support and link limit, kernel version for RENAME_WHITEOUT, host mount layout, host inode attributes); F2 to F4 narrowed, new F9 EINVAL/EPERM from missing support; F7 narrowed, new F10 immutable or append-only EPERM; F8 narrowed to host-transient errors (U1), new F11 EMLINK; F9 to F11 host-config (U2); T4 no longer claims F8; the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `renameat2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+renameat2&body=Discussion+for+the+%60renameat2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frenameat2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60renameat2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: renameat2" (https://github.com/rrnewton/hermit/issues/402), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=renameat2` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `renameat2` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
renameat2(AT_FDCWD, "/tmp/sysclass45/slr2", AT_FDCWD, "/tmp/sysclass45/slr3", 0) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
