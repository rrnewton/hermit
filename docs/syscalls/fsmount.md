---
schema: syscall-pages/v1
syscall: fsmount
nr_x86_64: 432
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/361
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsmount&body=Discussion+for+the+%60fsmount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsmount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsmount%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_mount_ns_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`fsmount(fs_fd, flags, attr_flags)` turns a filesystem context that has completed `FSCONFIG_CMD_CREATE` into a detached mount and returns a descriptor for it, which `move_mount` can attach (new mount API, Linux 5.2). Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219`).

Manual reference: Linux man-pages fsmount(2).

## Linux semantics

The kernel takes the superblock that `fsconfig(FSCONFIG_CMD_CREATE)` produced in the context `fs_fd`, makes a mount of it with the attributes in `attr_flags`, and returns a descriptor for that detached mount, using the lowest free descriptor number. If the last reference to the mount closes before it is attached, the kernel unmounts it.

- The caller needs `CAP_SYS_ADMIN` over its mount namespace. The kernel checks this first, so an unprivileged caller gets `EPERM` for any arguments; the host strace under Additional information, with every argument invalid, also returned `EPERM`.
- A privileged caller then gets `EINVAL` for unknown `flags` bits, for `attr_flags` bits outside the accepted set, or for an invalid atime value; `EINVAL` when `fs_fd` is not a filesystem context; and `EBUSY` when the context has not reached the "created, awaiting mount" phase.

### Flags and options

- F1: `FSMOUNT_CLOEXEC`: set close-on-exec on the returned descriptor.
- F2: `FSMOUNT_NAMESPACE`: create the mount in a new mount namespace. It is defined in the 7.1 kernel headers on this host (`/usr/src/kernels/7.1.3-0_fbk0_0_g295ad0959f34/include/uapi/linux/mount.h:113`); kernels without it reject the bit with `EINVAL`.
- F3: `MOUNT_ATTR_RDONLY`: the mount is read-only.
- F4: `MOUNT_ATTR_NOSUID`: ignore set-user-ID and set-group-ID bits.
- F5: `MOUNT_ATTR_NODEV`: disallow access to device special files.
- F6: `MOUNT_ATTR_NOEXEC`: disallow program execution.
- F7: atime mode in the `MOUNT_ATTR__ATIME` field: `MOUNT_ATTR_RELATIME` (0), `MOUNT_ATTR_NOATIME` or `MOUNT_ATTR_STRICTATIME`; any other value of the field is `EINVAL`.
- F8: `MOUNT_ATTR_NODIRATIME`: do not update directory access times.
- F9: `MOUNT_ATTR_NOSYMFOLLOW` (Linux 5.10): do not follow symlinks on this mount.
- F10: argument errors: missing `CAP_SYS_ADMIN` is `EPERM` before every other check; other `flags` or `attr_flags` bits are `EINVAL` (idmapping is set with `mount_setattr`, not here); an `fs_fd` that is not a filesystem context is `EINVAL`; a context not yet created is `EBUSY`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Without the capability the call fails with `EPERM` deterministically. With it, the result depends on the context and its filesystem driver, on host filesystem support and resources, on namespace and privilege policy, and on the lowest free descriptor number.

Under Hermit the call returns `EPERM` without consulting the host and allocates no descriptor, so it is deterministic by construction.

### Nondeterminism sources

- Descriptor and mount-object allocation: the returned mount descriptor is an object Detcore does not track
- Host filesystem support and resources
- Namespace and privilege policy

## Hermit design

Target policy: `refuse`. The legacy action was NEEDS-HANDLER; the code has since chosen refusal. What the code does at hermit f4055eb9cae4:

- `fsmount` is a member of `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`, member at line 948) and is classified Determinized (`detcore/src/syscall_classification.rs:361`).
- The dispatch arm at `detcore/src/lib.rs:2215-2219` matches that predicate before the typed match and returns `Err(Error::Errno(Errno::EPERM))` for every argument combination. Its comment (`detcore/src/lib.rs:2203-2214`) says the call is never forwarded to the host.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`). The arm has no `panic_on_unsupported_syscalls` branch, so the refusal is the same whether or not Hermit fails closed.
- The unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`, member at line 2677) checks only the classification and the predicate membership; it does not run a guest.

Why `refuse`: the predicate's doc comment (`detcore/src/syscall_classification.rs:926-938`) says a deterministic container pins the guest's mount hierarchy for the whole run. `fsopen`, which `fsmount` needs first, and `move_mount`, which would attach the result, are refused by the same arm, so a forwarded `fsmount` could never complete the mount path. A handler that modelled detached mounts would need a virtual mount table that Detcore does not have.

How this compares with Linux: an unprivileged Linux caller gets `EPERM` for every argument combination (F10), so Hermit's answer matches unprivileged Linux exactly. Under the ptrace backend the guest runs as uid 0 inside a user namespace (`detcore/src/syscalls/misc.rs:672`); if that namespace owns the guest's mount namespace, the kernel would grant the call, so there `EPERM` is a policy refusal rather than the kernel's answer.

## Recommended tests

- T1 (covers F1, F3, F4, F5, F6, F7, F8, F9): under `hermit run --verify` on ptrace, call raw `syscall(SYS_fsmount, ...)` on fd `-1` and on an ordinary file descriptor with `FSMOUNT_CLOEXEC` and with each mount attribute and atime mode. Each call returns -1 with `EPERM`, the next `open` still gets the same lowest descriptor, and both runs agree.
- T2 (covers F2, F10): `FSMOUNT_NAMESPACE`, an undefined `flags` bit, an undefined `attr_flags` bit, `MOUNT_ATTR_IDMAP`, and the atime value `0x30` all return `EPERM`, the same as unprivileged Linux, and `/proc/self/mountinfo` is unchanged.
- T3 (covers none): record a guest that calls `fsmount` and replay it; replay returns the same `EPERM` and the guest's output matches.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/361: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215-2219, which matches unprivileged Linux because the capability check runs first); linux_determinism stays conditional; 10 flag behaviours and 3 tests listed; no runtime test names fsmount, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fsmount`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsmount&body=Discussion+for+the+%60fsmount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsmount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsmount%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fsmount" (https://github.com/rrnewton/hermit/issues/361), legacy variant A2.

It was ranked 82 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

### Note on the strace evidence below

The strace text below was written on 2026-07-24 against the host kernel, not through Hermit. At hermit f4055eb9cae4 Hermit never forwards `fsmount`; it returns `EPERM` itself (`detcore/src/lib.rs:2215-2219`), the same errno the host gave the unprivileged probe.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fsmount -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(432, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fsmount(-1, FSMOUNT_CLOEXEC|0xfffffffe, MOUNT_ATTR_RDONLY|MOUNT_ATTR_NOSUID|MOUNT_ATTR_NODEV|MOUNT_ATTR_NOEXEC|MOUNT_ATTR__ATIME|MOUNT_ATTR_NODIRATIME|MOUNT_ATTR_NOSYMFOLLOW|0xffdfff00) = -1 EPERM (Operation not permitted)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
