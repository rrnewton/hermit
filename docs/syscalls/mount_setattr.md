---
schema: syscall-pages/v1
syscall: mount_setattr
nr_x86_64: 442
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/295
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mount_setattr&body=Discussion+for+the+%60mount_setattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmount_setattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mount_setattr%60%2C+or+in+what+the+page+says%3F%0A
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

`mount_setattr(dfd, path, flags, attr, size)` changes the properties of an existing mount, or with `AT_RECURSIVE` of a whole mount subtree: mount attributes such as read-only, the propagation type, and idmapping (Linux 5.12). Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219`).

## Linux semantics

The kernel resolves `path` relative to `dfd` and applies the changes described by `struct mount_attr` (`attr_set`, `attr_clr`, `propagation`, `userns_fd`), whose size the caller passes in `size`. With `AT_RECURSIVE` the change applies to every mount in the subtree, and either all of them change or none does.

Argument shape is checked before privilege:

1. Flag bits other than `AT_EMPTY_PATH`, `AT_RECURSIVE`, `AT_SYMLINK_NOFOLLOW` and `AT_NO_AUTOMOUNT` give `EINVAL`.
2. A `size` larger than a page gives `E2BIG`; a `size` smaller than `MOUNT_ATTR_SIZE_VER0` (32) gives `EINVAL`. The host strace under Additional information, `mount_setattr(-1, "", 0, NULL, 0) = -1 EINVAL`, shows the size check running before any privilege check.

Changing a mount needs `CAP_SYS_ADMIN` over its mount namespace, otherwise `EPERM`. Bytes of a larger structure beyond the fields the kernel knows must be zero (`E2BIG`). An `attr` with nothing to set, clear or propagate is a no-op that returns 0. Unknown attribute bits, or a `propagation` value other than 0 or one of `MS_SHARED`, `MS_SLAVE`, `MS_PRIVATE` and `MS_UNBINDABLE`, give `EINVAL`. Idmapping has further conditions on `userns_fd` and on the mount and filesystem.

### Flags and options

- F1: `AT_RECURSIVE`: apply the change to the whole mount subtree, atomically.
- F2: `AT_EMPTY_PATH`: an empty `path` refers to `dfd` itself.
- F3: `AT_SYMLINK_NOFOLLOW`: do not follow a trailing symlink in `path`.
- F4: `AT_NO_AUTOMOUNT`: do not trigger an automount on the final component.
- F5: `attr_set`/`attr_clr` with `MOUNT_ATTR_RDONLY`, `MOUNT_ATTR_NOSUID`, `MOUNT_ATTR_NODEV`, `MOUNT_ATTR_NOEXEC`, `MOUNT_ATTR_NODIRATIME` or `MOUNT_ATTR_NOSYMFOLLOW`: set or clear that attribute.
- F6: atime mode: with `MOUNT_ATTR__ATIME` in `attr_clr`, set `MOUNT_ATTR_RELATIME`, `MOUNT_ATTR_NOATIME` or `MOUNT_ATTR_STRICTATIME` in `attr_set`.
- F7: `MOUNT_ATTR_IDMAP` with `userns_fd`: idmap the mount through the user namespace that `userns_fd` refers to.
- F8: `propagation`: `MS_SHARED`, `MS_SLAVE`, `MS_PRIVATE` or `MS_UNBINDABLE` changes the propagation type; 0 leaves it unchanged.
- F9: `size` and structure version: below 32 is `EINVAL`, above a page is `E2BIG`, non-zero bytes beyond the known fields are `E2BIG`; an all-zero `attr` is a no-op that returns 0.
- F10: other errors: undefined flag or attribute bits, or an invalid `propagation` value, are `EINVAL`; missing `CAP_SYS_ADMIN` is `EPERM`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Malformed arguments fail deterministically (`EINVAL`, `E2BIG`), and an unprivileged caller with well-formed arguments fails with `EPERM`. With privilege, the result depends on the host mount namespace and changes mount state that other processes see, and propagation changes affect peer mounts.

Under Hermit the call returns `EPERM` without consulting the host, so it is deterministic by construction.

### Nondeterminism sources

- Requires `CAP_SYS_ADMIN`; the effect depends on the host mount namespace. Verified on 2026-07-24: a bad-argument call fails with `EINVAL` deterministically.
- Propagation changes, which affect peer mounts in other namespaces
- Automount triggers on the final component, unless `AT_NO_AUTOMOUNT`

## Hermit design

Target policy: `refuse` (from `investigate`). What the code does at hermit f4055eb9cae4:

- `mount_setattr` is a member of `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`, member at line 944) and is classified Determinized (`detcore/src/syscall_classification.rs:357`).
- The dispatch arm at `detcore/src/lib.rs:2215-2219` matches that predicate before the typed match and returns `Err(Error::Errno(Errno::EPERM))` for every argument combination. Its comment (`detcore/src/lib.rs:2203-2214`) says the call is never forwarded to the host.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`). The arm has no `panic_on_unsupported_syscalls` branch, so the refusal is the same whether or not Hermit fails closed.
- The unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`, member at line 2673) checks only the classification and the predicate membership; it does not run a guest.

Why `refuse`: the predicate's doc comment (`detcore/src/syscall_classification.rs:926-938`) says a deterministic container pins the guest's mount hierarchy for the whole run. A forwarded `mount_setattr` could make paths Detcore tracks read-only or non-executable mid-run, and a propagation change could affect mounts outside the guest. Modelling it would need a virtual mount table that Detcore does not have.

Where Hermit differs from Linux:

- F9, F10: natively a malformed call fails with `EINVAL` or `E2BIG` before any privilege check; Hermit returns `EPERM`. The host-strace case (size 0) gives `EINVAL` natively and `EPERM` under Hermit.
- F1 to F8: under the ptrace backend the guest runs as uid 0 inside a user namespace (`detcore/src/syscalls/misc.rs:672`). If that namespace owns the guest's mount namespace, the kernel would grant changes to mounts it owns, so `EPERM` is a policy refusal there rather than the kernel's answer.

Target (owner decision 6): keep the fixed `EPERM`, returned before any argument is read, which is what the code does today (`detcore/src/lib.rs:2215-2219`). Linux's order, in `SYSCALL_DEFINE5(mount_setattr)` in `fs/namespace.c`, checks the flag bits and then `size` before privilege (F9, F10); Hermit deliberately does not reproduce it, because the call is refused, not emulated. A guest that uses it (a container runtime making a mount read-only or idmapped) gets the permission error an unprivileged Linux caller gets for well-formed arguments.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F8): under `hermit run --verify` on ptrace, call raw `syscall(SYS_mount_setattr, ...)` on `/` with a 32-byte `struct mount_attr` that sets each attribute, each atime mode and each propagation type, with and without `AT_RECURSIVE`, and on an open directory descriptor with `""` and `AT_EMPTY_PATH`, plus `AT_SYMLINK_NOFOLLOW` and `AT_NO_AUTOMOUNT` on a symlink. Each call returns -1 with `EPERM`, `/proc/self/mountinfo` read before and after is byte-identical, and both runs agree.
- T2 (covers F7): `MOUNT_ATTR_IDMAP` with `userns_fd` from `open("/proc/self/ns/user")` returns `EPERM`, and mountinfo is unchanged.
- T3 (covers F9, F10): the host-strace case `mount_setattr(-1, "", 0, NULL, 0)`, `size` 4097, a 40-byte structure with a non-zero tail, an all-zero `attr`, an undefined flag bit and an undefined attribute bit all return `EPERM` under Hermit, where a privileged Linux caller gets `EINVAL`, `E2BIG`, `E2BIG`, 0, `EINVAL` and `EINVAL`; the size and flag-bit cases fail the same way for an unprivileged caller, because they are checked before privilege. The test pins the fixed errno of owner decision 6.
- T4 (covers none): record a guest that calls `mount_setattr` and replay it; replay returns the same `EPERM` and the guest's output matches.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/295#issuecomment-5872850636
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/295: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mount_setattr records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/295 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM for every argument combination at detcore/src/lib.rs:2215-2219 and never forwards the call); linux_determinism stays conditional; 10 flag behaviours and 4 tests listed; no runtime test names mount_setattr, so tracking_status stays todo and ptrace_verification untested; open question on native argument errnos
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (fixed EPERM before any argument check stays the target; Linux order in SYSCALL_DEFINE5(mount_setattr) deliberately not reproduced); target_policy stays refuse; T3 now pins the decision-6 errno; no new F/T ids; open question on native argument errnos removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mount_setattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mount_setattr&body=Discussion+for+the+%60mount_setattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmount_setattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mount_setattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mount_setattr" (https://github.com/rrnewton/hermit/issues/295), legacy variant A1.

### Note on the strace evidence below

The strace text below was written on 2026-07-24, when `mount_setattr` was forwarded to the kernel. At hermit f4055eb9cae4 Hermit no longer forwards it; it returns `EPERM` (`detcore/src/lib.rs:2215-2219`), so the same call under Hermit gives `EPERM`, not `EINVAL`.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/295.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mount_setattr`, and by confirming hermit currently routes `mount_setattr` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mount_setattr(-1, "", 0, NULL, 0) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/295#issuecomment-5872850636
