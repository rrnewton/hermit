---
schema: syscall-pages/v1
syscall: move_mount
nr_x86_64: 429
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/296
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+move_mount&body=Discussion+for+the+%60move_mount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmove_mount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60move_mount%60%2C+or+in+what+the+page+says%3F%0A
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

`move_mount(from_dfd, from_path, to_dfd, to_path, flags)` moves an attached mount to a new location, or attaches a detached mount tree made by `open_tree(OPEN_TREE_CLONE)` or `fsmount` (new mount API, Linux 5.2). Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219`).

## Linux semantics

The source is named by `from_dfd`/`from_path` and the destination by `to_dfd`/`to_path`. If the source is an attached mount, the call moves it, like `mount --move`. If the source is a detached tree held by a descriptor, the call attaches it at the destination.

The caller needs `CAP_SYS_ADMIN` over its mount namespace. The kernel checks this before it looks at the flags or paths, so an unprivileged caller gets `EPERM` for any arguments; the host strace under Additional information, `move_mount(-1, "", -1, "", 0) = -1 EPERM`, is consistent with that. A privileged caller then gets `EINVAL` for flag bits outside `MOVE_MOUNT__MASK`, or for `MOVE_MOUNT_SET_GROUP` together with `MOVE_MOUNT_BENEATH`, and the usual path-resolution errors.

When the destination is under a shared mount, the kernel propagates copies of the new mount to every peer mount, which can be in other mount namespaces.

### Flags and options

- F1: flags 0 with an attached source: move the mount to the destination.
- F2: a detached source tree (a descriptor from `open_tree(OPEN_TREE_CLONE)` or `fsmount`, named with `MOVE_MOUNT_F_EMPTY_PATH`): attach the tree at the destination.
- F3: `MOVE_MOUNT_F_SYMLINKS`: follow a trailing symlink in `from_path`.
- F4: `MOVE_MOUNT_F_AUTOMOUNTS`: trigger an automount on the final component of `from_path`.
- F5: `MOVE_MOUNT_F_EMPTY_PATH`: an empty `from_path` refers to `from_dfd` itself.
- F6: `MOVE_MOUNT_T_SYMLINKS`: follow a trailing symlink in `to_path`.
- F7: `MOVE_MOUNT_T_AUTOMOUNTS`: trigger an automount on the final component of `to_path`.
- F8: `MOVE_MOUNT_T_EMPTY_PATH`: an empty `to_path` refers to `to_dfd` itself.
- F9: `MOVE_MOUNT_SET_GROUP` (Linux 5.15): copy the source's sharing group to the destination instead of moving a mount.
- F10: `MOVE_MOUNT_BENEATH` (Linux 6.5): mount the source beneath the top mount at the destination. Combined with `MOVE_MOUNT_SET_GROUP` it is `EINVAL`.
- F11: missing `CAP_SYS_ADMIN`: `EPERM`, checked before every other argument. Bits outside `MOVE_MOUNT__MASK`: `EINVAL` for a privileged caller.
- F12: mount propagation: a mount moved or attached under a shared mount is copied to the peer mounts.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Without the capability the call fails with `EPERM` deterministically. With it, the outcome depends on the host mount namespace, on propagation to peer mounts, and on which paths exist, and the call changes global mount state that other processes see.

Under Hermit the call returns `EPERM` without consulting the host, so it is deterministic by construction.

### Nondeterminism sources

- Requires `CAP_SYS_ADMIN`; the outcome depends on the host mount namespace. Verified on 2026-07-24: a bad-argument call fails with `EPERM` deterministically.
- Mount propagation to peer mounts, possibly in other namespaces
- Automount triggers on either path, when requested

## Hermit design

Target policy: `refuse` (from `investigate`). What the code does at hermit f4055eb9cae4:

- `move_mount` is a member of `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`, member at line 945) and is classified Determinized (`detcore/src/syscall_classification.rs:358`).
- The dispatch arm at `detcore/src/lib.rs:2215-2219` matches that predicate before the typed match and returns `Err(Error::Errno(Errno::EPERM))` for every argument combination. Its comment (`detcore/src/lib.rs:2203-2214`) says the call is never forwarded to the host.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`). The arm has no `panic_on_unsupported_syscalls` branch, so the refusal is the same whether or not Hermit fails closed.
- The unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`, member at line 2674) checks only the classification and the predicate membership; it does not run a guest.

Why `refuse`: the predicate's doc comment (`detcore/src/syscall_classification.rs:926-938`) says a deterministic container pins the guest's mount hierarchy for the whole run. A forwarded `move_mount` would change the filesystem view under paths Detcore already tracks, and with propagation it could change mounts outside the guest. Modelling it would need a virtual mount table that Detcore does not have.

How this compares with Linux: an unprivileged Linux caller gets `EPERM` for every argument combination (F11), so Hermit's answer matches unprivileged Linux exactly. Under the ptrace backend the guest runs as uid 0 inside a user namespace (`detcore/src/syscalls/misc.rs:672`); if that namespace owns the guest's mount namespace, the kernel would grant the call, so there `EPERM` is a policy refusal rather than the kernel's answer. Because `open_tree` and `fsmount` are refused too, a guest cannot obtain a detached tree for F2.

## Recommended tests

- T1 (covers F1, F3, F4, F6, F7, F11): under `hermit run --verify` on ptrace, call raw `syscall(SYS_move_mount, ...)` to move one directory onto another with flags 0 and with each of `MOVE_MOUNT_F_SYMLINKS`, `MOVE_MOUNT_F_AUTOMOUNTS`, `MOVE_MOUNT_T_SYMLINKS` and `MOVE_MOUNT_T_AUTOMOUNTS`. Each call returns -1 with `EPERM`, `/proc/self/mountinfo` read before and after is byte-identical, and both runs agree.
- T2 (covers F2, F5, F8): with an `O_PATH` descriptor for a directory as the source (a real detached tree cannot be obtained under Hermit), call with `MOVE_MOUNT_F_EMPTY_PATH`, `MOVE_MOUNT_T_EMPTY_PATH` and both, using empty paths. Each returns `EPERM` and mountinfo is unchanged.
- T3 (covers F9, F10, F11, F12): `MOVE_MOUNT_SET_GROUP`, `MOVE_MOUNT_BENEATH`, both together, and an undefined bit such as `0x8`: all return `EPERM`, the same as unprivileged Linux. With a shared mount as the destination, mountinfo is unchanged afterwards, so nothing propagated.
- T4 (covers none): record a guest that calls `move_mount` and replay it; replay returns the same `EPERM` and the guest's output matches.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/296#issuecomment-5872853014
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/296: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; move_mount records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/296 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215-2219, which matches unprivileged Linux for every argument because the capability check runs first); linux_determinism stays conditional; 12 flag behaviours and 4 tests listed; no runtime test names move_mount, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `move_mount`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+move_mount&body=Discussion+for+the+%60move_mount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmove_mount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60move_mount%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: move_mount" (https://github.com/rrnewton/hermit/issues/296), legacy variant A1.

### Note on the strace evidence below

The strace text below was written on 2026-07-24, when `move_mount` was forwarded to the kernel. At hermit f4055eb9cae4 Hermit no longer forwards it; it returns `EPERM` itself (`detcore/src/lib.rs:2215-2219`). The errno happens to be the same one the host gave the unprivileged probe.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/296.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=move_mount`, and by confirming hermit currently routes `move_mount` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
move_mount(-1, "", -1, "", 0)   = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/296#issuecomment-5872853014
