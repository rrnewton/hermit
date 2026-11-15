---
schema: syscall-pages/v1
syscall: renameat
nr_x86_64: 264
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/401
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+renameat&body=Discussion+for+the+%60renameat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frenameat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60renameat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 4
---

## Summary

`renameat(olddirfd, oldpath, newdirfd, newpath)` is `rename` with each path resolved against its own directory descriptor. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no directory state of its own.

## Linux semantics

The kernel resolves `oldpath` against `olddirfd` and `newpath` against `newdirfd`. A relative path uses its descriptor's directory (or the working directory for `AT_FDCWD`); an absolute path ignores its descriptor. From there the call is `rename`: the kernel follows symlinks in every earlier component but never in the last component of either path, requires both paths to be on the same mount, locks both parents, checks the types of the two objects and performs the move as one step. No other process can see a state where `newpath` is missing, or where both names are gone. The raw syscall has no flags argument; `renameat2` (https://github.com/rrnewton/hermit/issues/402) is the variant with `RENAME_NOREPLACE`, `RENAME_EXCHANGE` and `RENAME_WHITEOUT`. It returns 0 or a negative errno and writes nothing to guest memory. Side effects show up only through other calls: both parents' mtime and ctime change, on most filesystems the moved inode's ctime changes, a moved directory's `..` is repointed, and a replaced inode loses one link. Errors are checked in path-walk order, so the errno is a function of the two descriptors, the two paths, the filesystem state, the credentials and the mounts.

### Flags and options

- F1: directory descriptors: `AT_FDCWD` resolves a relative path against the working directory; any other descriptor must be an open directory. A relative path with a descriptor that is not open: `EBADF`. A relative path with a descriptor that is open but not a directory: `ENOTDIR`. An absolute path ignores its descriptor. The two descriptors may differ, moving an entry between directories.
- F2: `newpath` does not exist: the entry is moved and `oldpath` disappears
- F3: `newpath` exists and both are non-directories: `newpath` is atomically replaced. The replaced inode loses a link and is freed only when no links and no open descriptors remain. A non-directory `oldpath` onto an existing directory `newpath`: `EISDIR`.
- F4: `oldpath` is a directory: `newpath` must be absent or an empty directory, which is replaced. A non-empty directory `newpath`: `ENOTEMPTY` (POSIX also allows `EEXIST`; Linux returns `ENOTEMPTY`). An existing non-directory `newpath`: `ENOTDIR`.
- F5: `oldpath` and `newpath` are hard links to the same inode: the call does nothing and returns 0, and both names remain
- F6: a symlink in the last component of either path is not followed: renaming a symlink moves the link itself, and a symlink at `newpath` is replaced rather than its target
- F7: `newpath` is inside the directory `oldpath` (a directory moved into its own subtree): `EINVAL`. A last component of `.` or `..` in either path: `EBUSY`. Either path is a mount point, or the root: `EBUSY`.
- F8: `oldpath` and `newpath` are on different mounts, including two bind mounts of one filesystem: `EXDEV`
- F9: trailing slashes are accepted only when `oldpath` is a directory. A trailing slash on either path when `oldpath` is not a directory: `ENOTDIR`.
- F10: `oldpath` is missing, either path is empty, or `newpath`'s parent is missing: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F11: write permission is denied on either parent, search permission on a path component, or write permission on a directory being moved to a new parent (to update its `..`): `EACCES`. A parent is sticky and the caller owns neither the parent nor the entry being removed or replaced: `EPERM`. F14 holds immutable and append-only entries.
- F12: a component is longer than `NAME_MAX`, or a path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in a prefix: `ELOOP`. Either path points outside the address space: `EFAULT`.
- F13: `EROFS`: either parent is on a read-only mount. F15 and F16 hold the other host-dependent failures.
- F14: the entry being moved, removed or replaced, or a parent, carries the immutable or append-only attribute, which only the host can set: `EPERM`.
- F15: host-transient failures: `ENOSPC` (the target directory needs a new block and the host filesystem is full), `EDQUOT` (a host quota is exceeded) and `EIO` (failing storage).
- F16: `EMLINK`: moving a directory into a parent whose link count is at the host filesystem's limit.

## Determinism analysis

The result is a function of the two directory descriptors, the filesystem state along both paths, the caller's credentials, and the mount layout. The descriptors themselves name directories the guest opened earlier, so they are guest state, not host state. Nothing in the call depends on timing. The kernel makes the move atomic with respect to other threads and processes, and Hermit runs threads one at a time anyway, so two guest threads racing on the same names get a fixed order. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it. This matches `rename` (https://github.com/rrnewton/hermit/issues/400), which is also `deterministic`.

### Nondeterminism sources

- Changes to the filesystem made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host resources and configuration behind F13, F14, F15 and F16: read-only remounts, inode attributes, free space, quota, filesystem link limits.
- Values the call changes but does not return: the parents' and the moved inode's timestamps. They become visible only through `stat` and similar calls, and are handled there.

### Cross-host variation

- Host mount layout visible in the guest: which paths are mount points (`EBUSY`, F7) and which pairs of paths lie on different mounts (`EXDEV`, F8).
- Filesystem type behind the guest's paths: its directory link limit decides where `EMLINK` starts (F16).
- Inode attributes set on the host: immutable or append-only entries give `EPERM` (F14).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `renameat` is classified `PassThrough` at `detcore/src/syscall_classification.rs:764`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:756` and `:757` gives the condition: "These synchronous extent and pathname operations are repeatable for guest-owned files in a fixed namespace with adequate space and no external mutation". `rename` is beside it at `detcore/src/syscall_classification.rs:762`, and `renameat2` is in the pass-through group at `detcore/src/syscall_classification.rs:732`.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::renameat` at `detcore/src/syscall_classification.rs:2063` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F13 the same way the guest's other path syscalls see them.
- The directory descriptors are guest descriptors that the kernel already holds; Detcore does not translate descriptor numbers for pass-through calls, and does not need to, because the guest's descriptor table is itself reproduced by the guest's earlier deterministic calls.
- Every other input that decides the result is either container state Hermit already holds fixed (the private `/tmp`, the launch filesystem image, fixed virtual-root credentials, serialized threads) or a host resource that no emulation could make up without lying about the filesystem.
- The return value carries no host-specific data: 0 or an errno that follows from the inputs.

Record and replay. Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore does not see `renameat`. `hermit record` has its own subscriptions, in a recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record captures `renameat`. No record/replay integration test issues `renameat` at this SHA.

Existing run-mode evidence, all on ptrace:

- The cargo integration test `deterministic_file_io_syscalls_verify` (`hermit-cli/tests/syscall_file_io.rs:51`) runs `tests/c/syscall_file_io.c` with `--backend=ptrace run --strict` and then `--verify`. That guest issues raw `syscall(SYS_renameat, AT_FDCWD, renamed, AT_FDCWD, renamed_at)` at `tests/c/syscall_file_io.c:157`, checks with `require_absent` that the source is gone (`tests/c/syscall_file_io.c:159`), and checks that the bytes survived under the new name (`tests/c/syscall_file_io.c:164`). The test asserts that the trace shows `inbound syscall: renameat(` (`hermit-cli/tests/syscall_file_io.rs:91` to `:95`). This covers F2 with `AT_FDCWD` and absolute paths only.
- `tests/c/rename_ops.c:86` calls glibc `renameat(rootfd, "b", subfd, "c")` with two real directory descriptors and checks that the source is gone and the content survived (`tests/c/rename_ops.c:88`). On this host's glibc 2.34 the wrapper issues `SYS_renameat` (host check under Additional information). Cell `c-programs/rename-ops` at `tests/e2e/manifests/c-programs.yaml:13408` runs it in verify mode with `ci: true`. This covers part of F1 and F2.

No coverage token is recorded yet: these tests cover F2 and part of F1, and the remaining behaviours have no test.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, call raw `syscall(SYS_renameat, ...)` with `AT_FDCWD` and relative paths, with two different real directory descriptors and relative paths, and with absolute paths and a deliberately invalid descriptor (which must be ignored). Assert the move and the content each time. Assert `EBADF` for a relative path with descriptor 999 and `ENOTDIR` for a relative path with a regular-file descriptor.
  Existing partial coverage: `tests/c/syscall_file_io.c:157` (raw, `AT_FDCWD`, absolute) and `tests/c/rename_ops.c:86` (glibc, two directory descriptors).
- T2 (covers F3): replace an existing regular file and assert that `newpath` has the old content and inode, and that a descriptor opened on the replaced file still reads its old content. Assert `EISDIR` for a file onto an existing directory.
- T3 (covers F4): rename a directory onto an absent name and onto an empty directory. Assert `ENOTEMPTY` onto a non-empty directory and `ENOTDIR` onto an existing file.
- T4 (covers F5, F6): rename between two hard links of one inode and assert 0 with both names present. Rename a symlink and assert with `lstat` that the link itself moved; rename a file onto a symlink and assert that the symlink, not its target, was replaced.
- T5 (covers F7, F9, F10): assert `EINVAL` for a directory into its own subdirectory; `EBUSY` for `.` and `..` as the last component; `ENOTDIR` for `"file/"`; `ENOENT` for a missing source, for `""` and for a missing destination parent; and `ENOTDIR` when a regular file is used as an intermediate component.
- T6 (covers F8): assert `EXDEV` between the private `/tmp` and another mount the guest can write, such as a separate tmpfs or bind mount. Record the chosen mounts in the test.
- T7 (covers F11): assert `EACCES` when the destination parent is not writable. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid. Assert `EPERM` for removing another owner's entry from a sticky directory.
- T8 (covers F12): assert `ENAMETOOLONG` for a 256-byte component and for a path over 4096 bytes, `ELOOP` for a symlink loop in a prefix, and `EFAULT` for `oldpath = (char *)1`.
- T9 (covers F13): assert `EROFS` under a read-only mount the guest can see. `ENOSPC`, `EDQUOT`, `EIO`, `EMLINK` and immutable-entry `EPERM` are F14 to F16, recorded by U1 and U2.
- U1 (covers F15; host-transient): `ENOSPC`, `EDQUOT` and `EIO` depend on the host filesystem's free space, quotas and storage health, shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F15 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F14, F16; host-config): the immutable and append-only attributes need `CAP_LINUX_IMMUTABLE` in the host's initial user namespace, which the guest lacks, and the directory link limit belongs to the host filesystem, so CI cannot produce these errors from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:764`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/401: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (classified at detcore/src/syscall_classification.rs:764, forwarded at detcore/src/lib.rs:2843; Hermit owns no directory state); listed 13 flag behaviours and 9 tests; related now names rename and renameat2; no coverage token recorded because the existing tests cover only F2 and part of F1, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host mount layout for EBUSY and EXDEV, filesystem link limit, host inode attributes); F11 narrowed, new F14 immutable or append-only EPERM; F13 narrowed to EROFS, new F15 ENOSPC/EDQUOT/EIO (host-transient U1) and F16 EMLINK; F14 and F16 host-config (U2); the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `renameat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+renameat&body=Discussion+for+the+%60renameat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frenameat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60renameat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: renameat" (https://github.com/rrnewton/hermit/issues/401), legacy variant A1.

### Open questions

- Timestamps changed by the move are outside this issue. They belong to the stat family tracking.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=renameat` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `renameat` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
renameat(AT_FDCWD, "/tmp/sysclass45/slr", AT_FDCWD, "/tmp/sysclass45/slr2") = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-10-03 glibc wrapper check

On the measurement host (Linux 7.1.3, glibc 2.34, x86_64), `strace` of a program calling glibc `renameat(dirfd, "a", dirfd, "b")` showed `renameat(3, "a", 3, "b") = 0`: the wrapper issues `SYS_renameat`, not `renameat2`.
