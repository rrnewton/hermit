---
schema: syscall-pages/v1
syscall: fsetxattr
nr_x86_64: 190
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/360
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsetxattr&body=Discussion+for+the+%60fsetxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsetxattr%60%2C+or+in+what+the+page+says%3F%0A
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

`fsetxattr(fd, name, value, size, flags)` creates or replaces an extended attribute on the inode behind an open file descriptor. Hermit forwards it to the host kernel unchanged, with the container filesystem as the attribute store; Detcore keeps no xattr state.

Manual reference: Linux man-pages setxattr(2).

## Linux semantics

The VFS resolves `fd`, validates `flags`, `name` and `size`, and invokes the filesystem and security xattr setters on the referenced inode. No path lookup happens. The descriptor's open mode does not matter: an `O_RDONLY` descriptor works, because the write permission check is made against the inode (host probe). An `O_PATH` descriptor is refused with `EBADF` (host probe). Everything else (namespaces, name and value limits, permission, filesystem support) is as for `setxattr(2)`. On success it returns 0 and changes the inode's ctime.

### Flags and options

- F1: `flags = 0`: create the attribute if absent, replace its value if present
- F2: `XATTR_CREATE` (1): `EEXIST` if the attribute already exists
- F3: `XATTR_REPLACE` (2): `ENODATA` if the attribute does not exist
- F4: `XATTR_CREATE | XATTR_REPLACE` (3) always fails (`EEXIST` or `ENODATA`); any other bit in `flags`, including the all-ones value in the 2026-07-24 strace probe: `EINVAL`
- F5: `fd` is not an open descriptor, or is an `O_PATH` descriptor: `EBADF` (host probe)
- F6: `fd` is open read-only, write-only or read-write: the open mode is ignored and permission is checked on the inode (an `O_RDONLY` descriptor succeeded in the host probe)
- F7: namespace from the name prefix: `user.` (regular files and directories only, other inode types such as a pipe or socket give `EPERM`; sticky-directory owner rule), `trusted.` (`CAP_SYS_ADMIN` in the initial user namespace, else `EPERM`), `security.` (LSM or capability rules), `system.` (POSIX ACLs; an unknown `system.` name gives `EOPNOTSUPP`); no recognised prefix: `EOPNOTSUPP`
- F8: `system.posix_acl_access`/`system.posix_acl_default`: a binary ACL value; setting the access ACL rewrites the mode's permission bits; malformed ACL `EINVAL`; no ACL support `EOPNOTSUPP`
- F9: `name` empty or longer than 255 bytes: `ERANGE`; a bare prefix such as `user.`: `EINVAL`
- F10: `size = 0` stores an empty value; `size > 65536`: `E2BIG`; a smaller filesystem limit gives `ENOSPC` or `E2BIG`; no space or quota: `ENOSPC` or `EDQUOT`
- F11: permission on the inode: write access required (`EACCES`), immutable or append-only inode `EPERM`, read-only mount `EROFS`
- F12: the filesystem does not support extended attributes or this namespace: `EOPNOTSUPP`
- F13: `name` or `value` points outside the address space: `EFAULT`
- F14: side effects on success: the inode's ctime changes, an `IN_ATTRIB` inotify event is generated, and the value persists in the filesystem

## Determinism analysis

For fixed input bytes, credentials, and a stable, isolated filesystem state on the same filesystem type with the same mount options, the mutation and the errno repeat. The descriptor is the guest's own and threads run one at a time under Hermit, so racing guest writers get a fixed order. It is `conditional` because F7, F8, F10, F11 and F12 change with the host's filesystem, mount options and security policy.

### Nondeterminism sources

- External xattr mutation by host processes on the same inode
- Filesystem limits, namespace and ACL support, mount options, and LSM policy
- Credentials, including the privilege of the guest's root inside Hermit's user namespace
- Quotas and storage errors (`ENOSPC`, `EDQUOT`)

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `fsetxattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:783`, in the block that opens at `detcore/src/syscall_classification.rs:690` and ends at `detcore/src/syscall_classification.rs:804`, under the group comment at `detcore/src/syscall_classification.rs:767` ("Stable guest-owned metadata and synchronous writeback operations are repeatable in Hermit's fixed mount namespace and filesystem image."). The census lists it in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1698`, and `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists it at `detcore/src/syscall_classification.rs:2057` and asserts `PassThrough` at `detcore/src/syscall_classification.rs:2094`. The generic arm at `detcore/src/lib.rs:2843` sends it to `Detcore::passthrough` (`detcore/src/lib.rs:367`), which forwards it unchanged. Detcore's single-fd helper `get_fd` knows the descriptor argument (`detcore/src/syscalls/helpers.rs:856`), but nothing in the dispatch path rewrites it.

Why passthrough: Hermit has no virtual filesystem, and all twelve path, symlink and fd xattr syscalls are passed through, so the attribute store is the inode on the host filesystem. Under ptrace the forwarded call runs in the guest process on the guest's own descriptor, and the kernel applies F5 and F6 the same way on every run. Modelling `fsetxattr` alone would make `fgetxattr`, `getxattr` and the list calls disagree with it.

Run mode subscribes every syscall (`Subscription::all_syscalls()` at `detcore/src/lib.rs:1094`). Code facts show `subscribed_record_replay: false`: under `--passthru-opt`, which the comment at `detcore/src/lib.rs:3140` says `hermit record` and `hermit replay` turn on, the call never reaches Detcore and runs directly against the replay host's filesystem.

Existing evidence: `tests/c/syscall_file_metadata.c:181` calls glibc `fsetxattr(fd, "user.hermit", value, 9, 0)` on a read-write regular file. If it returns 0, the program requires `fgetxattr` to return the same 9 bytes (`tests/c/syscall_file_metadata.c:185`), `flistxattr` to list the name, and `fremovexattr` to succeed (`tests/c/syscall_file_metadata.c:192`). It accepts `EACCES`, `ENODATA`, `ENOTSUP`, `EOPNOTSUPP` and `EPERM` as a pass (`tests/c/syscall_file_metadata.c:37`). It runs on ptrace under `run --strict --verify` in `hermit-cli/tests/syscall_file_metadata.rs:51`, which asserts an inbound `fgetxattr` trace line (`hermit-cli/tests/syscall_file_metadata.rs:94`) but not `fsetxattr`, and in the cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`). verification on the default backend stays untested: no test names `SYS_fsetxattr` or requires the success path.

## Recommended tests

- T1 (covers F1, F2, F3, F4): on a fresh regular file opened `O_RDWR` in the guest's tmp directory, call raw `syscall(SYS_fsetxattr, ...)` with `flags = 0`, `XATTR_CREATE`, `XATTR_REPLACE`, `3` and `4`, asserting 0/`EEXIST`/`ENODATA`/`EINVAL` and reading values back with `fgetxattr`.
  Existing partial coverage of `flags = 0` through glibc: `tests/c/syscall_file_metadata.c:181`, which tolerates failure.
- T2 (covers F5, F6): assert `EBADF` for a closed descriptor and an `O_PATH` descriptor; assert success through an `O_RDONLY` descriptor and through an `O_WRONLY` descriptor.
- T3 (covers F7, F12): assert success for `user.k` on a regular file and a directory descriptor; `EPERM` for `user.k` on a pipe; `EOPNOTSUPP` for `noprefix`, `system.bogus` and a descriptor for a file under `/proc`. Record what the ptrace guest's root gets for `trusted.k` and `security.k`.
- T4 (covers F8): on a fixture filesystem with ACL support, set a valid `system.posix_acl_access` and assert the changed `st_mode` bits; assert `EINVAL` for a malformed value. The test names its filesystem and fails rather than skips without ACL support.
- T5 (covers F9, F10, F13): assert `ERANGE` for an empty and a 256-byte name, `EINVAL` for `user.`, an empty value stored with `size = 0`, `E2BIG` for `size = 65537`, and `EFAULT` for `name` or `value` set to `(void *)1`. Record the fixture filesystem's own size limit.
- T6 (covers F11): assert `EACCES` through a descriptor whose file was `chmod`ed to 000 after opening, and `EROFS` on a descriptor for a file under a read-only mount the guest can see.
- T7 (covers F14): assert an inotify `IN_ATTRIB` event after a successful call, and that `st_ctime` before and after matches across the two runs of `hermit run --verify` on ptrace.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/360: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (forwarded at detcore/src/lib.rs:2843, consistent with the other eleven xattr calls); listed 14 flag behaviours and 7 tests; ptrace_verification stays untested because tests/c/syscall_file_metadata.c:181 tolerates fsetxattr failure; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fsetxattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsetxattr&body=Discussion+for+the+%60fsetxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsetxattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fsetxattr" (https://github.com/rrnewton/hermit/issues/360), legacy variant A2. Comment history stays on that issue.

Rank 81 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls (legacy Summary text).

Open questions:

- F11: an `EPERM` from an immutable or append-only inode needs `CAP_LINUX_IMMUTABLE`, which the ptrace CI lane may not have. The owner decides whether an unreachable host error must be covered before `done`.

### 2026-10-03 host probe (syscall-tracking-p3)

Raw `syscall(190, ...)` through Python `ctypes` on this host (Linux 7.1.3, unprivileged uid, btrfs). Not run under Hermit.

```text
fsetxattr O_RDONLY fd    -> 0
fsetxattr bad fd         -> -1 EBADF
fsetxattr O_PATH fd      -> -1 EBADF
```

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fsetxattr -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(190, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fsetxattr(-1, 0xffffffffffffffff, 0xffffffffffffffff, 18446744073709551615, XATTR_CREATE|XATTR_REPLACE|0xfffffffc) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
