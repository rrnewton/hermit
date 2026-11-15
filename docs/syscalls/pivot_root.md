---
schema: syscall-pages/v1
syscall: pivot_root
nr_x86_64: 155
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/321
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pivot_root&body=Discussion+for+the+%60pivot_root%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpivot_root.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pivot_root%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_privileged_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`pivot_root(new_root, put_old)` makes `new_root` the root mount of the caller's mount namespace and moves the old root to `put_old`. Hermit refuses it: Detcore returns a fixed `EPERM` without forwarding the call or reading its arguments (`detcore/src/lib.rs:2164`), consistent with its refusal of `mount`, `umount2`, `unshare` and `setns`.

Manual reference: [pivot_root(2)](https://man7.org/linux/man-pages/man2/pivot_root.2.html).

## Linux semantics

Container runtimes use `pivot_root` to switch a new mount namespace onto its root filesystem, usually followed by a lazy `umount2` of the old root.

- The capability check is `CAP_SYS_ADMIN` in the user namespace that owns the caller's mount namespace. Root inside a user namespace that owns its mount namespace passes it.
- On success the old root mount is moved to `put_old`, `new_root` becomes the root mount, and the kernel also moves the root directory and current directory of every process in the namespace that pointed at the old root. Processes outside the caller are affected.
- The order of the capability check and path resolution differs between kernels. The legacy probe on Linux 6.17 returned `EPERM` for two nonexistent paths. A 2026-10-03 probe on Linux 7.1.3, unprivileged, returned `ENOENT` for a nonexistent `new_root` and `EPERM` for existing directories (`/tmp`, `/tmp` and `/`, `/`). On that kernel the paths are resolved before the capability is checked.

### Flags and options

- F1: successful pivot: `new_root` and `put_old` are directories, `new_root` is a mount point, `put_old` is at or under `new_root`, neither is on the current root mount, and no involved mount has shared propagation. The old root moves to `put_old`.
- F2: the `pivot_root(".", ".")` idiom: with the working directory at `new_root`, the old root is stacked on top of the new one and the caller then unmounts it, so no `put_old` directory is needed.
- F3: invalid layout, privileged caller: `EINVAL` when `new_root` is not a mount point, `put_old` is not at or under `new_root`, the current root is not a mount point (after `chroot`) or is on rootfs, or a mount involved has shared propagation; `EBUSY` when `new_root` or `put_old` is on the current root mount.
- F4: path errors: `ENOENT`, `ENOTDIR` (either path is not a directory), `EFAULT`, `EACCES`, `ELOOP`, `ENAMETOOLONG`.
- F5: no `CAP_SYS_ADMIN` over the mount namespace: `EPERM`. Whether F4 errors win over this depends on the kernel version (see above).
- F6: side effect on other processes: their root and current directory move with the pivot when they pointed at the old root.

## Determinism analysis

`linux_determinism: conditional`. Unprivileged, the result is an error that is fixed for a given kernel and path set, but which error it is depends on the kernel version (`EPERM` on 6.17, `ENOENT` for a missing path on 7.1.3) and on which host paths exist. Privileged, the outcome depends on the host mount table and propagation settings, and the pivot changes the root and working directory of other processes in the namespace concurrently with whatever they are doing.

### Nondeterminism sources

- `CAP_SYS_ADMIN` over the caller's mount namespace. Verified on the legacy host: unprivileged, it fails with `EPERM`.
- The host mount namespace: which mounts exist and their propagation
- The kernel version, which orders path resolution and the capability check differently
- Other processes in the namespace whose root or working directory moves

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the dispatch arm at `detcore/src/lib.rs:2164-2168` returns `Err(Error::Errno(Errno::EPERM))` for every syscall in `is_privileged_admin_refused_syscall` (`detcore/src/syscall_classification.rs:902`), which lists `pivot_root` among 16 privileged administration calls. The arm runs before the typed match and before any `record_or_replay` call: it reads neither path, never forwards the call, and record, replay and both `--verify` runs compute the same `EPERM`. The refusal applies in every mode, not only when Hermit fails closed. The rationale comments are at `detcore/src/lib.rs:2154-2163` and `detcore/src/syscall_classification.rs:889-901`, tagged `TODO-HUMAN-REVIEW(#722)` (https://github.com/rrnewton/hermit/issues/722).

Why `refuse`:

- Detcore pins the guest's mount hierarchy for the whole run and refuses every call that could change it: `mount`, `umount2`, `move_mount`, `open_tree`, the `fs*` mount API, `unshare` and `setns` (`is_mount_ns_admin_refused_syscall`, `detcore/src/syscall_classification.rs:939-957`). A guest therefore cannot create a mount namespace or mount a new root to pivot onto, and accepting `pivot_root` alone would be incoherent.
- A forwarded pivot would rewrite the mount namespace Hermit set up for the guest, and move the root and working directory of every guest process at once, beneath Detcore's file and path tracking.
- A fixed `EPERM` removes the kernel-version dependence of the native errno. It is a deliberate difference: on a 7.1-class kernel an unprivileged process gets `ENOENT` for a missing `new_root`, while Hermit returns `EPERM` for every argument.

The code already realizes the policy. The unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks only that `pivot_root` is classified Determinized and is in the refusal set; it does not execute the syscall, and as a unit test it cannot serve as a coverage path.

## Recommended tests

- T1 (covers F1, F2, F5, F6): under `hermit run --verify` on ptrace, a guest calls `pivot_root` with two existing directories and, after `chdir("/")`, with `(".", ".")`. Each call must return -1 with `EPERM`. Afterwards the guest and a child it forked before the calls must still see the same `/` listing and the same working directory.
- T2 (covers F3, F4): `pivot_root` with a nonexistent `new_root`, with a regular file as `put_old`, with `new_root == "/"`, and with a NULL pointer: all must return `EPERM`. On a 7.1-class host the nonexistent-path case tells the refusal from forwarding, because the native unprivileged result there is `ENOENT`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/321#issuecomment-5872909530
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/321: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; pivot_root records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/321 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (fixed EPERM at detcore/src/lib.rs:2164-2168, never forwarded, consistent with the mount-namespace refusal set); linux_determinism stays conditional (native errno order differs between Linux 6.17 and 7.1.3); 6 flag behaviours and 2 tests listed; no coverage token, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pivot_root`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pivot_root&body=Discussion+for+the+%60pivot_root%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpivot_root.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pivot_root%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pivot_root" (https://github.com/rrnewton/hermit/issues/321), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/321.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=pivot_root`, and by confirming hermit currently routes `pivot_root` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
pivot_root("/nonexistent-a", "/nonexistent-b") = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/321#issuecomment-5872909530

### Related rr test

The upstream rr test `rr_pivot_root` is listed at `tests/BUCK:576` in the `exclude` list of the Buck `wrap_test_suite` that starts at `tests/BUCK:533`, under the comment `# verify failures, i.e. non-deterministic` (`tests/BUCK:571`). It is a legacy Buck entry, not a ptrace CI cell, and records only that the test was excluded.

### 2026-10-03 host probe (syscall-tracking-p3)

Linux 7.1.3, uid 212630 with an empty effective capability set, raw `syscall(155, ...)` through Python ctypes:

```text
pivot_root("/nonexistent-a", "/nonexistent-b") = -1 ENOENT
pivot_root("/nonexistent-a", "/tmp")           = -1 ENOENT
pivot_root("/tmp", "/tmp")                     = -1 EPERM
pivot_root("/", "/")                           = -1 EPERM
```
