---
schema: syscall-pages/v1
syscall: fsopen
nr_x86_64: 430
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/362
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsopen&body=Discussion+for+the+%60fsopen%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsopen.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsopen%60%2C+or+in+what+the+page+says%3F%0A
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

`fsopen(fs_name, flags)` creates a filesystem configuration context for the filesystem type `fs_name` and returns a descriptor for it, which `fsconfig` configures and `fsmount` turns into a mount (new mount API, Linux 5.2). Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219`).

Manual reference: Linux man-pages fsopen(2).

## Linux semantics

The kernel resolves a filesystem type, allocates a filesystem context, and returns an anonymous-inode descriptor (`[fscontext]`), using the lowest free descriptor number.

- The caller needs `CAP_SYS_ADMIN` over its mount namespace. The kernel checks this first, so an unprivileged caller gets `EPERM` for any arguments; the host strace under Additional information, with invalid flags, also returned `EPERM`.
- A privileged caller then gets `EINVAL` for any flag other than `FSOPEN_CLOEXEC`, and `ENODEV` for a filesystem type the kernel does not know. Looking up an unknown type can make the kernel try to load a module for it.
- Reading the returned descriptor returns the context's queued error, warning and information messages, which the filesystem writes while it parses parameters.

### Flags and options

- F1: `fs_name`: the filesystem type, resolved against the types the host kernel has registered; an unknown type gives `ENODEV`, and the lookup can trigger a module load.
- F2: `FSOPEN_CLOEXEC`: set close-on-exec on the returned descriptor.
- F3: any other flag bit: `EINVAL` for a privileged caller.
- F4: missing `CAP_SYS_ADMIN` over the mount namespace: `EPERM`, checked before every other argument.
- F5: `read` on the returned descriptor: returns the filesystem context's queued messages.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Without the capability the call fails with `EPERM` deterministically. With it, the result depends on the host's registered filesystem types and loadable modules, on namespace and security policy, and on the lowest free descriptor number; the context's message log depends on the filesystem driver.

Under Hermit the call returns `EPERM` without consulting the host and allocates no descriptor, so it is deterministic by construction.

### Nondeterminism sources

- Descriptor allocation and bookkeeping: the returned descriptor is an object Detcore does not track
- Host filesystem registration and module autoloading
- Namespace and security policy

## Hermit design

Target policy: `refuse`. The legacy action was NEEDS-HANDLER; the code has since chosen refusal. What the code does at hermit f4055eb9cae4:

- `fsopen` is a member of `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`, member at line 947) and is classified Determinized (`detcore/src/syscall_classification.rs:360`).
- The dispatch arm at `detcore/src/lib.rs:2215-2219` matches that predicate before the typed match and returns `Err(Error::Errno(Errno::EPERM))` for every argument combination. Its comment (`detcore/src/lib.rs:2203-2214`) says the call is never forwarded to the host.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`). The arm has no `panic_on_unsupported_syscalls` branch, so the refusal is the same whether or not Hermit fails closed.
- The unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`, member at line 2676) checks only the classification and the predicate membership; it does not run a guest.

Why `refuse`: the predicate's doc comment (`detcore/src/syscall_classification.rs:926-938`) says a deterministic container pins the guest's mount hierarchy for the whole run. `fsopen` is the first step toward a new mount, and the rest of that path (`fsmount`, `move_mount`) is refused too, so a forwarded context could never be used to mount anything. Forwarding it would also expose the host's filesystem table and could load kernel modules. A handler that modelled contexts would need a virtual mount table that Detcore does not have.

How this compares with Linux: an unprivileged Linux caller gets `EPERM` for every argument combination (F4), so Hermit's answer matches unprivileged Linux exactly. Under the ptrace backend the guest runs as uid 0 inside a user namespace (`detcore/src/syscalls/misc.rs:672`); if that namespace owns the guest's mount namespace, the kernel would grant the call, so there `EPERM` is a policy refusal rather than the kernel's answer. Because no context descriptor is ever created, F5 cannot occur under Hermit.

## Recommended tests

- T1 (covers F1, F2, F4): under `hermit run --verify` on ptrace, call raw `syscall(SYS_fsopen, ...)` with `"tmpfs", 0`, `"tmpfs", FSOPEN_CLOEXEC` and `"no-such-fs", 0`. Each call returns -1 with `EPERM`, the next `open` still gets the same lowest descriptor, and both runs agree.
- T2 (covers F3): `fsopen("tmpfs", 0x2)` returns `EPERM`, the same as unprivileged Linux.
- T3 (covers F5): after the T1 calls, `readlink` on every entry of `/proc/self/fd` shows no `anon_inode:[fscontext]` descriptor, so there is no context to read messages from.
- T4 (covers none): record a guest that calls `fsopen` and replay it; replay returns the same `EPERM` and the guest's output matches.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/362: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215-2219, which matches unprivileged Linux because the capability check runs first); linux_determinism stays conditional; 5 flag behaviours and 4 tests listed; no runtime test names fsopen, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fsopen`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsopen&body=Discussion+for+the+%60fsopen%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsopen.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsopen%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fsopen" (https://github.com/rrnewton/hermit/issues/362), legacy variant A2.

It was ranked 83 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

### Note on the strace evidence below

The strace text below was written on 2026-07-24 against the host kernel, not through Hermit. At hermit f4055eb9cae4 Hermit never forwards `fsopen`; it returns `EPERM` itself (`detcore/src/lib.rs:2215-2219`), the same errno the host gave the unprivileged probe.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fsopen -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(430, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fsopen(0xffffffffffffffff, FSOPEN_CLOEXEC|0xfffffffe) = -1 EPERM (Operation not permitted)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
