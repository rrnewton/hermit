---
schema: syscall-pages/v1
syscall: setxattr
nr_x86_64: 188
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/463
  - https://github.com/rrnewton/hermit/issues/469
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setxattr&body=Discussion+for+the+%60setxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setxattr%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`setxattr(path, name, value, size, flags)` creates or replaces an extended attribute on the filesystem object named by `path`, following symlinks. Hermit forwards it to the host kernel unchanged. The attribute store is the guest's container filesystem, which Hermit treats as fixed input; Detcore keeps no xattr state of its own.

Manual reference: [setxattr(2)](https://man7.org/linux/man-pages/man2/setxattr.2.html).

## Linux semantics

The kernel copies `name` and `value` from the caller, resolves `path` (following a symlink in the last component, unlike `lsetxattr`), and dispatches on the name's namespace prefix to a handler. Each namespace has its own authorization rule. The handler checks permission on the target inode, and the filesystem stores the value. On success the call returns 0 and changes the inode's ctime. It returns nothing else; the value becomes visible through `getxattr`, `listxattr` and their `l`/`f` variants. Which namespaces are supported, and the largest value accepted, depend on the filesystem type and mount options. The host probe recorded under Additional information shows a 65536-byte `user.*` value accepted on tmpfs, refused with `ENOSPC` on btrfs, and every `user.*` write refused with `EOPNOTSUPP` on procfs.

### Flags and options

- F1: `flags = 0`: create the attribute if it is absent, replace its value if present
- F2: `XATTR_CREATE` (1): `EEXIST` if the attribute already exists
- F3: `XATTR_REPLACE` (2): `ENODATA` if the attribute does not exist
- F4: `XATTR_CREATE | XATTR_REPLACE` (3) always fails: `EEXIST` if the attribute exists, `ENODATA` if not (host probe). Any other bit in `flags`: `EINVAL`.
- F5: namespace from the name prefix. `user.`: only regular files and directories accept it (other inode types give `EPERM`), and on a sticky directory only the owner or a `CAP_FOWNER` holder may write. `trusted.`: needs `CAP_SYS_ADMIN` in the initial user namespace, else `EPERM`. `security.`: LSM labels and `security.capability`; an unprivileged write gave `EPERM` in the host probe. `system.`: POSIX ACL names; an unknown `system.` name gives `EOPNOTSUPP`. No recognised prefix: `EOPNOTSUPP`.
- F6: `system.posix_acl_access` and `system.posix_acl_default`: the value is a binary ACL. Setting the access ACL also rewrites the group/other permission bits of the inode's mode. A malformed ACL gives `EINVAL` on a filesystem with ACL support; a filesystem or mount without ACL support gives `EOPNOTSUPP` (the host probe's btrfs scratch mount did).
- F7: `name` empty or longer than 255 bytes (`XATTR_NAME_MAX`): `ERANGE`. A bare prefix such as `user.`: `EINVAL`.
- F8: `value`/`size`: `size = 0` (with `value` NULL or not) stores an empty value. `size > 65536` (`XATTR_SIZE_MAX`): `E2BIG`. A smaller filesystem limit gives `ENOSPC` or `E2BIG` depending on the filesystem. No space or quota: `ENOSPC` or `EDQUOT`.
- F9: path resolution follows symlinks in every component, including the last: a dangling symlink or a missing path gives `ENOENT`; a non-directory prefix component `ENOTDIR`; an over-long path or component `ENAMETOOLONG`; a symlink loop `ELOOP`; no search permission on a prefix `EACCES`
- F10: permission on the target inode: write access is required, so a mode-000 file gives `EACCES` even to its owner (host probe). An immutable or append-only inode gives `EPERM`. A read-only mount gives `EROFS`.
- F11: the filesystem does not support extended attributes, or not this namespace: `EOPNOTSUPP` (procfs in the host probe)
- F12: `path`, `name` or `value` points outside the address space: `EFAULT`
- F13: side effects on success: the inode's ctime is updated (host probe), an `IN_ATTRIB` inotify event is generated, and the value is stored in the filesystem, so it outlives the process and the Hermit run unless the file is in a scratch directory

## Determinism analysis

The result is a function of the path and its target inode, the name, the value bytes, `flags`, the caller's credentials, and the filesystem's xattr support and limits. Nothing depends on timing. Threads run one at a time under Hermit, so two guest threads writing the same attribute get a fixed order. The call repeats when the guest-visible filesystem starts in the same state, sits on the same filesystem type with the same mount options, and nothing outside the container changes it. It is `conditional`, not `deterministic`, because F5, F6, F8, F10 and F11 change with the host's filesystem, mount options and security policy.

### Nondeterminism sources

- Filesystem type and mount options (`acl`, `user_xattr`, `ro`), which decide F6, F8 and F11. The host probe shows the same 65536-byte write succeeding on tmpfs and failing with `ENOSPC` on btrfs.
- Security modules and their policy (F5 `security.*`).
- Credentials, including what privilege the guest's root has inside Hermit's user namespace (F5 `trusted.*`, F10).
- Free space and quota (F8).
- Changes to the filesystem made outside the container, including concurrent xattr writes by host processes.
- The ctime change (F13) is visible through the stat family, and is handled there.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `setxattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:742`, in the block that opens at `detcore/src/syscall_classification.rs:690` ("conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions", `detcore/src/syscall_classification.rs:691`) and ends at `detcore/src/syscall_classification.rs:804`. The comment above it at `detcore/src/syscall_classification.rs:737` says the xattr calls "are deterministic for stable objects and do not introduce asynchronous state or new kernel objects". The census lists it in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1747`. The generic arm at `detcore/src/lib.rs:2843` sends it to `Detcore::passthrough` (`detcore/src/lib.rs:367`), which forwards it unchanged.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The attribute store lives in the inodes of the container's filesystem, and all twelve path, symlink and fd xattr syscalls read and write the same store. Modelling `setxattr` alone would make `getxattr`, `listxattr` and `removexattr` disagree with it, and every xattr syscall is passed through today.
- The inputs that decide the result are either container state Hermit already holds fixed (the filesystem image, the guest's scratch directories) or properties of the host filesystem, which an emulation would have to copy anyway to stay compatible.
- Emulation would also have to reproduce F6, where an ACL write changes the inode's mode, which `stat` and every permission check then see.

Run mode subscribes every syscall (`Subscription::all_syscalls()` at `detcore/src/lib.rs:1094`). Code facts show `subscribed_record_replay: false`: under `--passthru-opt`, which the comment at `detcore/src/lib.rs:3140` says `hermit record` and `hermit replay` turn on, `setxattr` never reaches Detcore and runs directly against the kernel. A replay therefore writes the attribute again on the replay host and gets that host's result. This body does not claim more about how record handles it.

Existing evidence: `tests/c/syscall_file_metadata.c:167` calls glibc `setxattr(path, "user.hermit", value, 9, 0)` on a regular file under the guest's tmp directory. If it returns 0, the program requires `getxattr` to return the same 9 bytes (`tests/c/syscall_file_metadata.c:171`) and `removexattr` to succeed (`tests/c/syscall_file_metadata.c:178`). It accepts `EACCES`, `ENODATA`, `ENOTSUP`, `EOPNOTSUPP` and `EPERM` as a pass (`tests/c/syscall_file_metadata.c:37`), so a filesystem without `user.*` support passes without exercising the success path. The program runs on ptrace under `run --strict --verify` in `hermit-cli/tests/syscall_file_metadata.rs:51`, whose trace assertion list (`hermit-cli/tests/syscall_file_metadata.rs:91`) does not name `setxattr`, and in the e2e cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`). The cargo test passes `--tmp` with a directory under `CARGO_TARGET_TMPDIR` (`hermit-cli/tests/syscall_file_metadata.rs:79`), so the filesystem type is whatever holds the build directory. verification on the default backend stays untested: no test names `SYS_setxattr` or requires the success path.

## Recommended tests

- T1 (covers F1, F2, F3, F4): on a fresh regular file in the guest's tmp directory, call raw `syscall(SYS_setxattr, ...)` with `flags = 0` and read the value back with `getxattr`. Then assert `EEXIST` for `XATTR_CREATE` on the existing name, 0 and the new value for `XATTR_REPLACE`, `ENODATA` for `XATTR_REPLACE` on a missing name, `EEXIST`/`ENODATA` for `flags = 3`, and `EINVAL` for `flags = 4`.
  Existing partial coverage of `flags = 0` through glibc: `tests/c/syscall_file_metadata.c:167`, which tolerates failure.
- T2 (covers F5, F11): assert success for `user.k` on a regular file and on a directory; `EPERM` for `user.k` on a FIFO; `EOPNOTSUPP` for `noprefix`, for `system.bogus` and for a file under `/proc`. Assert and record what the ptrace guest gets for `trusted.k` and `security.k` (`EPERM` for an unprivileged host user).
- T3 (covers F6): on a fixture filesystem with ACL support, write a valid `system.posix_acl_access` value and assert the changed group bits in `st_mode`; assert `EINVAL` for a 4-byte malformed value. The test names its fixture filesystem and fails rather than skips if ACLs are unsupported there.
- T4 (covers F7, F8): assert `ERANGE` for an empty name and a 256-byte name, success for a 255-byte name, `EINVAL` for `user.`; assert that `size = 0` with a NULL value stores an empty value (`getxattr` returns 0); assert `E2BIG` for `size = 65537`. Record the fixture filesystem's own limit (`ENOSPC` at 65536 on btrfs).
- T5 (covers F9, F12): through a symlink to a regular file, assert the attribute lands on the target (`getxattr` on the target sees it, `lgetxattr` on the link gives `ENODATA`). Assert `ENOENT` for a dangling symlink and a missing path, `ENOTDIR`, `ELOOP` and `ENAMETOOLONG`, and `EFAULT` for `path`, `name` or `value` set to `(void *)1`.
- T6 (covers F10): assert `EACCES` on a mode-000 file owned by the guest, and `EROFS` under a read-only mount the guest can see. For `EPERM` on an immutable inode, the test must say whether the ptrace CI lane can set `chattr +i`.
- T7 (covers F13): assert that an inotify watch on the file reports `IN_ATTRIB` after a successful `setxattr`, and that `st_ctime` printed before and after is identical across the two runs of `hermit run --verify` on ptrace.
- T8 (covers F1, F5): two guest threads race `setxattr` on the same `user.*` name with different values, then the main thread reads it back. Under `hermit run --verify` on ptrace the final value must match across runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/463: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/469 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (forwarded at detcore/src/lib.rs:2843; Hermit has no xattr store and every xattr call is passed through); merged duplicate https://github.com/rrnewton/hermit/issues/469 (its DETERMINISTIC classification lost to conditional, see Additional information); listed 13 flag behaviours and 8 tests; ptrace_verification stays untested because tests/c/syscall_file_metadata.c tolerates setxattr failure; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setxattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setxattr&body=Discussion+for+the+%60setxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setxattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setxattr" (https://github.com/rrnewton/hermit/issues/463), legacy variant A1. Comment history stays on https://github.com/rrnewton/hermit/issues/463.

Duplicate https://github.com/rrnewton/hermit/issues/469 ("Syscall classification: setxattr", filed 2026-07-24, legacy variant A1, 0 comments) is merged into this body. Reconciliation:

- Classification: https://github.com/rrnewton/hermit/issues/469 said DETERMINISTIC ("deterministic given fixed filesystem state") with "None from the syscall; only external concurrent FS mutation" as its nondeterminism sources. https://github.com/rrnewton/hermit/issues/463 said conditional, citing filesystem support, mount options, security modules, quotas and permissions. Conditional wins: the 2026-10-03 host probe shows the same call succeeding on tmpfs and failing on btrfs (`ENOSPC`) and procfs (`EOPNOTSUPP`), so the result depends on more than the guest-visible file state.
- Both issues recorded a successful `user.*` write on a `/tmp` file; both strace lines are kept below. Their namespace lists (user, trusted, security, system) are merged into F5.

Open questions:

- F10: an `EPERM` from an immutable inode needs `CAP_LINUX_IMMUTABLE`, which the ptrace CI lane may not have. The owner decides whether an unreachable host error must be covered before `done`, or recorded as unreachable.
- `hermit-cli/tests/rr_suite.rs:567` declares `rr_xattr`, but the `rr_test!` macro marks every such test `#[ignore]` (`hermit-cli/tests/rr_suite.rs:233`), so it cannot be cited as ptrace coverage. Its source program is not in the exported tree.

### 2026-10-03 host probe (syscall-tracking-p3)

Raw `syscall(188/189/190, ...)` through Python `ctypes` on this host (Linux 7.1.3, unprivileged uid, btrfs scratch directory unless noted). Not run under Hermit.

```text
setxattr user.k CREATE new                 -> 0
setxattr user.k CREATE existing            -> -1 EEXIST
setxattr user.k REPLACE existing           -> 0
setxattr user.nokey REPLACE missing        -> -1 ENODATA
setxattr flags 3 (existing)                -> -1 EEXIST
setxattr flags 3 (missing)                 -> -1 ENODATA
setxattr flags 4                           -> -1 EINVAL
setxattr empty value size0 NULL            -> 0
setxattr size 65537                        -> -1 E2BIG
setxattr name no prefix                    -> -1 ENOTSUP
setxattr name empty                        -> -1 ERANGE
setxattr name 'user.'                      -> -1 EINVAL
setxattr name 256 long                     -> -1 ERANGE
setxattr name 255 long                     -> 0
setxattr trusted.k                         -> -1 EPERM
setxattr security.k                        -> -1 EPERM
setxattr system.bogus                      -> -1 ENOTSUP
setxattr system.posix_acl_access "junk"    -> -1 ENOTSUP
setxattr bad path ptr                      -> -1 EFAULT
setxattr bad value ptr                     -> -1 EFAULT
setxattr missing path                      -> -1 ENOENT
setxattr via symlink (follows)             -> 0
setxattr via dangling symlink              -> -1 ENOENT
setxattr on directory user.d               -> 0
setxattr mode 000 user.k (owner)           -> -1 EACCES
setxattr user.k: ctime changed             -> True
setxattr user.s65536 size 65536, btrfs     -> -1 ENOSPC
setxattr user.s65536 size 65536, tmpfs     -> 0
setxattr user.s4000 size 4000, procfs      -> -1 ENOTSUP
```

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/463)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setxattr`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setxattr` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setxattr("/tmp/hermit-b5-xattr-GCqUqL", "user.hermit_batch5", "x", 1, 0) = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/469)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setxattr` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `setxattr` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
setxattr("/tmp/sysclass45/tf", "user.k", "v", 1, 0) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
