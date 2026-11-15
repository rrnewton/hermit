---
schema: syscall-pages/v1
syscall: symlinkat
nr_x86_64: 266
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/490
  - https://github.com/rrnewton/hermit/issues/511
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+symlinkat&body=Discussion+for+the+%60symlinkat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsymlinkat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60symlinkat%60%2C+or+in+what+the+page+says%3F%0A
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

`symlinkat(target, newdirfd, linkpath)` creates a symbolic link at `linkpath`, resolved against the directory descriptor `newdirfd`, whose contents are the string `target`, stored verbatim and never resolved. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no link state of its own.

## Linux semantics

The kernel resolves the parent of the last component of `linkpath` against `newdirfd` (`AT_FDCWD` means the working directory, which gives `symlinkat` the same base-directory behaviour as `symlink`; an absolute `linkpath` ignores the descriptor), following symlinks in every earlier component, and creates a new inode of type `S_IFLNK` there. `target` is copied byte for byte. It is not looked up, so it may name a missing path (a dangling link), and a relative `target` is interpreted only later, relative to the directory that holds the link, each time the link is followed; `newdirfd` plays no part in that. `lstat` reports the new link's `st_size` as the length of `target`. The call takes no flags. On success the kernel sets the new inode's timestamps to the current time and updates the parent directory's mtime and ctime. The call returns 0 or a negative errno. It never reports the inode number or the timestamps; a guest sees those only through the stat family and `getdents`. Errors are checked in path-walk order, so the errno is a function of the two strings, the descriptor, the filesystem state, the credentials and the mount flags.

Manual reference: [symlinkat(2)](https://man7.org/linux/man-pages/man2/symlink.2.html).

### Flags and options

- F1: `target` contents: stored verbatim and not resolved. An absolute, relative or missing `target` all succeed, and `readlink` returns the exact bytes. An empty `target` gives `ENOENT`. A `target` longer than the filesystem's limit (a page, minus one byte, on most filesystems) gives `ENAMETOOLONG`.
- F2: `newdirfd`: `AT_FDCWD` resolves a relative `linkpath` against the working directory, inside the caller's root and mount namespace; any other descriptor must be an open directory. A relative `linkpath` with a descriptor that is not open: `EBADF`. A relative `linkpath` with a descriptor that is open but not a directory: `ENOTDIR`. An absolute `linkpath` ignores the descriptor. A relative `target` is still resolved relative to the link's own directory when followed, not relative to `newdirfd` or the working directory.
- F3: `linkpath` already exists as any type, including another symlink, dangling or not: `EEXIST`. The existing entry is never followed or replaced.
- F4: trailing slash on `linkpath`: when the name does not exist, `symlinkat("t", fd, "new/")` fails with `ENOENT`, because a trailing slash demands a directory. When it exists, the result is `EEXIST`.
- F5: mode and ownership: the link's mode is always `0777`; the umask does not apply. The owner is the caller's filesystem uid. The group is the caller's filesystem gid, unless the parent is setgid (or the filesystem is mounted `grpid`), in which case the link takes the parent's group.
- F6: an intermediate component of `linkpath` is missing, or `linkpath` is empty: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`. Too many symlinks in the prefix: `ELOOP`. A component longer than `NAME_MAX`, or a path longer than `PATH_MAX`: `ENAMETOOLONG`.
- F7: search or write permission is denied on the parent path: `EACCES`. F11 holds `EPERM` from a filesystem without symlink support.
- F8: `target` or `linkpath` points outside the address space: `EFAULT`
- F9: `EROFS` (the parent is on a read-only mount), and `ENOSPC` from a container-owned filesystem of fixed size. F10 holds the host's own space, quota and I/O failures.
- F10: host-transient failures: `ENOSPC` (the host filesystem has no space or no inodes left), `EDQUOT` (a host quota is exceeded) and `EIO` (failing storage).
- F11: the parent's filesystem does not support symlinks: `EPERM`.

## Determinism analysis

`conditional`. The result is a function of the two argument strings, the directory descriptor, the filesystem state along `linkpath`, the caller's credentials and the mount flags. The new link's contents are exactly `target`, so they are input-determined. The descriptor names a directory the guest opened earlier, so it is guest state. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so two guest threads racing to create the same `linkpath` get a fixed order and a fixed winner. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it. That condition is why the classification is `conditional` rather than `deterministic`: Hermit holds the filesystem stable by assumption, not by modelling it. This matches `symlink` (https://github.com/rrnewton/hermit/issues/487). The merge notes under Additional information record how the two filings' classifications were reconciled.

### Nondeterminism sources

- Changes to the filesystem made outside the container: path existence, directory permissions, mount behaviour and security policy all decide the errno. Hermit does not try to make a changing host filesystem deterministic.
- Host resources behind F9, F10 and F11: free space, inode count, quota, read-only remounts, I/O errors and the filesystem type.
- Values the call creates but does not return: the inode number and timestamps. They become visible only through `lstat`, `getdents` and similar calls, and are handled there.

### Cross-host variation

- Filesystem type behind the guest's paths: the longest `target` it stores before `ENAMETOOLONG` (F1), and whether it supports symlinks at all (`EPERM`, F11).
- Mount options of the host filesystem: under `grpid` the link takes the parent's group (F5).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `symlinkat` is classified `PassThrough` at `detcore/src/syscall_classification.rs:734`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The block comment at `detcore/src/syscall_classification.rs:691` and `:692` calls these calls "conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions". The group comment at `detcore/src/syscall_classification.rs:725` and `:726` gives the condition for `symlinkat`: these calls "repeat given stable guest-visible filesystem state with no external mutation".
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::symlinkat` at `detcore/src/syscall_classification.rs:2087` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F9 the same way the guest's later `readlinkat`, `lstat`, `open` and path walks see them.
- The descriptor is a guest descriptor that the kernel already holds, and the guest's descriptor table is reproduced by its earlier calls, so no translation is needed.
- The call returns only 0 or an errno. The two values it creates and does not return, the inode number and the timestamps, are the stat family's concern.
- Emulating the call would mean answering path lookups without the kernel, which is the job of a filesystem model Hermit does not have. Both filings recommended PASS-THRU.

Record and replay. Code facts show `subscribed_record_replay: false`, so Detcore's `--passthru-opt` subscription leaves `symlinkat` out. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module that is not in the exported tree, so this body does not describe how record captures `symlinkat`. The record/replay integration test `record_mkdir_eexist_materialization_semantics` (`hermit-cli/tests/record_replay.rs:1479`) runs the workload `c_record_replay_mkdir_eexist` (`hermit-cli/tests/record_replay.rs:1510`), whose guest calls glibc `symlinkat("real/deep", AT_FDCWD, argv[6])` at `tests/c/record_replay_mkdir_eexist.c:80` and requires success. That harness, `record_replay_command` (`hermit-cli/tests/record_replay.rs:494`), selects no backend explicitly.

Existing run-mode evidence, all through the glibc wrapper, which on this host's glibc 2.34 issues `SYS_symlinkat` (host check under Additional information):

- `tests/c/symlink_ops.c:89` calls `symlinkat(target, root_fd, "link")` with a real directory descriptor and checks with `readlinkat` that the contents equal the target (`tests/c/symlink_ops.c:90` to `:94`). Cell `c-programs/symlink-ops` at `tests/e2e/manifests/c-programs.yaml:13904` runs it in verify mode with ptrace in CI.
- The cargo integration test `deterministic_file_io_syscalls_verify` (`hermit-cli/tests/syscall_file_io.rs:51`) runs `tests/c/syscall_file_io.c` with `--backend=ptrace run --strict` and then `--verify`. That guest calls `symlinkat(renamed_at, AT_FDCWD, link_path)` at `tests/c/syscall_file_io.c:166` with absolute paths and requires 0. The test's list of syscalls that must appear in the trace (`hermit-cli/tests/syscall_file_io.rs:91`) does not include `symlinkat`, so it does not prove the raw syscall was seen.

`tests_naming` is 0: no test calls `syscall(SYS_symlinkat, ...)`. No coverage token is recorded yet: these tests cover the success case of F1 and part of F2, and the remaining behaviours have no test.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, call raw `syscall(SYS_symlinkat, ...)` with `AT_FDCWD` and a relative `linkpath`, with a real directory descriptor and a relative `linkpath`, and with an absolute `linkpath` and an invalid descriptor (which must be ignored). Use an absolute, a relative, a missing and a 4095-byte `target`. Assert `readlinkat` returns the exact bytes and `lstat` reports `S_IFLNK` with `st_size == strlen(target)`. Assert that a relative `target` created through a descriptor resolves from the link's directory, not from the working directory. Assert `ENOENT` for an empty `target`, `ENAMETOOLONG` for a `target` of 4096 bytes or more, `EBADF` for a relative `linkpath` with descriptor 999, and `ENOTDIR` for a relative `linkpath` with a regular-file descriptor.
  Existing partial coverage via the glibc wrapper: `tests/c/symlink_ops.c:89` (cell `c-programs/symlink-ops`) and `tests/c/syscall_file_io.c:166`.
- T2 (covers F3, F4): assert `EEXIST` when `linkpath` is an existing directory, a regular file, a live symlink and a dangling symlink, and that the existing entry is unchanged afterwards. Assert `ENOENT` for `linkpath = "new/"` when `new` does not exist.
- T3 (covers F5): under umask `077`, assert the new link's `lstat` mode is `S_IFLNK | 0777`. Make the parent setgid with a second mapped group and assert the link's `st_gid` is the parent's group; under a non-setgid parent assert it is the caller's gid.
- T4 (covers F6, F8): assert `ENOENT` for a missing intermediate component and for `""`, `ENOTDIR` with a regular file as an intermediate component, `ELOOP` for a symlink loop in the prefix, `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes, and `EFAULT` for `target = (char *)1` and for `linkpath = (char *)1`.
- T5 (covers F7): assert `EACCES` when creating under a directory the guest cannot write. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid. Record the chosen paths in the test. `EPERM` from a filesystem without symlink support is F11, recorded by U2.
- T6 (covers F9): assert `EROFS` under a read-only mount the guest can see, and `ENOSPC` on a small size-limited tmpfs. This `ENOSPC` comes from a container-owned filesystem of fixed size, so it is guest-determined. The host's own `ENOSPC`, `EDQUOT` and `EIO` are F10, recorded by U1.
- U1 (covers F10; host-transient): `ENOSPC`, `EDQUOT` and `EIO` depend on the host filesystem's free space, quotas and storage health, shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F10 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F11; host-config): which filesystems lack symlink support, and whether one is visible in the guest, is set by the host, so CI cannot rely on producing this error from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:734`, `detcore/src/lib.rs:2843`).
- T7 (covers F1, F3): record/replay on ptrace: `hermit record` replays a raw `SYS_symlinkat` success and an `EEXIST` failure from the recording. Existing partial coverage for the success case via the glibc wrapper: `tests/c/record_replay_mkdir_eexist.c:80`, driven by `hermit-cli/tests/record_replay.rs:1479`.
- T8 (covers F3): in two guest threads, race `symlinkat` to the same `linkpath` with different targets, and require the same winner, the same `readlinkat` contents and the same `EEXIST` loser across repeated `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/490: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/511 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged duplicate https://github.com/rrnewton/hermit/issues/511; linux_determinism deterministic -> conditional (the duplicate's CONDITIONAL wins, matching symlink, because Hermit assumes rather than enforces a stable filesystem); kept passthrough (classified at detcore/src/syscall_classification.rs:734, forwarded at detcore/src/lib.rs:2843); listed 9 flag behaviours and 8 tests; related now names the duplicate and symlink; no coverage token recorded because no test calls SYS_symlinkat directly, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type for the target length limit and symlink support, grpid mount option); F7 narrowed to EACCES, new F11 EPERM without symlink support (host-config U2); F9 narrowed to EROFS and container-owned ENOSPC, new F10 host ENOSPC/EDQUOT/EIO (host-transient U1); the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `symlinkat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+symlinkat&body=Discussion+for+the+%60symlinkat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsymlinkat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60symlinkat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: symlinkat" (https://github.com/rrnewton/hermit/issues/490), legacy variant A1.

Duplicate https://github.com/rrnewton/hermit/issues/511 was filed 2026-07-24 as "Syscall classification: symlinkat" (legacy variant A1) and is merged into this body. Its comment history (0 comments) stays on that issue; the comment history of https://github.com/rrnewton/hermit/issues/490 stays on that issue.

Merge reconciliation:

- Classification: https://github.com/rrnewton/hermit/issues/490 said DETERMINISTIC ("Same as symlink; deterministic given fixed FS state"); https://github.com/rrnewton/hermit/issues/511 said CONDITIONAL ("Results depend on external filesystem state"). Both describe the same fact: the result repeats given a fixed filesystem state. `conditional` wins because that fixed state is an assumption Hermit does not enforce, and because the `symlink` body (https://github.com/rrnewton/hermit/issues/487) already settled the same question the same way.
- Nondeterminism sources: https://github.com/rrnewton/hermit/issues/490 named only external concurrent filesystem mutation; https://github.com/rrnewton/hermit/issues/511 also named directory-fd identity, path state, permissions, mounts, security policy and concurrent mutations. All are kept: directory-fd identity is F2 (guest state, not host state), permissions F7, mounts F9, and concurrent guest mutation is serialized (T8).
- Kernel note from https://github.com/rrnewton/hermit/issues/511 ("AT_FDCWD gives symlinkat the same base-directory behavior as symlink for a relative link path") is part of F2. Its manual reference is kept under Linux semantics. Both issues recommended PASS-THRU, which is the target policy.
- Summaries: "Creates a symbolic link relative to a directory fd" (https://github.com/rrnewton/hermit/issues/490) and "Creates a symbolic link relative to a directory file descriptor" (https://github.com/rrnewton/hermit/issues/511) say the same thing; the Summary above replaces both.

### Open questions

- New inode numbers and timestamps are outside this issue. They belong to the stat family and getdents tracking.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=symlinkat` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `symlinkat` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
symlinkat("/etc/hostname", AT_FDCWD, "/tmp/sysclass45/sl2") = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence from https://github.com/rrnewton/hermit/issues/511

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=symlinkat`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `symlinkat` in the UNCLASSIFIED temporary-passthrough block. The temporary-link probe succeeded.

Observed strace line:
```text
symlinkat("/nonexistent", AT_FDCWD, "/tmp/hermit-b5-link-3143103") = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-10-03 glibc wrapper check

On the measurement host (Linux 7.1.3, glibc 2.34, x86_64), `strace` of a program calling glibc `symlinkat("b", dirfd, "s")` showed `symlinkat("b", 3, "s") = 0`: the wrapper issues `SYS_symlinkat`.
