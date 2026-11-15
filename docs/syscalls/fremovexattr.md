---
schema: syscall-pages/v1
syscall: fremovexattr
nr_x86_64: 199
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/358
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fremovexattr&body=Discussion+for+the+%60fremovexattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffremovexattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fremovexattr%60%2C+or+in+what+the+page+says%3F%0A
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

`fremovexattr(fd, name)` removes the extended attribute `name` from the inode behind the open descriptor `fd`. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable; Detcore keeps no extended-attribute state of its own.

## Linux semantics

The kernel resolves `fd` to its file, copies `name` from user memory, and splits it into a namespace prefix and a suffix. It then hands the removal to the xattr handler that the inode's filesystem registers for that prefix, after the generic permission check and the LSM hook. The call returns 0 or a negative errno and returns no data. On success the inode's ctime changes. Removing `system.posix_acl_access` also drops the access ACL, which changes what later permission checks and `getxattr` return; the mode bits are not rewritten. The descriptor's open mode does not matter: a read-only descriptor can remove an attribute. The name is copied before the descriptor is checked, so a bad descriptor with a bad name pointer gives `EFAULT` (the 2026-07-24 trace below). Errors depend on the descriptor, the name, the inode type, the caller's credentials, the filesystem's xattr support and the loaded LSMs.

### Flags and options

- F1: `user.*` namespace on a regular file or directory: the caller needs write permission on the inode. A missing attribute gives `ENODATA` (glibc spells it `ENOATTR`).
- F2: `user.*` namespace on any other inode type (FIFO, socket, symlink, device): `EPERM`. Host check: a pipe descriptor gave `EPERM`.
- F3: `trusted.*` namespace: needs `CAP_SYS_ADMIN` in the initial user namespace, otherwise `EPERM`.
- F4: `security.*` namespace: decided by the loaded LSM. Without a privileged caller or an LSM that allows it, `EPERM`.
- F5: `system.posix_acl_access` and `system.posix_acl_default`: remove the access ACL or the directory's default ACL. Needs inode ownership or `CAP_FOWNER`. Whether removing an absent ACL succeeds or gives `ENODATA` is up to the filesystem; on the host's btrfs it succeeded.
- F6: a namespace prefix the kernel does not know: `EOPNOTSUPP` (`ENOTSUP`). A filesystem with no handler for the prefix is F11.
- F7: an empty `name`, or a name longer than `XATTR_NAME_MAX` (255 bytes): `ERANGE`
- F8: `name` points outside the address space: `EFAULT`
- F9: `fd` is not open, or is an `O_PATH` descriptor: `EBADF`. Both checked on the host's Linux 7.1 kernel.
- F10: host-config failures: `EROFS` (read-only mount) and `EPERM` on an immutable or append-only inode. `EDQUOT` and `ENOSPC` are F12.
- F11: host-config failure: a filesystem with no handler for the prefix, for example tmpfs before Linux 6.6 for `user.*`: `EOPNOTSUPP`.
- F12: host-transient failures: `EDQUOT` and `ENOSPC` on filesystems that rewrite xattr blocks, which depend on host quota and free space shared with other processes.

## Determinism analysis

The result is a function of the inode's attribute set, the name, the caller's credentials, the descriptor, the filesystem type and the LSM policy. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so two guest threads racing to set and remove the same attribute get a fixed order. The call repeats exactly when the guest-visible filesystem starts with the same attributes on the same filesystem type, under the same kernel and LSM configuration, and nothing outside the container changes it. The result is `conditional` rather than `deterministic` because F4, F5 and F6 depend on host configuration that varies across machines even when the container's files do not: for example `tmpfs` gained `user.*` support only in Linux 6.6, so the same guest gets `EOPNOTSUPP` or `ENODATA` depending on the host kernel.

### Nondeterminism sources

- External xattr mutation: another process outside the container changes the inode's attributes.
- Filesystem and LSM policy: which namespaces the filesystem supports (F5, F6) and what the LSM allows for `security.*` (F4) differ by host.
- Host resources and mount state behind F10.

### Cross-host variation

- Host security module: what removal of a `security.*` name returns is decided by the loaded LSM and its policy (F4).
- Filesystem type and kernel version: which namespaces have a handler decides `EOPNOTSUPP` (F11), and whether removing an absent ACL succeeds or gives `ENODATA` is up to the filesystem (F5).
- Host mount flags and inode flags: a read-only mount gives `EROFS` and an immutable or append-only inode gives `EPERM` (F10).

## Hermit design

Target policy: `passthrough`. The code already does this. `fremovexattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:781`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The group comment at `detcore/src/syscall_classification.rs:767` gives the condition: "Stable guest-owned metadata and synchronous writeback operations are repeatable in Hermit's fixed mount namespace and filesystem image." The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only delegates to the record/replay helper, so in run mode the host kernel executes the call as issued.

Why passthrough, not `model` or `emulate`:

- Extended attributes live in the inode, on the guest's container filesystem. Hermit has no virtual filesystem, so only the kernel can apply F1 to F10 consistently with what the path-based siblings `removexattr` (`detcore/src/syscall_classification.rs:741`) and `lremovexattr`, and the readers `fgetxattr` and `flistxattr`, see. All of them are passed through too.
- The call creates no kernel object, returns no data and does not block, so it has no scheduling or time interaction for Detcore to control.
- The host-dependent part (F4 to F6) is a property of the container's filesystem and the host kernel, the same boundary every passed-through filesystem call already accepts. Emulating it would mean faking xattr support Hermit does not have.

Code facts show `subscribed_record_replay: false`: Detcore's `--passthru-opt` subscription leaves `fremovexattr` out, so in that mode the call never reaches Detcore. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module that is not in the exported tree, so this body does not describe how record captures `fremovexattr`.

The open gap is test evidence, not code. `tests_naming` is 0: no test names `SYS_fremovexattr`. The only existing caller is glibc `fremovexattr()` at `tests/c/syscall_file_metadata.c:191`, run by the ptrace verify cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`). That check is weak: if the preceding `fsetxattr` fails, the removal passes with any of `EACCES`, `ENODATA`, `ENOTSUP`, `EOPNOTSUPP` or `EPERM` (`tests/c/syscall_file_metadata.c:37`), so it does not pin F1 on a filesystem without `user.*` support.

## Recommended tests

- T1 (covers F1): under `hermit run --verify` on ptrace, on a guest-owned regular file and a directory, `fsetxattr` a `user.*` attribute and remove it with raw `syscall(SYS_fremovexattr, ...)`. Assert 0, then `ENODATA` for a second removal, and assert that `flistxattr` no longer lists it. The fixture must sit on a filesystem with `user.*` support; if it has none, the test fails rather than skips.
- T2 (covers F2): open a FIFO with `O_RDWR` and a socket, and assert `EPERM` for removing a `user.*` name from each.
- T3 (covers F3, F4): assert `EPERM` for `trusted.hermit` and for `security.hermit`. Record the guest's credentials and the host LSM in the test, because both results depend on them.
- T4 (covers F5): set an access ACL and a default ACL on a guest-owned directory through `fsetxattr`, remove each through `fremovexattr`, and assert that `fgetxattr` then reports `ENODATA`. Assert that removing an absent ACL gives the same result in both verify runs and record which result the CI filesystem gives.
- T5 (covers F6, F7, F8): assert `EOPNOTSUPP` for `bogus.name`, `ERANGE` for `""` and for a 300-byte `user.` name, and `EFAULT` for `name = (char *)1`.
- T6 (covers F9): assert `EBADF` for a closed descriptor number and for an `O_PATH` descriptor of an existing file.
- T7 (covers none): assert `EROFS` on a read-only mount the guest can see, and record the mount. That error comes from the host's mount flags (host-config, U1), as do immutable inodes, and quota and space are host-transient (U2), so this test documents F10 but does not cover it.
- U1 (covers F10, F11; host-config): the host's mount flags, inode flags and filesystem decide whether `EROFS`, the immutable or append-only `EPERM` (F10) and the missing-handler `EOPNOTSUPP` (F11) occur, so CI cannot produce them on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:781`, `detcore/src/lib.rs:2843`).
- U2 (covers F12; host-transient): `EDQUOT` and `ENOSPC` depend on host quota and free space shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F12 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/358: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (forwarded at detcore/src/lib.rs:2843; namespace support and LSM policy are host-dependent); listed 10 flag behaviours and 7 tests; no coverage token, because the only caller (tests/c/syscall_file_metadata.c:191) uses glibc and accepts five errnos, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host LSM policy, filesystem namespace support and ACL handling, mount and inode flags); split F6 into F6 and new host-config F11, split F10 into host-config F10 and new host-transient F12, U1 host-config covers F10 and F11, U2 host-transient covers F12, T7 now covers none; resolved the open question on unreachable host errors

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fremovexattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fremovexattr&body=Discussion+for+the+%60fremovexattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffremovexattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fremovexattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fremovexattr" (https://github.com/rrnewton/hermit/issues/358), legacy variant A2. The legacy body ranked it 79th in the batch-2 classification sequence, after excluding the 50 workload-observed batch-1 syscalls. Manual reference: Linux man-pages removexattr(2).

### 2026-10-03 host errno probe

Run by syscall-tracking-p3 on the measurement host (Linux 7.1.3, btrfs `/var/tmp`, unprivileged uid, Python `os.removexattr` on a descriptor): existing `user.a` gave 0; a second removal `ENODATA`; `bogus.a` `ENOTSUP`; `""` and a 305-byte name `ERANGE`; `trusted.a` and `security.a` `EPERM`; `system.posix_acl_access` with no ACL set 0; an `O_PATH` descriptor `EBADF`; a closed descriptor `EBADF`; a pipe descriptor `EPERM`; a write-only descriptor 0; raw `syscall(199, fd, (void *)1)` `EFAULT`. This was the host, not Hermit.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fremovexattr -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(199, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fremovexattr(-1, 0xffffffffffffffff) = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
