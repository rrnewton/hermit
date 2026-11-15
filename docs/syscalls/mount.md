---
schema: syscall-pages/v1
syscall: mount
nr_x86_64: 165
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/294
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mount&body=Discussion+for+the+%60mount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mount%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_mount_ns_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`mount(source, target, fstype, mountflags, data)` attaches a filesystem, or bind-mounts, remounts, moves or changes the propagation of an existing mount. Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219` at hermit f4055eb9cae4), so the guest's mount table stays the one Hermit's container set up.

Manual reference: [mount(2)](https://man7.org/linux/man-pages/man2/mount.2.html).

## Linux semantics

The kernel resolves the `target` path first, so a bad or missing target gives `EFAULT` or `ENOENT` before any privilege check. The host probe under Additional information, `mount("", "", "", 0, NULL) = -1 ENOENT`, shows this. The caller then needs `CAP_SYS_ADMIN` in the user namespace that owns its mount namespace, otherwise `EPERM`. A process that is root in its own user namespace with its own mount namespace, as Hermit's ptrace guest is (`detcore/src/syscalls/misc.rs:672`), passes that check on Linux.

Which operation runs is decided by `mountflags` in a fixed order: `MS_REMOUNT`, then `MS_BIND`, then a propagation flag, then `MS_MOVE`, and otherwise a new mount. The legacy magic value `MS_MGC_VAL` in the top 16 bits is stripped first.

Inside a non-initial user namespace, a new mount is allowed only for filesystem types marked mountable there (for example `tmpfs`, `proc`, `sysfs`, `devpts`, `mqueue`, `overlay`, `fuse`). Mount flags inherited from a more privileged namespace are locked: a remount that tries to clear `MS_RDONLY`, `MS_NOSUID`, `MS_NODEV`, `MS_NOEXEC` or the atime flags on such a mount fails with `EPERM`.

A successful mount changes the mount table of every process in the same mount namespace, appears in `/proc/*/mountinfo`, and can propagate to peer mounts.

### Flags and options

- F1: new mount (no operation flag): `fstype` names the filesystem, `source` the device or a placeholder, and `data` a filesystem-specific option string. Unknown `fstype`: `ENODEV`. In a non-initial user namespace only user-namespace-mountable types are allowed.
- F2: per-mount flags on a new mount or remount: `MS_RDONLY`, `MS_NOSUID`, `MS_NODEV`, `MS_NOEXEC`, `MS_NOSYMFOLLOW`, and the atime family `MS_NOATIME`, `MS_NODIRATIME`, `MS_RELATIME`, `MS_STRICTATIME`.
- F3: superblock flags: `MS_SYNCHRONOUS`, `MS_DIRSYNC`, `MS_LAZYTIME`, `MS_SILENT`, `MS_I_VERSION`, `MS_POSIXACL`, and `MS_MANDLOCK` (mandatory locking was removed in Linux 5.15; the flag is ignored with a warning). Kernel-internal bits such as `MS_KERNMOUNT` and `MS_ACTIVE` are cleared, and `MS_NOUSER` gives `EINVAL`.
- F4: `MS_REMOUNT`: change the flags and `data` of an existing mount; with `MS_BIND`, change only the per-mount flags. Clearing a locked flag gives `EPERM`; a target that is not a mount point gives `EINVAL`.
- F5: `MS_BIND`, optionally with `MS_REC`: make `source` visible at `target`, recursively with `MS_REC`.
- F6: propagation change: `MS_SHARED`, `MS_PRIVATE`, `MS_SLAVE` or `MS_UNBINDABLE`, optionally with `MS_REC`; `source`, `fstype` and `data` are ignored.
- F7: `MS_MOVE`: move an existing mount from `source` to `target`; `EINVAL` if `source` is not a mount point.
- F8: `MS_MGC_VAL` (`0xC0ED0000`) in the top 16 bits: legacy magic, stripped and otherwise ignored.
- F9: errors: `ENOENT`, `ENOTDIR`, `ELOOP` or `EFAULT` while resolving the paths (before the privilege check), `EPERM` without `CAP_SYS_ADMIN` over the mount namespace, and `EBUSY`, `ENOTBLK`, `EACCES` or `EROFS` from the filesystem.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Without privilege the call fails with an error that depends only on the arguments and on the paths, so it repeats. With privilege it succeeds and changes mount-namespace state that every later path lookup by every process in that namespace sees. The result then depends on which filesystems the host kernel provides, on the existing mount table, and on any concurrent mounts. A new superblock also gets host-allocated identity: an anonymous device number (`st_dev`) from a global allocator, fresh inode numbers, and the mount ID shown in `/proc/self/mountinfo`. None of these repeat across runs or hosts.

### Nondeterminism sources

- `CAP_SYS_ADMIN` over the caller's mount namespace, and the user-namespace layout the host provides
- Filesystem types compiled into or loaded by the host kernel, and the existing mount table and its propagation
- Host-allocated device numbers, inode numbers and mount IDs for each new mount
- Concurrent mounts and unmounts by other processes in the same namespace or its peers

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the code already realizes it:

- `classify_syscall` marks `mount` Determinized in the mount and namespace administration group (`detcore/src/syscall_classification.rs:339-369`, reviewed under the code tag `TODO-HUMAN-REVIEW(#724)`, https://github.com/rrnewton/hermit/issues/724). The comment there gives the reason: the container pins the guest's namespaces and mount hierarchy for the whole run.
- `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`) includes it, and `handle_syscall_event` returns `Err(Errno::EPERM)` before the typed match (`detcore/src/lib.rs:2215-2219`). The arm reads no argument and is not gated by `--strict`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`), so `code_facts.refused` is true.
- The only test is the unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`), which checks classification and membership but runs no guest.

Why `refuse`:

- Detcore's view of files (inodes, device numbers, the procfs and mountinfo it presents) assumes the mount table Hermit set up at start. A guest mount would bring in host-allocated device and inode numbers and change path resolution behind Detcore's back.
- `passthrough`, the legacy behaviour, would let a guest that is root in its user namespace succeed for `tmpfs` and bind mounts, with the host-dependent identity listed above.
- `model` (a virtual mount table, for example allowing `tmpfs` and bind mounts with deterministic device numbers) would serve sandboxing tools, but nothing in the code at this SHA supports it. It remains a possible future target, listed under Open questions.

Known differences from Linux:

- On Linux the Hermit guest, being root of its own user and mount namespaces, could mount `tmpfs` or bind-mount a directory. Under Hermit every request fails with `EPERM`, as for an unprivileged process on a host without user namespaces. Programs that try to mount (sandboxing tools, `mount(8)`, some test suites) see that error.
- `capget` is passed through (`detcore/src/syscall_classification.rs:716`, in the PassThrough list that ends at line 804), so the guest may see `CAP_SYS_ADMIN` in its effective set and still get `EPERM` from `mount`.
- Hermit returns `EPERM` before resolving any path, where Linux returns `ENOENT`, `EFAULT` or `ENODEV` first (F9).

Target (owner decision 6): keep the fixed `EPERM`, returned before any argument is read, which is what the code does today (`detcore/src/lib.rs:2215-2219`). Linux's order is `SYSCALL_DEFINE5(mount)` in `fs/namespace.c`: the `type`, `source` and `data` strings are copied in (`EFAULT`), `do_mount` resolves `target` (`ENOENT`, `ENOTDIR`, `ELOOP`, `EFAULT`), and only then does `path_mount` check `may_mount()` (`EPERM`) before the per-operation work that gives `ENODEV`, `EBUSY` or `EINVAL`. Hermit deliberately does not reproduce the path errors that come before the privilege check, because the call is refused, not emulated. Guests that mount (`mount(8)`, sandboxing tools, test suites) see the permission error an unprivileged Linux caller gets for an existing target.

The test helper `hermit-cli/tests/common/readonly_proc.rs:12` (`deny_writable_mounts`) installs a seccomp filter that fails flags-zero `mount` calls with `EPERM`. It runs in the test's own child before Hermit starts (`hermit-cli/tests/cli.rs:1283`, `hermit-cli/tests/procfs_determinism.rs:1194`), to force Hermit's container setup onto its read-only `/proc` fallback. It does not exercise a guest `mount`.

## Recommended tests

- T1 (covers F1, F2, F3, F8): under `hermit run --verify` on ptrace, a guest calls `syscall(SYS_mount, ...)` to mount `tmpfs` on a directory it created, once with flags 0, once with `MS_NOSUID|MS_NODEV|MS_NOEXEC|MS_RDONLY|MS_NOATIME`, once with `MS_SYNCHRONOUS|MS_LAZYTIME`, and once with `MS_MGC_VAL` in the top bits. Each call must return -1 with `EPERM`. `/proc/self/mountinfo` read before and after must be identical, and a file the guest wrote in the directory must still be there.
- T2 (covers F4, F5, F6, F7): the same guest tries `MS_REMOUNT|MS_RDONLY` on `/`, `MS_REMOUNT|MS_BIND|MS_RDONLY` on its directory, `MS_BIND|MS_REC` of a directory onto another, `MS_PRIVATE|MS_REC` on `/`, and `MS_MOVE`. Each returns `EPERM`, and `/proc/self/mountinfo` is unchanged.
- T3 (covers F9): a missing target, `target = (char *)-1`, and an unknown `fstype` each return the fixed `EPERM` of owner decision 6 (Linux gives `ENOENT` and `EFAULT` for the first two to any caller, and `ENODEV` for the third to a privileged caller), and both `--verify` runs agree.
- T4 (covers F1, F5): `hermit record` then `hermit replay` a shell guest that runs `mount -t tmpfs none "$d"` and `mount --bind "$d" "$e"` (util-linux) and prints their exit status and stderr. Both runs must report the permission error identically, and replay must match the recording.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/294#issuecomment-5872848511
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/294: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mount records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/294 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215 for every mount request, never forwarded); linux_determinism stays conditional; 9 flag behaviours and 4 tests listed; no coverage token because no test runs a guest that calls mount (hermit-cli/tests/common/readonly_proc.rs filters Hermit's own setup mount, not a guest call), so tracking_status stays todo; open questions on sandboxing tools and errno order under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (fixed EPERM before any argument check stays the target; Linux order in SYSCALL_DEFINE5(mount)/do_mount/path_mount deliberately not reproduced); target_policy stays refuse; T3 now asserts the decision-6 errno; no new F/T ids; open question on checking paths first removed (the private-mount-table scope question stays)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mount`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mount&body=Discussion+for+the+%60mount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mount%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mount" (https://github.com/rrnewton/hermit/issues/294), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/294.

### Open questions

- Should Hermit ever `model` a private mount table, so sandboxing tools (bubblewrap, the Nix build sandbox, `unshare -m` scripts) work inside the guest? That needs deterministic device numbers, inode numbers and mount IDs for new mounts, and a mountinfo view that follows them. This is an owner decision about scope.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mount`, and by confirming hermit currently routes `mount` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mount("", "", "", 0, NULL)      = -1 ENOENT (No such file or directory)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

The pass-through routing described in that evidence was true when it was filed. At hermit f4055eb9cae4 `mount` is Determinized and refused (see Hermit design).

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/294#issuecomment-5872848511
