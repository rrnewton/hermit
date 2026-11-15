---
schema: syscall-pages/v1
syscall: unlink
nr_x86_64: 87
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/505
  - https://github.com/rrnewton/hermit/issues/526
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+unlink&body=Discussion+for+the+%60unlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Funlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60unlink%60%2C+or+in+what+the+page+says%3F%0A
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

`unlink(pathname)` removes a directory entry by path. When it was the inode's last link and no process still holds the file open or mapped, the file itself is freed. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no directory state of its own.

Manual reference: [unlink(2)](https://man7.org/linux/man-pages/man2/unlink.2.html).

## Linux semantics

The kernel looks up the parent of the last path component, following symlinks in every earlier component. The last component is never followed: if it is a symlink, the symlink is removed. It must not be a directory. On success the entry disappears, the inode's link count drops by one and its ctime changes, and the parent's mtime and ctime change. Open file descriptions and memory mappings keep the inode alive, readable and writable until the last of them is closed; `fstat` on such a descriptor then reports `st_nlink` 0. The call returns 0 or a negative errno and writes nothing to guest memory. Errors are checked in path-walk order, so the errno is a function of the path, the filesystem state, the credentials and the mounts.

### Flags and options

- F1: a name of a regular file, FIFO, socket or device node is removed and the inode's `st_nlink` drops by one. Other hard links to the inode keep working.
- F2: the last link is removed while a descriptor or mapping still refers to the inode: the data stays readable and writable through it, `fstat` reports `st_nlink` 0, and the storage is freed at the final close or unmap
- F3: the last component is a symlink, dangling or not: the symlink itself is removed and its target is untouched
- F4: the last component is a directory, including `.` and `..` and a path with a trailing slash that names a directory: `EISDIR` (Linux; POSIX specifies `EPERM`). A trailing slash on a non-directory: `ENOTDIR`.
- F5: the entry is missing, an intermediate component is missing, or `pathname` is empty: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F6: write or search permission is denied on the parent, or search permission on a path component: `EACCES`. The parent is sticky and the caller owns neither the parent nor the entry: `EPERM`. F11 holds immutable and append-only inodes.
- F7: the entry is a mount point (a file bind-mounted over): `EBUSY`
- F8: a component is longer than `NAME_MAX`, or the path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in the prefix: `ELOOP`. `pathname` points outside the address space: `EFAULT`.
- F9: `EROFS`: the parent is on a read-only mount. F12 and F13 hold the other host-dependent failures.
- F10: a relative `pathname` resolves against the calling process's working directory, inside its root and mount namespace
- F11: the inode or its parent carries the immutable or append-only attribute, which only the host can set: `EPERM`.
- F12: `EIO` from failing host storage.
- F13: a refusal from a host security module (SELinux, AppArmor or similar): `EACCES` or `EPERM`.

## Determinism analysis

The result is a function of the filesystem state along the path, the caller's credentials, and the mount layout. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so two guest threads unlinking or recreating the same name get a fixed order. Open references (F2) are guest state too: whether a descriptor still holds the inode is decided by the guest's own earlier calls. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it.

Reconciled with the duplicate https://github.com/rrnewton/hermit/issues/526, which classified `unlink` as CONDITIONAL. This body keeps `deterministic`. The duplicate's sources are directory permissions, path state, open references, mounts, security policy and concurrent filesystem mutation. Permissions, path state, open references and mounts are guest-visible filesystem and process state, which is exactly what the result is a function of. Concurrent mutation by guest threads is ordered by Hermit, and mutation from outside the container is the stable-filesystem assumption shared by every path call. Security policy is host-wide configuration that applies to every path call alike and does not change between runs on one host; it is listed under F9 and below. None of these is specific to `unlink` in the way `fs.protected_hardlinks` is specific to `link`, so the classification matches `mkdir`, `rmdir` and `rename`.

### Nondeterminism sources

- Changes to the filesystem made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host configuration behind F9, F11, F12 and F13: read-only remounts, inode attributes, device errors, and host security-module policy (SELinux, AppArmor or similar), which can refuse a path call independently of the container's filesystem image.
- When storage is actually freed (F2) and values the call changes but does not return: the inode's link count and ctime, and the parent's timestamps. They become visible only through `fstat`, `stat`, `statfs` and similar calls, and are handled there.

### Cross-host variation

- Host mount layout visible in the guest: which files are bind-mount targets and give `EBUSY` (F7).
- Inode attributes set on the host: immutable or append-only inodes give `EPERM` (F11).
- Host security-module policy (SELinux, AppArmor or similar): which unlinks are refused (F13).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `unlink` is classified `PassThrough` at `detcore/src/syscall_classification.rs:735`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:725` and `:726` gives the condition: these calls "repeat given stable guest-visible filesystem state with no external mutation". `unlinkat` is beside it at `detcore/src/syscall_classification.rs:736`.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::unlink` at `detcore/src/syscall_classification.rs:2091` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F10 the same way the guest's other path syscalls see them. F2 in particular depends on the kernel's own count of open references, which an emulation would have to duplicate.
- Every input that decides the result is either container state Hermit already holds fixed (the private `/tmp`, the launch filesystem image, fixed virtual-root credentials, serialized threads) or a host condition that no emulation could make up without lying about the filesystem.
- The return value carries no host-specific data: 0 or an errno that follows from the inputs.

Record and replay. Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore does not see `unlink`. `hermit record` has its own subscriptions, in a recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record captures `unlink`. The record-mode guest `tests/c/record_replay_path_queries.c` calls glibc `unlink` at `:256` to `:259` only to clean up and does not check the result.

Existing run-mode evidence, all on ptrace and through glibc `unlink()`, which issues `SYS_unlink` on this host's glibc 2.34 (host strace under Additional information):

- The cargo integration test `deterministic_file_io_syscalls_verify` (`hermit-cli/tests/syscall_file_io.rs:51`) runs `tests/c/syscall_file_io.c` with `--backend=ptrace run --strict` and then `--verify`. That guest removes a symlink and a regular file at `tests/c/syscall_file_io.c:181` and `:182` and checks with `require_absent` that both are gone (`:183`, `:184`). The test's trace assertion does not include `unlink`.
- The cargo integration test `deterministic_file_metadata_syscalls_verify` (`hermit-cli/tests/syscall_file_metadata.rs:51`) removes a symlink, a hard link and the file at `tests/c/syscall_file_metadata.c:224` to `:226` and requires 0 for each.
- `tests/c/path_file_ops.c:114` (cell `c-programs/path-file-ops`, `tests/e2e/manifests/c-programs.yaml:12961`) removes a file and fails on error.
- `tests/c/errno_path_identity.c:92` (cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1777`) unlinks a missing path. It prints the errno and fails only on an unexpected success, so it checks that runs agree, not that the errno is `ENOENT`.

No coverage token is recorded yet: these tests cover F1 and F3 with absolute paths and part of F5, and the remaining behaviours have no test. `tests_naming` is 0: no test names `SYS_unlink`.

## Recommended tests

- T1 (covers F1, F3, F10): under `hermit run --verify` on ptrace, `chdir` into a fresh directory and call raw `syscall(SYS_unlink, ...)` with relative paths. Remove one of two hard links and assert that the other's `st_nlink` drops to 1 and its data is unchanged. Remove a FIFO, a symlink and a dangling symlink, and check with `lstat` that each is gone while the symlink's target remains.
  Existing partial coverage: `hermit-cli/tests/syscall_file_io.rs:51` (glibc at `tests/c/syscall_file_io.c:181` and `:182`, a symlink and a file) and `hermit-cli/tests/syscall_file_metadata.rs:51` (glibc at `tests/c/syscall_file_metadata.c:224` to `:226`).
- T2 (covers F2): open a file and `mmap` it, unlink its only name, then write and read through the descriptor and the mapping. Assert that the data round-trips, that `fstat` reports `st_nlink` 0, and that the name is gone.
- T3 (covers F4): assert `EISDIR` for a directory, for `"d/."`, for `"d/.."` and for `"d/"`, and `ENOTDIR` for `"file/"`.
- T4 (covers F5): assert `ENOENT` for a missing name, for a missing parent and for `""`, and `ENOTDIR` when a regular file is used as an intermediate component.
  Existing partial coverage: `tests/c/errno_path_identity.c:92` (glibc, missing path; agreement only).
- T5 (covers F6, F7): assert `EACCES` for removing a file from a directory the guest cannot write. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid. Assert `EPERM` for removing another owner's file from a sticky directory, using a file and directory owned by unmapped uids. Assert `EBUSY` for a file that is a guest-visible bind-mount target. Record the chosen paths in the test.
- T6 (covers F8): assert `ENAMETOOLONG` for a 256-byte component and for a path over 4096 bytes, `ELOOP` for a symlink loop in the prefix, and `EFAULT` for `pathname = (char *)1`.
- T7 (covers F9): assert `EROFS` for removing a file under a read-only mount the guest can see. Immutable-inode `EPERM`, `EIO` and security-module refusals are F11 to F13, recorded by U1 and U2.
- U1 (covers F12; host-transient): `EIO` depends on the health of host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands it to the guest today. F12 counts toward done only once Hermit hides it or reports it as an infrastructure error.
- U2 (covers F11, F13; host-config): the immutable and append-only attributes need `CAP_LINUX_IMMUTABLE` in the host's initial user namespace, which the guest lacks, and security-module policy is set by the host, so CI cannot produce these errors from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:735`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/505: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/526 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (the code forwards at detcore/src/lib.rs:2843, and Hermit owns no directory state); merged duplicate https://github.com/rrnewton/hermit/issues/526 and rejected its CONDITIONAL, because its sources are guest filesystem state, Hermit-ordered threads or the shared stable-host assumption; listed 10 flag behaviours and 7 tests; existing ptrace evidence (hermit-cli/tests/syscall_file_io.rs, hermit-cli/tests/syscall_file_metadata.rs, c-programs/path-file-ops) uses glibc unlink and covers only F1, F3 and part of F5, so no coverage token is recorded and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host mount layout for EBUSY, host inode attributes, host security-module policy); F6 narrowed, new F11 immutable or append-only EPERM; F9 narrowed to EROFS, new F12 EIO (host-transient U1) and F13 security-module refusal; F11 and F13 host-config (U2); the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `unlink`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+unlink&body=Discussion+for+the+%60unlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Funlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60unlink%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: unlink" (https://github.com/rrnewton/hermit/issues/505), legacy variant A1. Its comment history stays on that issue.

Duplicate https://github.com/rrnewton/hermit/issues/526, filed 2026-07-24 as "Syscall classification: unlink" (legacy variant A1, batch 5), was merged into this body on 2026-10-03. Its comment history (0 comments) stays on that issue. What came from it: the man-page link in Summary; the open-reference behaviour (F2); its nondeterminism sources, reconciled under Determinism analysis, where its CONDITIONAL lost to `deterministic`; host security policy (F9); and the strace evidence below. Its recommended action, PASS-THRU, agrees with this body.

Legacy text replaced in this pass: Summary "Removes a name from the filesystem (deleting the file if it was the last link)."; Linux semantics "The inode is freed when the last link and open fd are gone. Verified: unlinking a /tmp file returned 0."; Determinism analysis "Deterministic given fixed FS state; an FS mutation passed through." with the source "None from the syscall; only external concurrent FS mutation." Every claim is kept in the sections above.

### 2026-10-03 host strace evidence

On this host (glibc 2.34, x86_64), a scratch probe calling glibc `unlink()` under `strace -f` showed the raw syscall, so the glibc-based tests above do exercise `SYS_unlink`:

```
unlink("c")                             = 0
```

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=unlink` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `unlink` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
unlink("/tmp/sysclass45/slr3")  = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence from duplicate https://github.com/rrnewton/hermit/issues/526

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=unlink`. The harness exited 0; at that time (`dd60278f`) the classification file had not yet given `unlink` an explicit arm. Removing a temporary file succeeded.

Observed strace line:
```text
unlink("/tmp/hermit-b5-unlink-YDF8S8") = 0
```
