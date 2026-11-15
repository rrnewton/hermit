---
schema: syscall-pages/v1
syscall: lsetxattr
nr_x86_64: 189
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/637
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsetxattr&body=Discussion+for+the+%60lsetxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsetxattr%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`lsetxattr(path, name, value, size, flags)` is `setxattr` without following a symlink in the last path component: when `path` names a symlink, the attribute is set on the link itself. Hermit forwards it to the host kernel unchanged, with the container filesystem as the attribute store; Detcore keeps no xattr state.

## Linux semantics

Identical to `setxattr(2)` except for path resolution: symlinks in earlier components are followed, the last component is not. That matters most for the `user.` namespace, which Linux allows only on regular files and directories: `lsetxattr` of a `user.*` name on any symlink, dangling or not, fails with `EPERM` (host probe). Namespaces that can label symlinks are `security.` (LSM labels) and `trusted.` (with privilege). When `path` does not end in a symlink, the call behaves exactly like `setxattr`. On success it returns 0 and changes the target inode's ctime. Namespace support and value limits depend on the filesystem type and mount options.

### Flags and options

- F1: `flags = 0`: create the attribute if absent, replace its value if present
- F2: `XATTR_CREATE` (1): `EEXIST` if the attribute already exists
- F3: `XATTR_REPLACE` (2): `ENODATA` if the attribute does not exist
- F4: `XATTR_CREATE | XATTR_REPLACE` (3) always fails (`EEXIST` or `ENODATA`); any other bit in `flags`: `EINVAL`
- F5: the last component is a symlink: the link itself is the target. A `user.*` name gives `EPERM` on a symlink, whether it points anywhere or not (host probe). `trusted.*` gives `EPERM` without `CAP_SYS_ADMIN` in the initial user namespace (host probe, on a symlink). `security.*` on a link is decided by the LSM.
- F6: the last component is not a symlink: same result as `setxattr`, including `user.*` succeeding on a regular file (host probe)
- F7: namespace from the name prefix on a non-symlink target: `user.` (regular files and directories only, sticky-directory owner rule), `trusted.` (`CAP_SYS_ADMIN`), `security.` (LSM or capability rules), `system.` (POSIX ACLs; an unknown `system.` name gives `EOPNOTSUPP`); no recognised prefix: `EOPNOTSUPP`. Setting `system.posix_acl_access` also rewrites the mode's permission bits.
- F8: `name` empty or longer than 255 bytes: `ERANGE`; a bare prefix such as `user.`: `EINVAL`
- F9: `size = 0` stores an empty value; `size > 65536`: `E2BIG`; a smaller filesystem limit gives `ENOSPC` or `E2BIG`; no space or quota: `ENOSPC` or `EDQUOT`
- F10: resolution of the earlier components: `ENOENT` for a missing path, `ENOTDIR`, `ENAMETOOLONG`, `ELOOP`, and `EACCES` without search permission
- F11: permission on the target inode: write access required (`EACCES`), immutable or append-only inode `EPERM`, read-only mount `EROFS`
- F12: the filesystem does not support extended attributes or this namespace: `EOPNOTSUPP`
- F13: `path`, `name` or `value` points outside the address space: `EFAULT`
- F14: side effects on success: the target inode's ctime changes, an `IN_ATTRIB` inotify event is generated, and the value persists in the filesystem

## Determinism analysis

As for `setxattr`, the result is a function of the path, the name, the value, `flags`, the credentials, and the filesystem's xattr support and limits. It repeats under fixed credentials and a stable, isolated filesystem state with the same filesystem type and mount options. Hermit runs threads one at a time, so racing guest writers get a fixed order. The extra symlink rule (F5) is a fixed kernel rule and adds no nondeterminism. The call is `conditional` because F7, F9, F11 and F12 change with the host's filesystem, mount options and security policy.

### Nondeterminism sources

- Filesystem type and mount options (namespace support, value limits, ACL support, read-only mounts).
- Security modules and their policy, which decide `security.*` on symlinks (F5, F7).
- Credentials, including the privilege of the guest's root inside Hermit's user namespace (`trusted.*`, F11).
- Free space and quota.
- Changes made outside the container, including a host process replacing the symlink between the guest's calls.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `lsetxattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:793`, in the block that opens at `detcore/src/syscall_classification.rs:690` and ends at `detcore/src/syscall_classification.rs:804`, under the group comment at `detcore/src/syscall_classification.rs:767` ("Stable guest-owned metadata and synchronous writeback operations are repeatable in Hermit's fixed mount namespace and filesystem image."). The census lists it in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1718`, and `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the classification at `detcore/src/syscall_classification.rs:2094`. The generic arm at `detcore/src/lib.rs:2843` sends it to `Detcore::passthrough` (`detcore/src/lib.rs:367`), which forwards it unchanged.

Why passthrough: Hermit has no virtual filesystem, and every one of the twelve path, symlink and fd xattr syscalls is passed through (`setxattr`/`getxattr` at `detcore/src/syscall_classification.rs:739` to `detcore/src/syscall_classification.rs:742`, the `f` and `l` variants between `detcore/src/syscall_classification.rs:777` and `detcore/src/syscall_classification.rs:793`). Modelling the symlink variant alone would make it disagree with `lgetxattr` and `llistxattr`. The kernel already applies the symlink rule (F5) the same way on every run.

Run mode subscribes every syscall (`Subscription::all_syscalls()` at `detcore/src/lib.rs:1094`). Code facts show `subscribed_record_replay: false`: under `--passthru-opt`, which the comment at `detcore/src/lib.rs:3140` says `hermit record` and `hermit replay` turn on, the call never reaches Detcore and runs directly against the replay host's filesystem.

Existing evidence: `tests/c/syscall_file_metadata.c:195` calls glibc `lsetxattr(symlink_path, "user.hermit", ...)` on a symlink to a regular file. By F5 that always returns `EPERM`, which the program accepts (`tests/c/syscall_file_metadata.c:37`); `must_exist` is then 0 and the follow-up `lgetxattr`, `llistxattr` and `lremovexattr` checks accept failure too. So the program, run on ptrace under `run --strict --verify` by `hermit-cli/tests/syscall_file_metadata.rs:51` and by the cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`), exercises only the `EPERM` path of F5 and requires only that it repeats. verification on the default backend stays untested: no test names `SYS_lsetxattr` or asserts a specific result.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F6): on a regular file reached by a path with no trailing symlink, call raw `syscall(SYS_lsetxattr, ...)` with `flags = 0`, `XATTR_CREATE`, `XATTR_REPLACE`, `3` and `4`, and assert 0/`EEXIST`/`ENODATA`/`EINVAL` as listed, reading each value back with `lgetxattr`.
- T2 (covers F5): on a symlink to a regular file and on a dangling symlink, assert `EPERM` for `user.k` and for `trusted.k` (record what the ptrace guest's root gets), and assert that the target file gained no attribute.
  Existing partial coverage of the `user.*` case through glibc: `tests/c/syscall_file_metadata.c:195`, which accepts the `EPERM` without asserting it.
- T3 (covers F7, F12): on a regular file, assert success for `user.k`, `EOPNOTSUPP` for `noprefix` and `system.bogus`, and `EOPNOTSUPP` for a file under `/proc`.
- T4 (covers F8, F9): assert `ERANGE` for an empty and a 256-byte name, `EINVAL` for `user.`, an empty value stored with `size = 0`, and `E2BIG` for `size = 65537`.
- T5 (covers F10, F13): assert `ENOENT`, `ENOTDIR`, `ELOOP` (a loop in an earlier component) and `ENAMETOOLONG`, and `EFAULT` for `path`, `name` or `value` set to `(void *)1`.
- T6 (covers F11): assert `EACCES` on a mode-000 regular file owned by the guest and `EROFS` under a read-only mount the guest can see.
- T7 (covers F14): assert an inotify `IN_ATTRIB` event after a successful call on a regular file, and that `st_ctime` before and after matches across the two runs of `hermit run --verify` on ptrace.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/637: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (forwarded at detcore/src/lib.rs:2843, consistent with the other eleven xattr calls); listed 14 flag behaviours and 7 tests; noted that tests/c/syscall_file_metadata.c:195 only reaches the user.* EPERM path on a symlink, so ptrace_verification stays untested and tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lsetxattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsetxattr&body=Discussion+for+the+%60lsetxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsetxattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: lsetxattr" (https://github.com/rrnewton/hermit/issues/637), legacy variant B. Comment history stays on that issue.

The legacy Summary ("Audit the x86_64 `lsetxattr(2)` syscall and replace its temporary catch-all policy with an explicit Detcore classification") is superseded: the classification is explicit at `detcore/src/syscall_classification.rs:793`.

Open questions:

- F11: an `EPERM` from an immutable inode needs `CAP_LINUX_IMMUTABLE`, which the ptrace CI lane may not have. The owner decides whether an unreachable host error must be covered before `done`.

### 2026-10-03 host probe (syscall-tracking-p3)

Raw `syscall(189, ...)` through Python `ctypes` on this host (Linux 7.1.3, unprivileged uid, btrfs). Not run under Hermit.

```text
lsetxattr user.k on symlink           -> -1 EPERM
lsetxattr user.k on regular file      -> 0
lsetxattr dangling symlink (user.k)   -> -1 EPERM
lsetxattr trusted.k on symlink        -> -1 EPERM
```

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
