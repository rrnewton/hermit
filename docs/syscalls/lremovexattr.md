---
schema: syscall-pages/v1
syscall: lremovexattr
nr_x86_64: 198
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/636
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lremovexattr&body=Discussion+for+the+%60lremovexattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flremovexattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lremovexattr%60%2C+or+in+what+the+page+says%3F%0A
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

`lremovexattr(path, name)` deletes one extended attribute from the file at `path` itself: a final symlink is not followed. Hermit forwards it to the host kernel unchanged. The attribute lives in the guest's container filesystem, and Detcore keeps no extended-attribute state of its own.

## Linux semantics

The kernel resolves `path`, following symlinks in every component except the last, copies `name` from guest memory, checks permission for the name's namespace, and asks the filesystem's handler to delete the attribute. On success the call returns 0 and updates the inode's ctime. It never reports anything else; a guest sees the effect only through the getxattr, listxattr and stat families. Errors are checked in path-walk order, then name, then permission, so the errno is a function of the path, the attribute state, the credentials and the mount flags.

### Flags and options

- F1: `user.*` names: the caller needs write permission on the inode, and the inode must be a regular file or a directory; on any other type the kernel answers `EPERM`. On a sticky directory, only the owner (or a caller with `CAP_FOWNER`) may remove `user.*` names.
- F2: `trusted.*` names: removal needs `CAP_SYS_ADMIN` in the initial user namespace; without it, `EPERM`. Hermit's guest root inside a user namespace is in this case.
- F3: `security.*` names: the security module decides. Removing `security.capability` needs `CAP_SETFCAP` over the file's user namespace.
- F4: `system.posix_acl_access` and `system.posix_acl_default`: removal deletes the ACL. The caller must own the file or hold `CAP_FOWNER`; `ENODATA` if the ACL is absent. Missing ACL support is F13.
- F5: a name with no known namespace prefix: `EOPNOTSUPP`
- F6: an empty name, or a name longer than `XATTR_NAME_MAX` (255 bytes): `ERANGE`
- F7: the attribute does not exist: `ENODATA`. Missing filesystem support is F13.
- F8: the file is immutable or append-only: `EPERM`. The mount is read-only: `EROFS`.
- F9: a final symlink is not followed: the call acts on the symlink itself. A `user.*` name on a symlink gives `EPERM` (F1; host probe under Additional information), while `trusted.*` and `security.*` names on a symlink follow F2 and F3. On any other file type the call behaves like `removexattr`.
- F10: path resolution errors: `ENOENT` (a missing component or empty path), `ENOTDIR`, `EACCES` (search permission), `ENAMETOOLONG`, `ELOOP`
- F11: `path` or `name` points outside the address space: `EFAULT`
- F12: side effect: the inode's ctime changes on success
- F13: host-config failure: a filesystem without extended-attribute support, or without ACL support for `system.posix_acl_*`: `EOPNOTSUPP`.

## Determinism analysis

The result is a function of the attribute state along the path, the caller's credentials, the mount flags and the host's security-module policy. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so two guest threads removing or setting the same attribute get a fixed order. The call repeats exactly when the guest-visible file starts with the same attributes and nothing outside the container changes them.

### Nondeterminism sources

- Changes to extended attributes made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host configuration behind F3, F4, F7 and F8: the active security module and its policy, ACL and xattr support on the backing filesystem, file flags and read-only remounts.
- The new ctime (F12): the call creates it but does not return it. It becomes visible through the stat family and is handled there.

### Cross-host variation

- Host security module: what removal of a `security.*` name returns is decided by the loaded LSM and its policy (F3).
- Filesystem type, kernel version and mount options: whether the filesystem supports the namespace and ACLs decides `EOPNOTSUPP` (F13).
- Host mount flags and inode flags: an immutable or append-only file gives `EPERM` and a read-only mount gives `EROFS` (F8). The guest's root in a user namespace cannot set or clear these flags.

## Hermit design

Target policy: `passthrough`. The code already does this. `lremovexattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:791`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The block comment at `detcore/src/syscall_classification.rs:767` gives the condition: "Stable guest-owned metadata ... are repeatable in Hermit's fixed mount namespace and filesystem image", and the line at `detcore/src/syscall_classification.rs:769` leaves that boundary open for human review. Its path-following twin `removexattr` sits under the comment at `detcore/src/syscall_classification.rs:737`, which states the same condition more directly: these calls "are deterministic for stable objects and do not introduce asynchronous state or new kernel objects". The generic arm at `detcore/src/lib.rs:2843` forwards the call through `passthrough` at `detcore/src/lib.rs:367`. The census unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins the classification at `detcore/src/syscall_classification.rs:2075`; it checks the table, not the behaviour.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The attributes live in the container's filesystem, and only the kernel applies F1 to F8 the same way the guest's other xattr calls see them.
- The call returns only 0 or an errno, and every input to that answer is container state or host configuration that Hermit treats as fixed in run mode.
- The one value it creates, the ctime, is the stat family's concern.

Code facts show `subscribed_record_replay: false`, so in `--passthru-opt` mode the call never reaches Detcore. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module that is not in the exported tree, so this body does not describe how record handles the call.

The open gap is test evidence, not code. `tests_naming` is 0: no test names `SYS_lremovexattr`.

## Recommended tests

- T1 (covers F1, F7, F9): under `hermit run --verify` on ptrace, set `user.a` on a regular file and on a directory in the guest's `/tmp` and remove each with raw `syscall(SYS_lremovexattr, ...)`; assert 0, then `ENODATA` on a second removal. Call it with `user.a` on a symlink to the file and assert `EPERM`, and that the target still has `user.a`. Assert `EPERM` for `user.a` on a FIFO.
  The test must fail, not skip, if the backing filesystem rejects `user.*` attributes.
  Existing partial coverage: `tests/c/syscall_file_metadata.c:205` calls the glibc wrapper on a symlink, run by `hermit-cli/tests/syscall_file_metadata.rs:51` and by manifest cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`). It requires 0 only when the preceding `lsetxattr` succeeded (`tests/c/syscall_file_metadata.c:197`), and otherwise accepts `EACCES`, `ENODATA`, `ENOTSUP`, `EOPNOTSUPP` and `EPERM` (`tests/c/syscall_file_metadata.c:37`). `lsetxattr` of a `user.*` name on a symlink fails with `EPERM`, so only the tolerated-error path ever runs.
- T2 (covers F2, F3): assert `EPERM` for `trusted.x` on a regular file and on a symlink. Write a version-2 `security.capability` value, remove it, and assert it is gone; then drop `CAP_SETFCAP` with `capset` and assert that removal fails.
- T3 (covers F4): set an access ACL and a default ACL on a directory, remove each, and assert that the ACL is gone and the mode bits are unchanged. Assert `ENODATA` for a second removal.
- T4 (covers F5, F6): assert `EOPNOTSUPP` for `bogus.a`, and `ERANGE` for `""` and for a 256-byte name.
- T5 (covers none): assert `EPERM` on a file with the immutable flag set (`FS_IOC_SETFLAGS`), where the guest can set it, and `EROFS` under a read-only mount the guest can see. The test must say whether ptrace CI can reach each case. Both errors come from host inode and mount flags (host-config, U1), so this test documents F8 but does not cover it.
- T6 (covers F10, F11): assert `ENOENT` for a missing path and for `""`, `ENOTDIR`, `ENAMETOOLONG` for a 256-byte component, `ELOOP` for a symlink loop, and `EFAULT` for `path = (char *)1` and for `name = (char *)1`.
- T7 (covers F12): `stat` the file before and after a successful removal, and assert that the ctime the guest sees is identical in both `--verify` runs.
- U1 (covers F8, F13; host-config): the host's inode flags, mount flags and filesystem decide whether the immutable or append-only `EPERM`, `EROFS` (F8) and the missing-support `EOPNOTSUPP` (F13) occur, so CI cannot produce them on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:791`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/636: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> deterministic, to match removexattr (same inputs, no timing dependence); kept passthrough (forwarded at detcore/src/lib.rs:2843, classified at detcore/src/syscall_classification.rs:791); 12 flag items, 7 tests; the existing test only reaches the tolerated EPERM path on a symlink, so no coverage token and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host LSM policy, filesystem namespace and ACL support, mount and inode flags); split filesystem-support EOPNOTSUPP out of F4 and F7 into new F13, U1 host-config covers F8 and F13, T5 now covers none; no open questions section

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lremovexattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lremovexattr&body=Discussion+for+the+%60lremovexattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flremovexattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lremovexattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: lremovexattr" (https://github.com/rrnewton/hermit/issues/636), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### 2026-10-03 host probe (syscall-tracking-p3)

Raw `syscall(197, ...)` and `syscall(198, ...)` through Python `ctypes` on the measurement host (kernel 7.1.3, btrfs, unprivileged user, not under Hermit), against a file holding `user.a`, `user.b` and `user.c` and a symlink to it: `lremovexattr(symlink, "user.a")` gave `EPERM`; `removexattr(symlink, "user.a")` returned 0 and removed the target's attribute; a second `removexattr` gave `ENODATA`; `bogus.a` gave `ENOTSUP` (the same value as `EOPNOTSUPP`); an empty name and a 305-byte name gave `ERANGE`; `trusted.x` gave `EPERM`.
