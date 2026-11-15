---
schema: syscall-pages/v1
syscall: open_tree
nr_x86_64: 428
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/314
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+open_tree&body=Discussion+for+the+%60open_tree%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopen_tree.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60open_tree%60%2C+or+in+what+the+page+says%3F%0A
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

`open_tree(dfd, path, flags)` returns a file descriptor for a location in the mount tree; with `OPEN_TREE_CLONE` the descriptor holds a new detached copy of the mount there, which `move_mount` can attach elsewhere (new mount API, Linux 5.2). Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219`).

## Linux semantics

`open_tree` resolves `path` relative to `dfd` and returns a descriptor, which is always the lowest free descriptor number.

- Without `OPEN_TREE_CLONE` the descriptor is an `O_PATH` descriptor for the resolved location. No privilege is needed.
- With `OPEN_TREE_CLONE` the kernel copies the mount at that location (with `AT_RECURSIVE`, the whole mount subtree below it) into a new detached tree and returns a descriptor that holds it. The clone needs `CAP_SYS_ADMIN` over the caller's mount namespace; without it the call fails with `EPERM`. The tree can be attached with `move_mount`; if the last reference to an unattached tree closes, the kernel unmounts it.

Errors: undefined flag bits, or `AT_RECURSIVE` without `OPEN_TREE_CLONE`, give `EINVAL`. Path resolution gives the usual `ENOENT`, `ENOTDIR`, `ELOOP` and `EACCES`. An empty `path` without `AT_EMPTY_PATH` gives `ENOENT`, which the host strace under Additional information shows: `open_tree(-1, "", 0) = -1 ENOENT`. Descriptor exhaustion gives `EMFILE` or `ENFILE`.

### Flags and options

- F1: no `OPEN_TREE_CLONE`: return an `O_PATH` descriptor for the resolved location. Unprivileged.
- F2: `OPEN_TREE_CLONE`: clone the mount at the location into a detached tree. Needs `CAP_SYS_ADMIN` over the caller's mount namespace, otherwise `EPERM`.
- F3: `AT_RECURSIVE` with `OPEN_TREE_CLONE`: clone the whole mount subtree. `AT_RECURSIVE` without `OPEN_TREE_CLONE` is `EINVAL`.
- F4: `OPEN_TREE_CLOEXEC` (equal to `O_CLOEXEC`): set close-on-exec on the returned descriptor.
- F5: `AT_EMPTY_PATH`: an empty `path` refers to `dfd` itself. Without it an empty path is `ENOENT`.
- F6: `AT_SYMLINK_NOFOLLOW`: do not follow a trailing symlink in `path`.
- F7: `AT_NO_AUTOMOUNT`: do not trigger an automount on the final component.
- F8: `dfd`: `AT_FDCWD` or a directory descriptor that a relative `path` is resolved against; ignored for an absolute `path`.
- F9: any other flag bit: `EINVAL`. The 7.1 kernel headers on this host also define `OPEN_TREE_NAMESPACE` (`/usr/src/kernels/7.1.3-0_fbk0_0_g295ad0959f34/include/uapi/linux/mount.h:65`), which clones the tree into a new mount namespace; kernels without it reject the bit with `EINVAL`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Natively the result depends on the host mount namespace (what exists and what is mounted at `path`), on the caller's capabilities for the clone mode, on automount triggers, and on the lowest free descriptor number. With the same filesystem and mount state, the same privileges and the same descriptor table, the call repeats.

Under Hermit none of these sources reach the guest: the call returns `EPERM` without consulting the host, so it is deterministic by construction.

### Nondeterminism sources

- The returned descriptor number, which must be virtualized if the call were ever forwarded
- The mount tree at `path`, which is host mount-namespace state
- Privilege: `OPEN_TREE_CLONE` needs `CAP_SYS_ADMIN`
- Automount triggers on the final component, unless `AT_NO_AUTOMOUNT`

## Hermit design

Target policy: `refuse` (from `investigate`). What the code does at hermit f4055eb9cae4:

- `open_tree` is a member of `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`, member at line 946) and is classified Determinized (`detcore/src/syscall_classification.rs:359`).
- The dispatch arm at `detcore/src/lib.rs:2215-2219` matches that predicate before the typed match and returns `Err(Error::Errno(Errno::EPERM))` for every argument combination. Its comment (`detcore/src/lib.rs:2203-2214`) says the call is never forwarded to the host.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`). The arm has no `panic_on_unsupported_syscalls` branch, so the refusal is the same whether or not Hermit fails closed.
- The unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`, member at line 2675) checks only the classification and the predicate membership; it does not run a guest.

Why `refuse`: the predicate's doc comment (`detcore/src/syscall_classification.rs:926-938`) says a deterministic container pins the guest's mount hierarchy for the whole run, and that the non-clone mode is refused deliberately because it would otherwise perturb that pinned state. Forwarding a clone would hand the guest a kernel mount object Detcore does not track, and its result would depend on host privilege and host mounts. Modelling the call would need a virtual mount table that Detcore does not have.

Where Hermit differs from Linux:

- F1: natively an unprivileged non-clone call succeeds and returns an `O_PATH` descriptor. Hermit returns `EPERM`, which Linux never returns for this mode.
- F3, F5, F9: natively a malformed call fails with `EINVAL` or `ENOENT`; Hermit returns `EPERM`.
- F2: under the ptrace backend the guest runs as uid 0 inside a user namespace (`detcore/src/syscalls/misc.rs:672`). If that namespace owns the guest's mount namespace, the kernel would grant the clone, so `EPERM` is a policy refusal there rather than the kernel's answer.

Target (owner decision 6): the clone mode stays refused and the non-clone mode becomes the decision's Linux-unprivileged exception. The kernel order relied on is `vfs_open_tree` in `fs/namespace.c` (called from `SYSCALL_DEFINE3(open_tree)`): undefined flag bits give `EINVAL`, then `AT_RECURSIVE` without `OPEN_TREE_CLONE` gives `EINVAL`, then, for the clone mode only, `may_mount()` gives `EPERM`, then `path` is resolved (`ENOENT`, `ENOTDIR`, `ELOOP`, `EACCES`), and the non-clone mode returns `dentry_open(path, O_PATH)` on a new descriptor.

- Any call with `OPEN_TREE_CLONE` set returns the fixed `EPERM` before any other check, as today (`detcore/src/lib.rs:2215-2219`), so `OPEN_TREE_CLONE` with an undefined bit also gets `EPERM`, where Linux gives `EINVAL`. A guest that clones a tree (a bind mount through the new mount API, as `mount(8)` or a container runtime does) gets the permission error an unprivileged Linux caller gets; `mount(2)`, the old route, is refused with the same `EPERM`.
- A call without `OPEN_TREE_CLONE` must succeed or fail as Linux does, with Linux's argument checks in the kernel order above: F1 returns an `O_PATH` descriptor, F3 and F9 give `EINVAL`, F5 and F8 resolve the path as Linux does, and F4, F6 and F7 behave as listed. This mode needs no privilege and creates no mount object, so it can go through the same path permission, `record_or_replay` and descriptor bookkeeping that `handle_openat` gives an `O_PATH` open (`detcore/src/syscalls/files.rs:979-1008`), so that later descriptor calls (`fstat`, `openat` relative to it, `move_mount` with `MOVE_MOUNT_F_EMPTY_PATH`) see a descriptor Detcore knows.
- Today the predicate `is_mount_ns_admin_refused_syscall` holds the whole syscall (`detcore/src/syscall_classification.rs:946`) and its doc comment calls the non-clone refusal deliberate (`detcore/src/syscall_classification.rs:926-938`); both change with this target. `target_policy` stays `refuse` because the clone mode, the mount-changing half, is refused.

## Recommended tests

- T1 (covers F1, F4, F5, F6, F7, F8): under `hermit run --verify` on ptrace, call raw `syscall(SYS_open_tree, ...)` without `OPEN_TREE_CLONE`: `AT_FDCWD, "/", 0`; a relative path against an open directory descriptor; that descriptor with `""` plus `AT_EMPTY_PATH`; `AT_SYMLINK_NOFOLLOW` and `AT_NO_AUTOMOUNT` on a symlink and on a directory; and `OPEN_TREE_CLOEXEC`. Each call returns the lowest free descriptor; `fcntl(F_GETFL)` shows `O_PATH`, `fcntl(F_GETFD)` shows `FD_CLOEXEC` only for the `OPEN_TREE_CLOEXEC` call, `fstat` on the descriptor matches `stat` (or `lstat` for the `AT_SYMLINK_NOFOLLOW` symlink case) of the path, and both runs agree.
- T2 (covers F2, F3, F4): `OPEN_TREE_CLONE`, `OPEN_TREE_CLONE|AT_RECURSIVE` and `OPEN_TREE_CLONE|OPEN_TREE_CLOEXEC` on `/` return `EPERM`, and `/proc/self/mountinfo` read before and after the calls is byte-identical.
- T3 (covers F3, F5, F9): malformed calls without `OPEN_TREE_CLONE` keep Linux's errno: `AT_RECURSIVE` alone and an undefined bit such as `0x80000000` return `EINVAL`, `""` without `AT_EMPTY_PATH` and a missing path return `ENOENT`. With `OPEN_TREE_CLONE`, the same undefined bit returns the fixed `EPERM` (owner decision 6). No descriptor is allocated by any failing call.
- T4 (covers none): record a guest that calls `open_tree` with and without `OPEN_TREE_CLONE` and replay it; replay returns the same descriptor numbers and the same `EPERM`, and the guest's output matches.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/314#issuecomment-5872892729
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/314: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; open_tree records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/314 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM for every argument combination at detcore/src/lib.rs:2215-2219 and never forwards the call); linux_determinism stays conditional; 9 flag behaviours and 4 tests listed; no runtime test names open_tree, so tracking_status stays todo and ptrace_verification untested; open questions on emulating the non-clone mode and on native argument errnos
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (clone mode keeps the fixed EPERM before any check; non-clone mode is the Linux-unprivileged exception and must return an O_PATH descriptor with Linux's checks in vfs_open_tree order); target_policy stays refuse; T1, T3 and T4 rewritten for the target, T1 now also covers F4; no new F/T ids; both open questions removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `open_tree`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+open_tree&body=Discussion+for+the+%60open_tree%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopen_tree.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60open_tree%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: open_tree" (https://github.com/rrnewton/hermit/issues/314), legacy variant A1.

### Note on the strace evidence below

The strace text below was written on 2026-07-24, when `open_tree` was forwarded to the kernel. At hermit f4055eb9cae4 Hermit no longer forwards it; it returns `EPERM` (`detcore/src/lib.rs:2215-2219`), so the same call under Hermit gives `EPERM`, not `ENOENT`.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/314.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=open_tree`, and by confirming hermit currently routes `open_tree` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
open_tree(-1, "", 0)            = -1 ENOENT (No such file or directory)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/314#issuecomment-5872892729
