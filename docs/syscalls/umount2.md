---
schema: syscall-pages/v1
syscall: umount2
nr_x86_64: 166
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/504
  - https://github.com/rrnewton/hermit/issues/525
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+umount2&body=Discussion+for+the+%60umount2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fumount2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60umount2%60%2C+or+in+what+the+page+says%3F%0A
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

`umount2(target, flags)` detaches the mount at `target`, optionally forcing it (`MNT_FORCE`), detaching it lazily (`MNT_DETACH`), expiring it (`MNT_EXPIRE`) or refusing to follow a final symlink (`UMOUNT_NOFOLLOW`). Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219` at hermit f4055eb9cae4), so the guest cannot change the mount table Hermit's container set up.

Manual reference: [umount2(2)](https://man7.org/linux/man-pages/man2/umount.2.html).

## Linux semantics

The kernel (`fs/namespace.c`, `ksys_umount`/`path_umount`) checks in this order:

1. Unknown bits in `flags` give `EINVAL`.
2. `target` is resolved, following a final symlink unless `UMOUNT_NOFOLLOW` is set. A bad pointer gives `EFAULT`; a missing path gives `ENOENT`. Both host probes under Additional information stop here.
3. The caller needs `CAP_SYS_ADMIN` over its mount namespace (`may_mount`), otherwise `EPERM`. Hermit's ptrace guest is root of its own user namespace (`detcore/src/syscalls/misc.rs:672`), so on Linux it would pass this check for mounts in its own mount namespace.
4. `target` must be the root of a mount in the caller's namespace, otherwise `EINVAL`. A mount locked by a less privileged namespace boundary (one the guest inherited from Hermit's parent namespace) gives `EINVAL`.
5. `MNT_FORCE` additionally needs `CAP_SYS_ADMIN` in the initial user namespace, otherwise `EPERM`.

Then the mount is detached. Without `MNT_DETACH`, a mount still in use (open files, a working directory, child mounts) gives `EBUSY`. With `MNT_DETACH` it disappears from the namespace at once and is freed when the last reference goes. Unmounting a shared mount also unmounts its propagated copies in peer namespaces.

### Flags and options

- F1: `flags = 0`: plain unmount; `EBUSY` if the mount is in use.
- F2: `MNT_FORCE`: ask the filesystem to abort pending I/O (meaningful for NFS and FUSE); needs `CAP_SYS_ADMIN` in the initial user namespace, otherwise `EPERM`.
- F3: `MNT_DETACH`: lazy unmount; the mount leaves the namespace immediately and is torn down when no longer busy.
- F4: `MNT_EXPIRE`: mark the mount expired; the first call returns `EAGAIN`, and a second call with no use in between unmounts it. Combined with `MNT_FORCE` or `MNT_DETACH` it gives `EINVAL`.
- F5: `UMOUNT_NOFOLLOW`: do not follow a symlink as the last component of `target`.
- F6: any other bit in `flags`: `EINVAL` before the path is resolved.
- F7: target errors: `ENOENT` for a missing path (both host probes, with flags 0 and with `MNT_DETACH`), `EFAULT` for a bad pointer, `ENAMETOOLONG`, `EINVAL` when `target` is not a mount point or is locked, and `EPERM` without the capability.

## Determinism analysis

`linux_determinism: conditional`, kept from both legacy issues. Without privilege, or with a bad path or bad flags, the call fails with an error that depends only on the arguments and the paths, so it repeats; the probes show `ENOENT` with no state change. With privilege it succeeds and changes the mount-namespace topology and path resolution for every process in the namespace, and through propagation for peer namespaces. Whether it succeeds then depends on whether the filesystem is busy, which depends on what other processes have open, and `MNT_EXPIRE` depends on use since the previous call.

### Nondeterminism sources

- Mount namespace contents and propagation (shared, slave and private peers)
- Capabilities: `CAP_SYS_ADMIN` over the mount namespace, and in the initial namespace for `MNT_FORCE`
- Open references and working directories on the mount, which decide `EBUSY`, and use since the last `MNT_EXPIRE` call
- Concurrent mounts and unmounts by other processes

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the code already realizes it:

- `classify_syscall` marks `umount2` Determinized in the mount and namespace administration group (`detcore/src/syscall_classification.rs:339-369`, reviewed under the code tag `TODO-HUMAN-REVIEW(#724)`, https://github.com/rrnewton/hermit/issues/724). The comment there says the container pins the guest's namespaces and mount hierarchy for the run.
- `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`) includes it, and `handle_syscall_event` returns `Err(Errno::EPERM)` before the typed match (`detcore/src/lib.rs:2215-2219`). The arm reads neither argument and is not gated by `--strict`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`), so `code_facts.refused` is true.
- The only test is the unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`), which checks classification and membership and runs no guest.

Why `refuse`:

- `umount2` is the inverse of `mount`, which Hermit also refuses (same arm). Allowing unmounts alone would let a guest remove the `/proc`, `/tmp` or other mounts Hermit set up, changing what later path lookups reach, while giving it no way to put them back.
- Success would depend on host-visible state (`EBUSY` from open references, `EAGAIN` from expiry), which Detcore does not track.
- `model` would only make sense together with a modelled mount table for `mount`; see the open question on `mount`.

Known differences from Linux:

- On Linux the guest, as root of its own namespaces, could unmount a mount it created or one not locked by the namespace boundary. Under Hermit every request fails with `EPERM`. `umount(8)` and cleanup code in test suites see that error.
- Hermit returns `EPERM` before checking `flags` or resolving `target`, so F6 and the `ENOENT`/`EFAULT`/`EINVAL` cases of F7 give `EPERM` under Hermit.
- `capget` is passed through (`detcore/src/syscall_classification.rs:716`), so the guest may see `CAP_SYS_ADMIN` and still get `EPERM`.

Target (owner decision 6): keep the fixed `EPERM`, returned before `flags` or `target` is read, which is what the code does today (`detcore/src/lib.rs:2215-2219`). Linux's order is `ksys_umount` in `fs/namespace.c`: unknown `flags` bits give `EINVAL`, `user_path_at` resolves `target` (`ENOENT`, `EFAULT`), then `path_umount` calls `can_umount`, which checks `may_mount()` (`EPERM`) before "not a mount point", "not in this namespace" and `MNT_LOCKED` (all `EINVAL`) and the `MNT_FORCE` capability check (`EPERM`); the `MNT_EXPIRE` combinations are rejected later, in `do_umount`. Hermit deliberately does not reproduce the flag and path errors that come before the privilege check. `umount(8)` and cleanup code in test suites see the permission error an unprivileged Linux caller gets for an existing path.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): under `hermit run --verify` on ptrace, a guest calls `syscall(SYS_umount2, ...)` on `/proc` and on `/tmp` with flags 0, `MNT_FORCE`, `MNT_DETACH`, `MNT_EXPIRE` and `UMOUNT_NOFOLLOW`. Each must return -1 with `EPERM`, `/proc/self/mountinfo` read before and after must be identical, and `/proc/self/status` must still be readable afterwards.
- T2 (covers F6, F7): call it with an unknown flag bit (`0x80000000`), `MNT_EXPIRE|MNT_DETACH`, a missing path such as `/nonexistent-mnt`, `target = (char *)-1`, and a directory that is not a mount point. Each returns the fixed `EPERM` of owner decision 6 (Linux gives `EINVAL`, `ENOENT` and `EFAULT` for the first, third and fourth to any caller, and `EINVAL` for the second and fifth only to a caller that passes `may_mount()`), and both `--verify` runs agree.
- T3 (covers F1, F3): `hermit record` then `hermit replay` a shell guest that runs `umount /tmp` and `umount -l /tmp` (util-linux) and prints their exit status and stderr; both runs and the replay must match, and `/tmp` must still be mounted.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/504: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/525 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215 for every umount2 request, never forwarded); linux_determinism stays conditional; duplicate https://github.com/rrnewton/hermit/issues/525 merged (its NEEDS-HANDLER and this issue's NEEDS-INVESTIGATION both resolved as refuse); 7 flag behaviours and 3 tests listed; no coverage token because no test runs a guest that calls umount2, so tracking_status stays todo; open question on errno order under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (fixed EPERM before any argument check stays the target; Linux order in ksys_umount/can_umount deliberately not reproduced); target_policy stays refuse; T2 now asserts the decision-6 errno; no new F/T ids; open question on checking flags and path first removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `umount2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+umount2&body=Discussion+for+the+%60umount2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fumount2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60umount2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: umount2" (https://github.com/rrnewton/hermit/issues/504), legacy variant A1. Its comment history stays on that issue.

Merged duplicate: https://github.com/rrnewton/hermit/issues/525, filed 2026-07-24 as "Syscall classification: umount2" (legacy variant A1), retired into this issue. Its comment history (0 comments at migration) stays on that issue.

### Merge notes

- Both issues classified the call CONDITIONAL; the merged body keeps `conditional`.
- This issue recommended NEEDS-INVESTIGATION and the duplicate NEEDS-HANDLER, adding "Hermit should refuse or explicitly model it". Both are resolved as `refuse`, which is what the code at f4055eb9cae4 does.
- The duplicate's facts kept here: the call changes mount-namespace topology and path resolution and can affect other processes; its nondeterminism sources (mount namespace contents, capabilities, open references, propagation, busy state, concurrent mounts) are under Nondeterminism sources; the flag families it names (lazy detach, force, expiration, no-follow) are F2-F5; its `MNT_DETACH` probe is in F7.
- Both issues said Hermit routed `umount2` through the UNCLASSIFIED temporary pass-through block (the duplicate at `dd60278f`). That was true when filed and is superseded: at f4055eb9cae4 the call is Determinized and refused. The code at the cited SHA wins.
- This issue's note "Usually EPERM/EINVAL/ENOENT under hermit" described the pass-through era; at f4055eb9cae4 the answer is always `EPERM`.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/504)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=umount2` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit then routed `umount2` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
umount2("/nonexistent-mnt", 0)  = -1 ENOENT (No such file or directory)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence (duplicate https://github.com/rrnewton/hermit/issues/525)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=umount2`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `umount2` in the UNCLASSIFIED temporary-passthrough block. The nonexistent mount probe returned ENOENT without changing state.

Observed strace line:
```text
umount2("/tmp/hermit-no-mount-b5", MNT_DETACH) = -1 ENOENT (No such file or directory)
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
