---
schema: syscall-pages/v1
syscall: mknodat
nr_x86_64: 259
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/289
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mknodat&body=Discussion+for+the+%60mknodat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmknodat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mknodat%60%2C+or+in+what+the+page+says%3F%0A
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

`mknodat(dirfd, pathname, mode, dev)` creates a filesystem node (regular file, FIFO, socket, or character or block device) at `pathname`, resolved against `dirfd` or the working directory. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no filesystem state of its own.

## Linux semantics

`mknodat` is the directory-descriptor form of `mknod`; inside the kernel `mknod(path, mode, dev)` is `mknodat(AT_FDCWD, path, mode, dev)`. On the host's glibc, `mkfifo()` and `mknod()` both issue `SYS_mknodat` (strace evidence below), so most programs that create a FIFO reach this syscall. The file type in `mode` selects what is created. The kernel looks up the parent of the last component, starting from `dirfd` for a relative path and ignoring it for an absolute one; the last component must not exist and is never followed as a symlink. On success the kernel allocates an inode with current timestamps and updates the parent's mtime and ctime. The call returns 0 or a negative errno and reports nothing else. Creating a FIFO or socket node does not open it and creates no pipe or socket object; those appear only when something later opens or binds the node. `dev` is a 32-bit value in the kernel ABI and matters only for device types.

### Flags and options

- F1: file type `S_IFREG`, or a type field of 0: an empty regular file
- F2: `S_IFIFO`: a FIFO node
- F3: `S_IFSOCK`: a socket node, with no listener bound to it
- F4: `S_IFCHR` or `S_IFBLK` with a nonzero `dev`: a device node. Needs `CAP_MKNOD` in the initial user namespace, so a caller with it only inside a user namespace gets `EPERM`.
- F5: `S_IFCHR` with `dev` 0 (an overlayfs whiteout): allowed without `CAP_MKNOD`. Host check: an unprivileged caller created one with mode `020000`.
- F6: `S_IFDIR`: `EPERM` (directories come from `mkdir`). `S_IFLNK` or any other type value: `EINVAL`.
- F7: `dev` is ignored for every non-device type. Host check: `S_IFREG` with `dev` 0x12345 created a regular file.
- F8: permission and special bits: the node gets `mode & 07777 & ~umask`. `S_ISUID`, `S_ISGID` and `S_ISVTX` are kept, subject to the umask, except that `S_ISGID` is cleared when the caller is not in the new file's group and lacks `CAP_FSETID`. Host check: `S_IFREG|06777` under umask `022` gave `0106755`.
- F9: the parent has a default POSIX ACL: the umask is not applied and the node's access ACL comes from the default ACL combined with `mode`
- F10: ownership: the owner is the caller's filesystem uid. The group is the caller's filesystem gid, unless the parent is setgid (or the filesystem is mounted `grpid`), in which case the node takes the parent's group.
- F11: `pathname` already exists as any type, including a symlink, dangling or not: `EEXIST`
- F12: an intermediate component is missing, `pathname` is empty, or the last component has a trailing slash: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`.
- F13: search or write permission is denied on the path: `EACCES`
- F14: a component is longer than `NAME_MAX`, or the path is longer than `PATH_MAX`: `ENAMETOOLONG`. Too many symlinks in the prefix: `ELOOP`.
- F15: `pathname` points outside the address space: `EFAULT`
- F16: `pathname` lies under a read-only mount the guest can see: `EROFS`
- F17: `dirfd == AT_FDCWD`: a relative `pathname` resolves against the calling process's working directory, inside its root and mount namespace
- F18: `dirfd` is an open directory, including an `O_PATH` one: a relative `pathname` resolves against it. Host check: an `O_PATH` directory descriptor worked.
- F19: `dirfd` is open but not a directory, with a relative `pathname`: `ENOTDIR`. `dirfd` is not open: `EBADF`.
- F20: an absolute `pathname` ignores `dirfd`, even when it is not open. Host check: descriptor 9999 with an absolute path succeeded.
- F21: host-config failure: the host filesystem holding the parent does not support creating the requested node type: `EPERM`
- F22: host-transient failures: `ENOSPC` (no space or no inodes) and `EDQUOT` (quota)

## Determinism analysis

The result is a function of the directory `dirfd` names, the filesystem state along the path, `mode`, `dev`, the umask, the caller's credentials and capabilities, and the mount flags. Nothing in the call depends on timing or on another thread, and it never blocks: a FIFO that would block is only created here, not opened. Under Hermit, threads run one at a time, so two guest threads racing on the same path get a fixed order. The call repeats exactly when the guest-visible filesystem starts in the same state, the guest runs with the same credentials, and nothing outside the container changes it. The privilege check behind F4 is a property of how the guest is launched, not of timing, so it repeats for a fixed launch configuration.

### Nondeterminism sources

- Changes to the filesystem made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host configuration behind F21: which node types the filesystem supports. Host resources behind F22: free space, inode count and quota.
- Values the call creates but does not return: the inode number and timestamps. They become visible only through the stat family and `getdents`, and are handled there.

### Cross-host variation

- Filesystem type under the guest's files: which node types it can create; a type it refuses gives `EPERM` (F21).
- Host mount options: on a filesystem mounted `grpid`, a new node takes the parent's group even when the parent is not setgid (F10).

## Hermit design

Target policy: `passthrough`. The code already does this. `mknodat` is classified `PassThrough` at `detcore/src/syscall_classification.rs:815`, next to `mknod` at line 814, in the issue-reviewed pass-through block whose comment at `detcore/src/syscall_classification.rs:806` says these "remain conditional on Hermit's fixed-container and stable-state assumptions". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`; `mknodat` at line 2104) pins the classification, not guest behaviour.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The node lives in the guest's container mount namespace, and only the kernel can apply F1 to F20 consistently with what `stat`, `open` and `unlink` later see.
- The call creates no kernel object beyond the inode and never blocks, so there is no scheduling or time interaction for Detcore to control. The blocking rendezvous of a FIFO belongs to `open`, which Detcore handles separately.
- Device nodes (F4) are a privilege question, and passing through gives the guest the same `EPERM` the host would give a process launched the same way. Emulating success would hand the guest a node it cannot use.

Code facts show `subscribed_record_replay: false`: Detcore's `--passthru-opt` subscription leaves `mknodat` out, so in that mode the call never reaches Detcore. `hermit record` has its own subscriptions in a recorder module that is not in the exported tree (comment at `detcore/src/lib.rs:1078`), so this body does not describe how record captures it.

The open gap is test evidence, not code. `tests_naming` is 0: no test names `SYS_mknodat`. Existing run-mode coverage goes through glibc in `tests/c/mknod_special.c`: `mkfifo` at line 51, `mknodat(root_fd, "f2", S_IFIFO | 0600, 0)` at line 59, `mknod(S_IFREG)` at line 67 and the `EEXIST` refusal at line 75. That fixture runs in the ptrace verify cell `c-programs/mknod-special` (`tests/e2e/manifests/c-programs.yaml:12642`) and fails the run unless all four checks pass (`tests/c/mknod_special.c:93`). Its header (`tests/c/mknod_special.c:9`) says device nodes are deliberately left out.

## Recommended tests

- T1 (covers F1, F2, F3, F7, F17, F18): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mknodat, ...)` with `AT_FDCWD` after a `chdir`, with an `O_RDONLY|O_DIRECTORY` descriptor and with an `O_PATH` descriptor. Create a regular file with type 0, one with `S_IFREG` and a nonzero `dev`, a FIFO and a socket node. Assert each node's type and size through `fstatat` without opening the FIFO.
  Existing partial coverage via glibc: `tests/c/mknod_special.c:51`, `:59` and `:67`.
- T2 (covers F4, F5, F6): assert `EPERM` for `S_IFCHR` with `makedev(1, 3)` and for `S_IFBLK` with `makedev(7, 0)`, and `EPERM` for `S_IFDIR`. Assert `EINVAL` for `S_IFLNK` and for type `0170000`. Create a whiteout (`S_IFCHR`, `dev` 0) and assert its mode and `st_rdev`. Record the guest's credentials, since F4 depends on them.
- T3 (covers F8): under umask `022` and `077`, create regular files with `0777`, `06777` and `01777` and assert the exact `st_mode`. Add a case where the file's group is one the caller is not in, and assert that `S_ISGID` is cleared.
- T4 (covers F9, F10): with a default ACL on the parent, assert that the node's mode follows the ACL, not the umask. With a setgid parent owned by a second mapped group, assert the node's `st_gid`. If the fixture filesystem lacks ACL support, the test fails rather than skips.
- T5 (covers F11, F12): assert `EEXIST` for an existing file, directory, symlink and dangling symlink; `ENOENT` for a missing parent, `""` and `"new/"`; `ENOTDIR` for a regular file used as an intermediate component.
  Existing partial coverage: `tests/c/mknod_special.c:75` (an existing regular file).
- T6 (covers F13, F14, F15): assert `EACCES` under a directory the guest cannot write, `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes, `ELOOP` for a symlink loop in the prefix, and `EFAULT` for `pathname = (char *)1`. The filesystem-support `EPERM` is F21, recorded as U1; this test does not cover it.
- T7 (covers F16): assert `EROFS` under a read-only mount the guest can see. `ENOSPC` and `EDQUOT` (F22) are recorded as U2; an `ENOSPC` from a small test-owned tmpfs would not cover F22, which is about host storage.
- T8 (covers F19, F20): assert `ENOTDIR` for a relative path with a regular-file descriptor and `EBADF` with a closed descriptor number; then pass the closed number with an absolute path and assert the node is created.
- U1 (covers F21; host-config): which node types can be created belongs to the host filesystem under the guest's files, which the test does not choose. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:815`, `detcore/src/lib.rs:2843`).
- U2 (covers F22; host-transient): `ENOSPC` and `EDQUOT` come from host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F22 counts toward done only once Hermit hides these errors or reports them as infrastructure errors.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/289#issuecomment-5872837179
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/289: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mknodat records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/289 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (forwarded at detcore/src/lib.rs:2843; no blocking, no kernel object beyond the inode); listed 20 flag behaviours and 8 tests; no coverage token, because tests/c/mknod_special.c reaches mknodat only through glibc and covers FIFO, regular and EEXIST cases, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem node-type support, grpid mount option); filesystem-support EPERM split from F13 as new F21 (host-config U1); F16 narrowed to EROFS with ENOSPC and EDQUOT split out as new F22 (host-transient U2); T6 and T7 narrowed; the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mknodat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mknodat&body=Discussion+for+the+%60mknodat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmknodat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mknodat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mknodat" (https://github.com/rrnewton/hermit/issues/289), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/289.

### Open questions

- F4: if the ptrace guest holds `CAP_MKNOD` only inside a user namespace, device-node creation gives `EPERM`, and T2 asserts that. If the owner wants guests to create device nodes, that is a policy change (an `emulate` design), not part of this passthrough body.

### 2026-10-03 host probes

Run by syscall-tracking-p3 on the measurement host (Linux 7.1.3, glibc 2.34, btrfs, unprivileged uid, umask `022`). This was the host, not Hermit.

- `strace -f` of the shell command `mkfifo` showed `mknodat(AT_FDCWD, ".../f1", S_IFIFO|0666) = 0`, and of Python `os.mknod(path, S_IFREG|0o644)` showed `mknodat(AT_FDCWD, ".../r1", S_IFREG|0644) = 0`.
- Probes against scratch paths in a scratch directory: type 0 gave `0100644`; `S_IFSOCK|0777` gave `0140755`; `S_IFIFO|0777` gave `010755`; `S_IFDIR` `EPERM`; type `0170000` and `S_IFLNK` `EINVAL`; `S_IFCHR` 1:3 and `S_IFBLK` 7:0 `EPERM`; `S_IFCHR` with `dev` 0 succeeded with mode `020000`; `S_IFREG|06777` gave `0106755`; an existing name `EEXIST`; `"t/"` and `""` `ENOENT`; a regular-file descriptor `ENOTDIR`; descriptor 9999 `EBADF`; descriptor 9999 with an absolute path succeeded; an `O_PATH` directory descriptor succeeded.
- `S_IFREG` with `dev` 0x12345 succeeded and created a regular file.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mknodat`, and by confirming hermit currently routes `mknodat` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mknodat(AT_FDCWD, "/tmp/sysclass/hf2", S_IFREG|0644) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/289#issuecomment-5872837179
