---
schema: syscall-pages/v1
syscall: fspick
nr_x86_64: 433
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/363
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fspick&body=Discussion+for+the+%60fspick%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffspick.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fspick%60%2C+or+in+what+the+page+says%3F%0A
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

`fspick(dfd, path, flags)` creates a filesystem context for reconfiguring the superblock of an existing mount and returns a descriptor for it, which `fsconfig` then configures (new mount API, Linux 5.2). Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219`).

Manual reference: Linux man-pages fspick(2).

## Linux semantics

The kernel resolves `path` relative to `dfd`, requires the result to be the root of a mount, creates a reconfiguration context for that mount's superblock, and returns an anonymous-inode descriptor (`[fscontext]`), using the lowest free descriptor number. Parameters set with `fsconfig` take effect with `FSCONFIG_CMD_RECONFIGURE`.

- The caller needs `CAP_SYS_ADMIN` over its mount namespace. The kernel checks this first, so an unprivileged caller gets `EPERM` for any arguments; the host strace under Additional information, with every argument invalid, also returned `EPERM`.
- A privileged caller then gets `EINVAL` for undefined flag bits, `EINVAL` when the resolved path is not the root of a mount, and the usual path-resolution errors (`ENOENT`, `ENOTDIR`, `ELOOP`, `EACCES`).

### Flags and options

- F1: `FSPICK_CLOEXEC`: set close-on-exec on the returned descriptor.
- F2: `FSPICK_SYMLINK_NOFOLLOW`: do not follow a trailing symlink in `path`.
- F3: `FSPICK_NO_AUTOMOUNT`: do not trigger an automount on the final component.
- F4: `FSPICK_EMPTY_PATH`: an empty `path` refers to `dfd` itself.
- F5: target resolution: `path` is resolved relative to `dfd` (or `AT_FDCWD`) and must name the root of a mount, otherwise `EINVAL`.
- F6: argument errors: missing `CAP_SYS_ADMIN` is `EPERM` before every other check; any other flag bit is `EINVAL` for a privileged caller.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Without the capability the call fails with `EPERM` deterministically. With it, the result depends on the mount topology at `path`, on the lifetime of that mount, on privilege, and on the lowest free descriptor number, and the context gives access to mutable superblock state.

Under Hermit the call returns `EPERM` without consulting the host and allocates no descriptor, so it is deterministic by construction.

### Nondeterminism sources

- Mount topology and lifecycle at `path`
- Descriptor allocation and bookkeeping: the context descriptor is an object Detcore does not track
- Privilege and path state

## Hermit design

Target policy: `refuse`. The legacy action was NEEDS-HANDLER; the code has since chosen refusal. What the code does at hermit f4055eb9cae4:

- `fspick` is a member of `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`, member at line 950) and is classified Determinized (`detcore/src/syscall_classification.rs:363`).
- The dispatch arm at `detcore/src/lib.rs:2215-2219` matches that predicate before the typed match and returns `Err(Error::Errno(Errno::EPERM))` for every argument combination. Its comment (`detcore/src/lib.rs:2203-2214`) says the call is never forwarded to the host.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`). The arm has no `panic_on_unsupported_syscalls` branch, so the refusal is the same whether or not Hermit fails closed.
- The unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`, member at line 2679) checks only the classification and the predicate membership; it does not run a guest.

Why `refuse`: the predicate's doc comment (`detcore/src/syscall_classification.rs:926-938`) says a deterministic container pins the guest's mount hierarchy for the whole run. A forwarded reconfiguration context would let the guest change mount options, such as read-only, of a superblock that may be shared with the host. `fsconfig`, which would apply the change, is refused by the same arm. A handler would need a virtual model of superblock options that Detcore does not have.

How this compares with Linux: an unprivileged Linux caller gets `EPERM` for every argument combination (F6), so Hermit's answer matches unprivileged Linux exactly. Under the ptrace backend the guest runs as uid 0 inside a user namespace (`detcore/src/syscalls/misc.rs:672`); if that namespace owns the guest's mount namespace, the kernel would grant the call, so there `EPERM` is a policy refusal rather than the kernel's answer.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): under `hermit run --verify` on ptrace, call raw `syscall(SYS_fspick, ...)` with `AT_FDCWD, "/"` and each of `FSPICK_CLOEXEC`, `FSPICK_SYMLINK_NOFOLLOW` and `FSPICK_NO_AUTOMOUNT`; with an open directory descriptor, `""` and `FSPICK_EMPTY_PATH`; and with a path that is not a mount root. Each call returns -1 with `EPERM`, the next `open` still gets the same lowest descriptor, and both runs agree.
- T2 (covers F6): an undefined flag bit such as `0x10` returns `EPERM`, the same as unprivileged Linux, and `/proc/self/mountinfo` is unchanged.
- T3 (covers none): record a guest that calls `fspick` and replay it; replay returns the same `EPERM` and the guest's output matches.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/363: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215-2219, which matches unprivileged Linux because the capability check runs first); linux_determinism stays conditional; 6 flag behaviours and 3 tests listed; no runtime test names fspick, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fspick`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fspick&body=Discussion+for+the+%60fspick%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffspick.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fspick%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fspick" (https://github.com/rrnewton/hermit/issues/363), legacy variant A2.

It was ranked 84 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

### Note on the strace evidence below

The strace text below was written on 2026-07-24 against the host kernel, not through Hermit. At hermit f4055eb9cae4 Hermit never forwards `fspick`; it returns `EPERM` itself (`detcore/src/lib.rs:2215-2219`), the same errno the host gave the unprivileged probe.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fspick -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(433, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fspick(-1, 0xffffffffffffffff, FSPICK_CLOEXEC|FSPICK_SYMLINK_NOFOLLOW|FSPICK_NO_AUTOMOUNT|FSPICK_EMPTY_PATH|0xfffffff0) = -1 EPERM (Operation not permitted)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
