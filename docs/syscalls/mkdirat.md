---
schema: syscall-pages/v1
syscall: mkdirat
nr_x86_64: 258
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/287
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mkdirat&body=Discussion+for+the+%60mkdirat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmkdirat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mkdirat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`mkdirat(dirfd, pathname, mode)` creates a directory at `pathname`, resolved against the directory open as `dirfd`, or against the working directory when `dirfd` is `AT_FDCWD`. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no directory state of its own.

## Linux semantics

`mkdirat` is the directory-descriptor form of `mkdir`; `mkdir(path, mode)` is `mkdirat(AT_FDCWD, path, mode)` inside the kernel. The kernel looks up the parent of the last path component, starting from `dirfd` for a relative path and ignoring `dirfd` for an absolute one, and following symlinks in every earlier component. The last component must not exist, and a symlink there is never followed, so a dangling symlink also gives `EEXIST`. On success the kernel allocates an inode, sets its timestamps to the current time, and updates the parent's mtime, ctime and, on most filesystems, link count. The call returns 0 or a negative errno and never reports the new inode number or timestamps; a guest sees those only through the stat family. Errors are checked in path-walk order, so the errno is a function of `dirfd`, the path, the filesystem state, the credentials and the mount flags. The call takes no flags argument; unlike several other `*at` calls it has no `AT_EMPTY_PATH`.

### Flags and options

- F1: permission bits: the new directory gets `mode & 0777 & ~umask`
- F2: special bits in `mode`: `S_ISVTX` (sticky) is kept, subject to the umask. `S_ISUID`, `S_ISGID` and file-type bits are ignored.
- F3: the parent has a default POSIX ACL: the umask is not applied and the new directory inherits the default ACL. The kernel combines `mode` with the ACL instead.
- F4: ownership: the owner is the caller's filesystem uid. The group is the caller's filesystem gid, unless the parent is setgid (or the filesystem is mounted `grpid`). In that case the new directory takes the parent's group and also gets `S_ISGID`.
- F5: `pathname` already exists as any type, including a symlink, dangling or not: `EEXIST`
- F6: an intermediate component is missing, or `pathname` is empty: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F7: trailing slashes are accepted: `mkdirat(dfd, "d/", ...)` creates `d`
- F8: search or write permission is denied on the path: `EACCES`
- F9: a component is longer than `NAME_MAX`, or the path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in the prefix: `ELOOP`.
- F10: `pathname` points outside the address space: `EFAULT`
- F11: `pathname` lies under a read-only mount the guest can see: `EROFS`
- F12: `dirfd == AT_FDCWD`: a relative `pathname` resolves against the calling process's working directory, inside its root and mount namespace
- F13: `dirfd` is an open directory, including an `O_PATH` one: a relative `pathname` resolves against it. Host check: an `O_PATH` directory descriptor worked.
- F14: `dirfd` is open but not a directory, with a relative `pathname`: `ENOTDIR`. `dirfd` is not open: `EBADF`.
- F15: an absolute `pathname` ignores `dirfd` entirely, even when it is not open. Host check: descriptor 9999 with an absolute path created the directory.
- F16: host-config failure: the host filesystem holding the parent does not support creating directories: `EPERM`
- F17: host-config failure: `EMLINK` when the parent's link count is at the limit of the host filesystem (the limit depends on the filesystem: ext4 without the `dir_nlink` feature stops at 65000 subdirectories, while ext4 with it and tmpfs do not stop)
- F18: host-transient failures: `ENOSPC` (no space or no inodes) and `EDQUOT` (quota)

## Determinism analysis

The result is a function of the directory `dirfd` names, the filesystem state along the path, the umask, the caller's credentials and the mount flags. Nothing in the call depends on timing or on another thread. Under Hermit, threads run one at a time, so two guest threads racing on the same path get a fixed order. The descriptor number in `dirfd` is the guest's own, issued by an earlier `open`, so it repeats whenever the guest's descriptor history does. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it.

### Nondeterminism sources

- Changes to the filesystem made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host configuration behind F16 and F17: filesystem support and link limits. Host resources behind F18: free space, inode count and quota.
- Values the call creates but does not return: the inode number and timestamps. They become visible only through `stat`, `getdents` and similar calls, and are handled there.

### Cross-host variation

- Filesystem type and features under the guest's files: whether directory creation is supported (F16) and the parent's link-count limit that gives `EMLINK` (F17).
- Host mount options: on a filesystem mounted `grpid`, a new directory takes the parent's group even when the parent is not setgid (F4).

## Hermit design

Target policy: `passthrough`. The code already does this. `mkdirat` is classified `PassThrough` at `detcore/src/syscall_classification.rs:731`, in the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:725` gives the condition: these calls "repeat given stable guest-visible filesystem state with no external mutation". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The directory tree in the guest's container mount namespace is the state, and only the kernel can apply F1 to F15 the same way the guest's other path syscalls see them.
- Every input that decides the result is container state Hermit already holds fixed, or another syscall's concern: `umask` is passed through at `detcore/src/syscall_classification.rs:724`, and `dirfd` comes from the guest's own earlier `open`.
- Its sibling `mkdir` (`detcore/src/syscall_classification.rs:730`) is passed through on the same grounds; the two must not diverge, because `mkdir` is `mkdirat(AT_FDCWD, ...)` in the kernel.

Code facts show `subscribed_record_replay: false`: Detcore's `--passthru-opt` subscription leaves `mkdirat` out, so in that mode the call never reaches Detcore. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module that is not in the exported tree. The integration test `record_mkdir_eexist_materialization_semantics` (`hermit-cli/tests/record_replay.rs:1479`) shows record/replay has `mkdirat`-specific replay behaviour: its doc comment at `hermit-cli/tests/record_replay.rs:1474` covers "mkdirat's absolute-path rule that ignores an unusable dirfd". It drives `tests/c/record_replay_mkdir_eexist.c`, whose `mkdirat` calls are at line 31.

The open gap is test evidence, not code. `tests_naming` is 0: no test names `SYS_mkdirat`. The existing run-mode coverage is one glibc `mkdirat()` call at `tests/c/mkdir_rmdir.c:77`, which creates a child relative to a directory descriptor (cell `c-programs/mkdir-rmdir`, `tests/e2e/manifests/c-programs.yaml:12575`, verify mode with ptrace in CI).

## Recommended tests

- T1 (covers F1, F2, F12, F13): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mkdirat, ...)` with `AT_FDCWD` after a `chdir`, with an `O_RDONLY|O_DIRECTORY` descriptor, and with an `O_PATH` descriptor. Use modes `0777`, `0755|S_ISVTX` and `0777|S_ISUID|S_ISGID` under umask `022` and `077`, and assert the exact `st_mode` from `fstatat`.
  Existing partial coverage via glibc `mkdirat()`: `tests/c/mkdir_rmdir.c:77` (relative to an `O_RDONLY|O_DIRECTORY` parent).
- T2 (covers F3): set a default ACL on the parent through `setxattr("system.posix_acl_default")` and create a child relative to the parent's descriptor under umask `077`. Assert that the child's mode and access ACL follow the default ACL. If the fixture filesystem lacks ACL support, the test fails rather than skips.
- T3 (covers F4): make the parent setgid and `chown` it to a second mapped group. Assert that the child's `st_gid` is the parent's group and that `S_ISGID` is set; assert that a child of a non-setgid parent takes the caller's gid.
- T4 (covers F5): assert `EEXIST` for an existing directory, a regular file, a symlink to an existing file, and a dangling symlink.
  Existing record/replay coverage: `tests/c/record_replay_mkdir_eexist.c:90` and `:91`, driven by `hermit-cli/tests/record_replay.rs:1479`.
- T5 (covers F6, F7): assert `ENOENT` for a missing parent and for `""`, `ENOTDIR` when a regular file is an intermediate component, and that `"newdir/"` creates `newdir`.
- T6 (covers F8): assert `EACCES` when creating under a directory the guest cannot write. The filesystem-support `EPERM` is F16, recorded as U1; this test does not cover it.
- T7 (covers F9, F10): assert `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes, `ELOOP` for a symlink loop in the prefix, and `EFAULT` for `pathname = (char *)1`.
- T8 (covers F11): assert `EROFS` under a read-only mount the guest can see. `EMLINK` (F17) is recorded as U1, and `ENOSPC` and `EDQUOT` (F18) as U2; an `ENOSPC` from a small test-owned tmpfs would not cover F18, which is about host storage.
- T9 (covers F14, F15): assert `ENOTDIR` for a relative path with a regular-file descriptor and `EBADF` with a closed descriptor number. Then pass the closed descriptor number with an absolute path and assert that the directory is created.
- U1 (covers F16, F17; host-config): directory support and the link-count limit belong to the host filesystem under the guest's files, which the test does not choose. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:731`, `detcore/src/lib.rs:2843`).
- U2 (covers F18; host-transient): `ENOSPC` and `EDQUOT` come from host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F18 counts toward done only once Hermit hides these errors or reports them as infrastructure errors.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/287#issuecomment-5872833051
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/287: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mkdirat records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/287 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (forwarded at detcore/src/lib.rs:2843, same grounds as mkdir); listed 15 flag behaviours (mkdir's 12 plus 3 dirfd cases) and 9 tests; no coverage token, because the only run-mode caller (tests/c/mkdir_rmdir.c:77) uses glibc mkdirat() for one case, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and features, grpid mount option); filesystem-support EPERM split from F8 as new F16; F11 narrowed to EROFS with EMLINK split out as new F17 (host-config U1, with F16) and ENOSPC and EDQUOT as new F18 (host-transient U2); T6 and T8 narrowed; the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mkdirat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mkdirat&body=Discussion+for+the+%60mkdirat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmkdirat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mkdirat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mkdirat" (https://github.com/rrnewton/hermit/issues/287), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/287.

### Open questions

- New inode numbers and timestamps are outside this issue. They belong to the stat family and getdents tracking.

### 2026-10-03 host errno probe

Run by syscall-tracking-p3 on the measurement host (Linux 7.1.3, btrfs, unprivileged uid, Python `os.mkdir(..., dir_fd=...)`): `"m1/"` created `m1`; `""` gave `ENOENT`; a regular-file descriptor gave `ENOTDIR`; descriptor 9999 gave `EBADF`; an `O_PATH` directory descriptor succeeded; descriptor 9999 with an absolute path succeeded; raw `syscall(258, AT_FDCWD, (char *)1, 0777)` gave `EFAULT`. The same host showed glibc and Python `mkdir()` issuing `SYS_mkdir`, not `SYS_mkdirat`, and `os.mkdir(name, dir_fd=fd)` issuing `mkdirat(3, "d3", 0777)`. This was the host, not Hermit.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mkdirat`, and by confirming hermit currently routes `mkdirat` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mkdirat(AT_FDCWD, "/tmp/sysclass/hd2", 0755) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/287#issuecomment-5872833051
