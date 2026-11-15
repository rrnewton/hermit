---
schema: syscall-pages/v1
syscall: rmdir
nr_x86_64: 84
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/405
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rmdir&body=Discussion+for+the+%60rmdir%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frmdir.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rmdir%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 1
---

## Summary

`rmdir(pathname)` removes an empty directory. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no directory state of its own.

## Linux semantics

The kernel looks up the parent of the last path component, following symlinks in every earlier component. The last component must name a directory and is never followed if it is a symlink. The directory must contain no entries besides `.` and `..`. On success the entry disappears from its parent, the parent's mtime and ctime change, and on most filesystems the parent's link count drops by one. The call returns 0 or a negative errno and writes nothing to guest memory. A process whose working directory, or an open descriptor, still refers to the removed directory keeps that reference, but the directory can no longer gain entries. Errors are checked in path-walk order, so the errno is a function of the path, the filesystem state, the credentials and the mounts.

### Flags and options

- F1: an empty directory is removed. If it is a process's working directory or is held open, those references stay valid, but creating an entry inside it then fails with `ENOENT`.
- F2: the directory contains entries other than `.` and `..`: `ENOTEMPTY` (POSIX also allows `EEXIST`; Linux returns `ENOTEMPTY`)
- F3: the last component is not a directory, including a symlink to a directory, which is not followed: `ENOTDIR`
- F4: a last component of `.`: `EINVAL`. A last component of `..`: `ENOTEMPTY`. The process's root directory (for example `/`): `EBUSY`.
- F5: the directory is a mount point: `EBUSY`
- F6: trailing slashes are accepted: `rmdir("d/")` removes `d`
- F7: the directory is missing, an intermediate component is missing, or `pathname` is empty: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F8: write or search permission is denied on the parent, or search permission on a path component: `EACCES`. The parent is sticky and the caller owns neither the parent nor the directory: `EPERM`. F12 holds immutable and append-only directories.
- F9: a component is longer than `NAME_MAX`, or the path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in the prefix: `ELOOP`. `pathname` points outside the address space: `EFAULT`.
- F10: `EROFS`: the parent is on a read-only mount. F13 holds `EIO`.
- F11: a relative `pathname` resolves against the calling process's working directory, inside its root and mount namespace
- F12: the directory or its parent carries the immutable or append-only attribute, which only the host can set: `EPERM`.
- F13: `EIO` from failing host storage.

## Determinism analysis

The result is a function of the filesystem state along the path, the caller's credentials, and the mount layout. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so a guest thread that creates an entry while another removes the directory gets a fixed order. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it.

### Nondeterminism sources

- Changes to the filesystem made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host configuration behind F10, F12 and F13: read-only remounts, inode attributes and device errors.
- Values the call changes but does not return: the parent's timestamps and link count. They become visible only through `stat` and similar calls, and are handled there.

### Cross-host variation

- Host mount layout visible in the guest: which directories are mount points and give `EBUSY` (F5).
- Inode attributes set on the host: immutable or append-only directories give `EPERM` (F12).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `rmdir` is classified `PassThrough` at `detcore/src/syscall_classification.rs:733`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:725` and `:726` gives the condition: these calls "repeat given stable guest-visible filesystem state with no external mutation".
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::rmdir` at `detcore/src/syscall_classification.rs:2083` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F11 the same way the guest's other path syscalls see them.
- Every input that decides the result is either container state Hermit already holds fixed (the private `/tmp`, the launch filesystem image, fixed virtual-root credentials, serialized threads) or a host condition that no emulation could make up without lying about the filesystem.
- The return value carries no host-specific data: 0 or an errno that follows from the inputs. Detcore keeps no virtual working directory, so removing the current directory (F1) needs no bookkeeping: `fchdir` and `getcwd` are passed through too.

Record and replay. Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore does not see `rmdir`. `hermit record` has its own subscriptions, in a recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record captures `rmdir`. The integration test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) requires the raw `SYS_rmdir` calls at `tests/c/record_replay_path_queries.c:245` and `:261` to replay. The test `record_refuses_a_removed_working_directory` (`hermit-cli/tests/record_replay.rs:2200`) runs the same guest with `--removed-cwd`, which removes its own working directory with raw `SYS_rmdir` at `tests/c/record_replay_path_queries.c:111` and then calls `fchdir` on it; recording must refuse that `fchdir`, not the `rmdir`.

Existing run-mode evidence, all on ptrace and through glibc `rmdir()`, which issues `SYS_rmdir` on this host's glibc 2.34 (host strace under Additional information):

- `tests/c/mkdir_rmdir.c:84` (`ENOTEMPTY`), `:90` (child removed) and `:92` (now-empty parent removed). Cell `c-programs/mkdir-rmdir` at `tests/e2e/manifests/c-programs.yaml:12575`.
- `tests/c/errno_path_identity.c:93` (`rmdir` of a missing path). Cell `system-utils/errno-path-identity` at `tests/e2e/manifests/system-utils.yaml:1777`. It prints the errno and fails only on an unexpected success, so it checks that runs agree, not that the errno is `ENOENT`.

No coverage token is recorded yet: these tests cover F1 and F2 with absolute paths and part of F7, and the remaining behaviours have no test.

## Recommended tests

- T1 (covers F1, F11): under `hermit run --verify` on ptrace, `chdir` into a fresh directory and call raw `syscall(SYS_rmdir, ...)` with relative paths. Remove an empty directory and assert that `stat` then gives `ENOENT`. Remove the current working directory after opening it with `O_DIRECTORY`: assert that `rmdir` returns 0, that `fchdir` on the descriptor still succeeds, and that `mkdir("x")` inside it gives `ENOENT`.
  Existing partial coverage: `tests/c/mkdir_rmdir.c:90` and `:92` (glibc, absolute paths; cell `c-programs/mkdir-rmdir`).
- T2 (covers F2, F3): assert `ENOTEMPTY` for a directory holding a file, and for one holding only an empty subdirectory. Assert `ENOTDIR` for a regular file and for a symlink to a directory, and check with `lstat` that the symlink and its target both survive.
  Existing partial coverage: `tests/c/mkdir_rmdir.c:84` (glibc, `ENOTEMPTY`).
- T3 (covers F4, F5): assert `EINVAL` for `rmdir("d/.")`, `ENOTEMPTY` for `rmdir("d/..")` where the parent is not empty, and `EBUSY` for `rmdir("/")`. Assert `EBUSY` for an empty directory that is a guest-visible mount point. Record the chosen path in the test.
- T4 (covers F6, F7): assert that `rmdir("d/")` removes `d`. Assert `ENOENT` for a missing directory, for a missing parent and for `""`, and `ENOTDIR` when a regular file is used as an intermediate component.
  Existing partial coverage: `tests/c/errno_path_identity.c:93` (glibc, missing path; agreement only).
- T5 (covers F8): assert `EACCES` for removing a child of a directory the guest cannot write. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid. Assert `EPERM` for removing another owner's empty directory from a sticky directory, using a directory and parent owned by unmapped uids.
- T6 (covers F9): assert `ENAMETOOLONG` for a 256-byte component and for a path over 4096 bytes, `ELOOP` for a symlink loop in the prefix, and `EFAULT` for `pathname = (char *)1`.
- T7 (covers F10): assert `EROFS` for removing an empty directory under a read-only mount the guest can see. `EIO` and immutable-directory `EPERM` are F13 and F12, recorded by U1 and U2.
- U1 (covers F13; host-transient): `EIO` depends on the health of host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands it to the guest today. F13 counts toward done only once Hermit hides it or reports it as an infrastructure error.
- U2 (covers F12; host-config): the immutable and append-only attributes need `CAP_LINUX_IMMUTABLE` in the host's initial user namespace, which the guest lacks, so CI cannot produce this error from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:733`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/405: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (the code forwards at detcore/src/lib.rs:2843, and Hermit owns no directory state); listed 11 flag behaviours and 7 tests; existing ptrace evidence (c-programs/mkdir-rmdir, system-utils/errno-path-identity) uses glibc rmdir with absolute paths and covers only F1, F2 and part of F7, so no coverage token is recorded and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host mount layout for EBUSY, host inode attributes); F8 narrowed, new F12 immutable or append-only EPERM (host-config U2); F10 narrowed to EROFS, new F13 EIO (host-transient U1); the open question on the unreachable EIO is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rmdir`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rmdir&body=Discussion+for+the+%60rmdir%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frmdir.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rmdir%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: rmdir" (https://github.com/rrnewton/hermit/issues/405), legacy variant A1.

Legacy text replaced in this pass: Summary "Removes an empty directory."; Linux semantics "Fails ENOTEMPTY if non-empty. Verified: removing an empty /tmp directory returned 0."; Determinism analysis "Deterministic given fixed FS state; an FS mutation passed through." with the source "None from the syscall; only external concurrent FS mutation." Every claim is kept in the sections above.

### 2026-10-03 host strace evidence

On this host (glibc 2.34, x86_64), a scratch probe calling glibc `rmdir()` under `strace -f` showed the raw syscall, so the glibc-based tests above do exercise `SYS_rmdir`:

```
rmdir("dd")                             = 0
```

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=rmdir` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `rmdir` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
rmdir("/tmp/sysclass45/rd")     = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
