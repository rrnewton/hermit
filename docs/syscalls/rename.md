---
schema: syscall-pages/v1
syscall: rename
nr_x86_64: 82
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/400
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rename&body=Discussion+for+the+%60rename%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frename.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rename%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 7
---

## Summary

`rename(oldpath, newpath)` moves a directory entry to a new name within one mount, atomically replacing an existing `newpath` when the types allow it. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no directory state of its own.

## Linux semantics

The kernel resolves the parents of both paths, following symlinks in every earlier component but never in the last component of either path. Both paths resolve relative to the caller's working directory. Both must be on the same mount. The kernel locks both parents and checks the types of the two objects, then performs the move as one step: no other process can see a state where `newpath` is missing, or where both names are gone. It returns 0 or a negative errno and writes nothing to guest memory. Side effects show up only through other calls: both parents' mtime and ctime change; on most filesystems the moved inode's ctime changes; a moved directory's `..` is repointed; and a replaced inode loses one link. Errors are checked in path-walk order, so the errno is a function of the two paths, the filesystem state, the credentials and the mounts.

### Flags and options

- F1: `newpath` does not exist: the entry is moved and `oldpath` disappears
- F2: `newpath` exists and both are non-directories: `newpath` is atomically replaced. The replaced inode loses a link and is freed only when no links and no open descriptors remain. A non-directory `oldpath` onto an existing directory `newpath`: `EISDIR`.
- F3: `oldpath` is a directory: `newpath` must be absent or an empty directory, which is replaced. A non-empty directory `newpath`: `ENOTEMPTY` (POSIX also allows `EEXIST`; Linux returns `ENOTEMPTY`). An existing non-directory `newpath`: `ENOTDIR`.
- F4: `oldpath` and `newpath` are hard links to the same inode: the call does nothing and returns 0, and both names remain
- F5: a symlink in the last component of either path is not followed: renaming a symlink moves the link itself, and a symlink at `newpath` is replaced rather than its target
- F6: `newpath` is inside the directory `oldpath` (a directory moved into its own subtree): `EINVAL`. A last component of `.` or `..` in either path: `EBUSY`. Either path is a mount point, or the root: `EBUSY`.
- F7: `oldpath` and `newpath` are on different mounts, including two bind mounts of one filesystem: `EXDEV`
- F8: trailing slashes are accepted only when `oldpath` is a directory. A trailing slash on either path when `oldpath` is not a directory: `ENOTDIR`.
- F9: `oldpath` is missing, either path is empty, or `newpath`'s parent is missing: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F10: write permission is denied on either parent, search permission on a path component, or write permission on a directory being moved to a new parent (to update its `..`): `EACCES`. A parent is sticky and the caller owns neither the parent nor the entry being removed or replaced: `EPERM`. F13 holds immutable and append-only entries.
- F11: a component is longer than `NAME_MAX`, or a path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in a prefix: `ELOOP`. Either path points outside the address space: `EFAULT`.
- F12: `EROFS`: either parent is on a read-only mount. F14 and F15 hold the other host-dependent failures.
- F13: the entry being moved, removed or replaced, or a parent, carries the immutable or append-only attribute, which only the host can set: `EPERM`.
- F14: host-transient failures: `ENOSPC` (the target directory needs a new block and the host filesystem is full), `EDQUOT` (a host quota is exceeded) and `EIO` (failing storage).
- F15: `EMLINK`: moving a directory into a parent whose link count is at the host filesystem's limit.

## Determinism analysis

The result is a function of the filesystem state along both paths, the caller's credentials, and the mount layout. Nothing in the call depends on timing. The kernel makes the move atomic with respect to other threads and processes, and Hermit runs threads one at a time anyway, so two guest threads racing on the same names get a fixed order. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it.

### Nondeterminism sources

- Changes to the filesystem made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host resources and configuration behind F12, F13, F14 and F15: read-only remounts, inode attributes, free space, quota, filesystem link limits.
- Values the call changes but does not return: the parents' and the moved inode's timestamps. They become visible only through `stat` and similar calls, and are handled there.

### Cross-host variation

- Host mount layout visible in the guest: which paths are mount points (`EBUSY`, F6) and which pairs of paths lie on different mounts (`EXDEV`, F7).
- Filesystem type behind the guest's paths: its directory link limit decides where `EMLINK` starts (F15).
- Inode attributes set on the host: immutable or append-only entries give `EPERM` (F13).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `rename` is classified `PassThrough` at `detcore/src/syscall_classification.rs:762`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:756` and `:757` gives the condition: "These synchronous extent and pathname operations are repeatable for guest-owned files in a fixed namespace with adequate space and no external mutation". `renameat` sits beside it at `detcore/src/syscall_classification.rs:764`, and `renameat2` is in the pass-through group at `detcore/src/syscall_classification.rs:732`.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::rename` at `detcore/src/syscall_classification.rs:2062` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F12 the same way the guest's other path syscalls see them.
- Every input that decides the result is either container state Hermit already holds fixed (the private `/tmp`, the launch filesystem image, fixed virtual-root credentials, serialized threads) or a host resource that no emulation could make up without lying about the filesystem.
- The return value carries no host-specific data: 0 or an errno that follows from the inputs.

Record and replay. Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore does not see `rename`. `hermit record` has its own subscriptions, in a recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record captures `rename`. The integration test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) requires the raw `SYS_rename` calls at `tests/c/record_replay_path_queries.c:238` (success) and `:246` (`ENOENT`) to replay from the recording.

Existing run-mode evidence, all on ptrace:

- The cargo integration test `deterministic_file_io_syscalls_verify` (`hermit-cli/tests/syscall_file_io.rs:51`) runs `tests/c/syscall_file_io.c` with `--backend=ptrace run --strict` and then `--verify`. That guest issues raw `syscall(SYS_rename, original, renamed)` at `tests/c/syscall_file_io.c:154` and checks with `require_absent` that the source is gone (`tests/c/syscall_file_io.c:155`). The test asserts that the trace shows `inbound syscall: rename(` (`hermit-cli/tests/syscall_file_io.rs:91` to `:95`). This covers only F1, with absolute paths.
- `tests/c/rename_ops.c:69` (move to an absent name), `:76` (replace an existing file) and `:103` (missing source, `ENOENT`) call glibc `rename()`, which issues `SYS_rename` on this host's glibc 2.34 (host strace under Additional information). Cell `c-programs/rename-ops` at `tests/e2e/manifests/c-programs.yaml:13408` runs it in verify mode with `ci: true` and ptrace enabled.

No coverage token is recorded yet: these tests cover F1, part of F2 and part of F9, and the remaining behaviours have no test.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, `chdir` into a fresh directory and call raw `syscall(SYS_rename, ...)` with relative paths. Move a file to an absent name. Replace an existing file while a descriptor holds the replaced file open: reading that descriptor must still return the old bytes, and its `fstat` must report `st_nlink` 0. Assert `EISDIR` for a file onto an existing directory.
  Existing partial coverage: `hermit-cli/tests/syscall_file_io.rs:51` (raw `SYS_rename` at `tests/c/syscall_file_io.c:154`, absent destination) and `tests/c/rename_ops.c:69` and `:76` (glibc; cell `c-programs/rename-ops`).
- T2 (covers F3): move a directory to an absent name and onto an empty directory (replaced). Assert `ENOTEMPTY` onto a non-empty directory and `ENOTDIR` onto a regular file.
- T3 (covers F4, F5): rename one hard link onto the other: assert 0, both names present, and `st_nlink` 2. Rename a symlink and a dangling symlink and check with `lstat` that the link moved. Rename a file onto a symlink and check that the symlink was replaced and its target is untouched.
- T4 (covers F6, F7): assert `EINVAL` for `rename("d", "d/sub/x")`, and `EBUSY` for `rename(".", "x")` and for `rename("x", "..")`. Assert `EBUSY` when either path is a guest-visible mount point. Assert `EXDEV` between two distinct guest-visible mounts. Record the chosen paths in the test.
- T5 (covers F8, F9): assert that `rename("dir/", "dir2/")` succeeds and that a trailing slash on a regular file gives `ENOTDIR`. Assert `ENOENT` for a missing source, for `""`, and for a missing parent of `newpath`, and `ENOTDIR` when a regular file is used as an intermediate component.
  Existing partial coverage: `tests/c/rename_ops.c:103` (glibc, missing source) and the record-mode `tests/c/record_replay_path_queries.c:246`.
- T6 (covers F10): assert `EACCES` for a move out of a directory the guest cannot write. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid. Assert `EPERM` for removing another owner's entry from a sticky directory, using an entry and directory owned by unmapped uids.
- T7 (covers F11): assert `ENAMETOOLONG` for a 256-byte component and for a path over 4096 bytes in each argument, `ELOOP` for a symlink loop in a prefix, and `EFAULT` for `oldpath = (char *)1` and for `newpath = (char *)1`.
- T8 (covers F12): assert `EROFS` under a read-only mount the guest can see. `ENOSPC`, `EDQUOT`, `EIO`, `EMLINK` and immutable-entry `EPERM` are F13 to F15, recorded by U1 and U2.
- U1 (covers F14; host-transient): `ENOSPC`, `EDQUOT` and `EIO` depend on the host filesystem's free space, quotas and storage health, shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F14 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F13, F15; host-config): the immutable and append-only attributes need `CAP_LINUX_IMMUTABLE` in the host's initial user namespace, which the guest lacks, and the directory link limit belongs to the host filesystem, so CI cannot produce these errors from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:762`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/400: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (the code forwards at detcore/src/lib.rs:2843, and Hermit owns no directory state); listed 12 flag behaviours and 8 tests; existing ptrace evidence (hermit-cli/tests/syscall_file_io.rs, c-programs/rename-ops) covers only F1 and parts of F2 and F9, so no coverage token is recorded and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host mount layout for EBUSY and EXDEV, filesystem link limit, host inode attributes); F10 narrowed, new F13 immutable or append-only EPERM; F12 narrowed to EROFS, new F14 ENOSPC/EDQUOT/EIO (host-transient U1) and F15 EMLINK; F13 and F15 host-config (U2); the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rename`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rename&body=Discussion+for+the+%60rename%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frename.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rename%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: rename" (https://github.com/rrnewton/hermit/issues/400), legacy variant A1.

Legacy text replaced in this pass: Summary "Renames/moves a file within a filesystem."; Linux semantics "Atomic replace within a filesystem. Verified: renaming a /tmp file returned 0."; Determinism analysis "Deterministic given fixed FS state; an FS mutation passed through (hermit does not determinize a changing filesystem)." with the source "None from the syscall; only external concurrent FS mutation." Every claim is kept in the sections above.

### Open questions

- Timestamps that `rename` changes are outside this issue. They belong to the stat family tracking.

### 2026-10-03 host strace evidence

On this host (glibc 2.34, x86_64), a scratch probe calling glibc `rename()` under `strace -f` showed the raw syscall, so the glibc-based tests above do exercise `SYS_rename`:

```
rename("a", "b")                        = 0
```

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=rename` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `rename` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
rename("/tmp/sysclass45/sl", "/tmp/sysclass45/slr") = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
