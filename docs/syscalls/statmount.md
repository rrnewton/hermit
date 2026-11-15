---
schema: syscall-pages/v1
syscall: statmount
nr_x86_64: 457
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/481
  - https://github.com/rrnewton/hermit/issues/483
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+statmount&body=Discussion+for+the+%60statmount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstatmount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60statmount%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_mount_introspection_enosys_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`statmount(req, buf, bufsize, flags)` (Linux 6.8 and later) returns attributes and strings of one mount, named by its 64-bit unique mount id, for the fields selected in a request mask. Hermit refuses it: Detcore answers every call with `ENOSYS` before looking at the arguments and never calls the kernel (`detcore/src/lib.rs:2110-2114` at hermit f4055eb9cae4), which is what a pre-6.8 kernel returns and sends callers to `/proc/self/mountinfo`, a file Hermit sanitizes.

Manual reference: Linux man-pages statmount(2).

## Linux semantics

`statmount` and its companion `listmount` replace parsing `/proc/self/mountinfo`. The caller passes a `struct mnt_id_req` (`size`, `spare`, `mnt_id`, `param`, `mnt_ns_id`); for `statmount`, `param` is the request mask. The kernel fills a `struct statmount`: fixed fields first, then NUL-terminated strings in a variable tail whose offsets are stored in the fixed fields. On return, `buf->mask` says which groups were written and `buf->size` how much space was used. The call is read-only.

Layouts and constants below are from `/usr/include/linux/mount.h` (kernel-headers 6.17.4 on this host): `struct statmount` at line 155, `struct mnt_id_req` at line 198, the request sizes at lines 207-208 and the mask bits at lines 213-227.

The mount id is the 64-bit unique id (also reported by `statx` with `STATX_MNT_ID_UNIQUE`), not the reusable id printed in `mountinfo`; the old reusable ids are returned too, as `mnt_id_old` and `mnt_parent_id_old`. A mount the caller cannot reach from its root, or a mount in another namespace, needs privilege over that namespace.

The duplicate issue described `statmount` as taking "a mount ID or file descriptor". The 6.17.4 header on this host defines no flag for a file-descriptor form and no other `flags` value, so this body lists only the mount-id form. If a later kernel adds one, it is a new flag to add to the inventory.

### Flags and options

- F1: `req->size` is `MNT_ID_REQ_SIZE_VER0` (24) or `MNT_ID_REQ_SIZE_VER1` (32, which adds `mnt_ns_id`); any other size, or a nonzero reserved `spare`, gives `EINVAL`.
- F2: `req->mnt_id` names a mount by its unique id; an id that names no mount in the target namespace gives `ENOENT`.
- F3: `req->mnt_ns_id` (VER1 only) selects another mount namespace by its id; 0 means the caller's namespace. Querying another namespace, or a mount outside the caller's root, without privilege over it gives `EPERM`.
- F4: `STATMOUNT_SB_BASIC` (0x1): superblock device major/minor, `sb_magic`, `sb_flags`.
- F5: `STATMOUNT_MNT_BASIC` (0x2): `mnt_id`, `mnt_parent_id`, the old reusable ids, `mnt_attr`, `mnt_propagation`, `mnt_peer_group`, `mnt_master`.
- F6: `STATMOUNT_PROPAGATE_FROM` (0x4): the propagation source in the current namespace.
- F7: `STATMOUNT_MNT_ROOT` (0x8): string, the mount's root relative to the filesystem root.
- F8: `STATMOUNT_MNT_POINT` (0x10): string, the mount point relative to the caller's root.
- F9: `STATMOUNT_FS_TYPE` (0x20): string, the filesystem type.
- F10: `STATMOUNT_MNT_NS_ID` (0x40): the id of the mount's namespace.
- F11: `STATMOUNT_MNT_OPTS` (0x80): string, comma-separated mount options.
- F12: `STATMOUNT_FS_SUBTYPE` (0x100): string, the filesystem subtype, if any.
- F13: `STATMOUNT_SB_SOURCE` (0x200): string, the mount source.
- F14: `STATMOUNT_OPT_ARRAY` (0x400): count and array of filesystem options.
- F15: `STATMOUNT_OPT_SEC_ARRAY` (0x800): count and array of security options.
- F16: `STATMOUNT_SUPPORTED_MASK` (0x1000): the mask bits this kernel supports.
- F17: `STATMOUNT_MNT_UIDMAP` (0x2000) and `STATMOUNT_MNT_GIDMAP` (0x4000): the mount's id-mapping arrays as seen from the caller's namespace.
- F18: `bufsize` too small for the fixed part plus the requested strings: `EOVERFLOW`.
- F19: `flags` must be 0; any other value gives `EINVAL`.
- F20: `req` or `buf` not accessible: `EFAULT` (host evidence: `statmount(NULL, NULL, 0, 0)` returned `EFAULT`).
- F21: kernel older than 6.8: `ENOSYS`, the same errno Hermit returns for every call.

## Determinism analysis

`linux_determinism: nondeterministic`. Unique mount ids and mount-namespace ids come from kernel-wide counters that only grow, so any mount or namespace created at the start of a run, by a container setup or by the guest, gets a different id in every run, even on one host. The remaining fields describe the host's mount table: sources, options, device numbers, propagation peers and id maps differ across hosts and change whenever any process mounts or unmounts. Error results (F1, F19, F20) repeat, but the success path does not, for any argument.

The duplicate issue https://github.com/rrnewton/hermit/issues/483 classified `statmount` as CONDITIONAL, while this issue said nondeterministic. Nondeterministic is kept, because of the ever-increasing ids: no request mask that returns ids or mount-table state repeats across runs.

### Nondeterminism sources

- Unique mount ids and mount-namespace ids, allocated from kernel-wide increasing counters
- Mount-namespace contents: sources, options, filesystem types, propagation, id maps and device numbers
- Concurrent mounts and unmounts by any process sharing the namespace
- Capability checks against the caller's namespace (F3) and kernel version (F16, F21)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` lists `statmount` among the Determinized syscalls (`detcore/src/syscall_classification.rs:379`), and `is_mount_introspection_enosys_syscall` includes it (`detcore/src/syscall_classification.rs:1190-1200`). The doc comment (`detcore/src/syscall_classification.rs:1182-1189`) says `statmount` reads mount ids and topology, which are not part of a deterministic guest's modeled state.
- In `handle_syscall_event` (`detcore/src/lib.rs:1892`), the dispatch match (`detcore/src/lib.rs:2028`) returns `Err(Errno::ENOSYS)` for this predicate (`detcore/src/lib.rs:2110-2114`). It does not read `req` and does not inject the syscall, so `buf` is never written and the answer is the same for F1-F21.
- The arm does not consult `panic_on_unsupported_syscalls` (contrast the `rseq` arm at `detcore/src/lib.rs:2031-2037`), so the answer is `ENOSYS` with and without fail-closed mode, matching `refused_only_when_fail_closed: false`.
- The predicate is in the deterministic refusal set (`detcore/src/syscall_classification.rs:1331`); the unit test `mount_introspection_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2377`) pins the classification and the membership.

Why `refuse`:

- `ENOSYS` is what every kernel before 6.8 returns (F21), so callers such as libraries that prefer `statmount` must already fall back to `/proc/self/mountinfo`. Hermit sanitizes that file: reads of `/proc/<pid>/mountinfo` are recognized (`detcore/src/procfs.rs:684`) and rewritten by `sanitize_mountinfo` (`detcore/src/procfs.rs:3429`), with device numbers taken from the same run-global pool as `stat`/`statx` (`detcore/src/procfs.rs:491-498`). The fallback therefore gets a determinized view, and the refusal steers callers to it.
- `passthrough` would expose ever-increasing unique ids and host mount state.
- `model` would need a virtual mount table with stable unique ids, namespace ids and string fields, kept consistent with the `mountinfo` sanitizer and with `statx`'s `STATX_MNT_ID_UNIQUE`. That is a large design that no known workload needs while the fallback exists.

The legacy recommendation from both issues was NEEDS-HANDLER, and the duplicate said the output needs "sanitization, deterministic refusal, or recording". Deterministic refusal is what the code does. If a workload appears that needs `statmount` without a fallback, `model` built on the `mountinfo` snapshot is the path, and this body would be revisited.

## Recommended tests

- T1 (covers F1, F2, F5, F19): a VER1 request for mount id 1 with an empty request mask and `flags` 0 returns `ENOSYS` under `hermit run --verify` on ptrace, in both default and fail-closed (`--panic-on-unsupported-syscalls`) runs.
  Existing candidates: `tests/c/statmount_enosys.c` (it sends a 32-byte request with `mount_id = 1` and `request_mask = 0`) through cell `c-programs/statmount-enosys` (`tests/e2e/manifests/c-programs.yaml:9147`, verify `ci: ptrace: true`), and `mount_introspection_syscalls_fall_back_deterministically` at `hermit-cli/tests/mount_introspection.rs:30`, which runs that guest with `--backend=ptrace`, `--strict`, `--verify` and `--panic-on-unsupported-syscalls`. F5 is only nominally covered here, since the existing guest requests no mask bits.
- T2 (covers F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17): obtain the root's unique mount id with `statx(STATX_MNT_ID_UNIQUE)` and call `statmount` once per mask bit and once with all bits, on a buffer pre-filled with a sentinel, with `mnt_ns_id` 0 and with a made-up namespace id. Every call must return `ENOSYS`, the sentinel must be unchanged, and the two `--verify` runs must agree.
- T3 (covers F1, F18, F19, F20, F21): a 16-byte request, a 1-byte buffer, `flags` 1, and NULL `req`/`buf` each return `ENOSYS` rather than `EINVAL`, `EOVERFLOW` or `EFAULT`, and no buffer is written.
- T4 (covers F5, F8): a guest that tries `statmount` first and falls back to parsing `/proc/self/mountinfo` on `ENOSYS` prints the same mount list in both `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/481: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_introspection_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/483 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns ENOSYS for every call without touching the kernel, detcore/src/lib.rs:2110-2114, as a pre-6.8 kernel does; callers fall back to /proc/self/mountinfo, which detcore/src/procfs.rs:3429 sanitizes); linux_determinism stays nondeterministic (duplicate's CONDITIONAL reconciled); merged https://github.com/rrnewton/hermit/issues/483; 21 flag behaviours and 4 tests listed; existing ptrace candidates for T1 named but no run result seen and one request shape exercised, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `statmount`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+statmount&body=Discussion+for+the+%60statmount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fstatmount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60statmount%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: statmount" (https://github.com/rrnewton/hermit/issues/481), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/483, filed 2026-07-24 with the same title (legacy variant A1, batch 5 of the classification sweep), was merged into this body on 2026-10-03; its comment history (0 comments) stays on that issue, and this issue's comment history stays here.

Both legacy bodies said Hermit routed `statmount` through the UNCLASSIFIED temporary pass-through block of `detcore/src/syscall_classification.rs` (the duplicate cites `dd60278f`). That was true when they were filed and is superseded: at hermit f4055eb9cae4 `statmount` is Determinized and refused, as described under Hermit design.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=statmount` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `statmount` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
statmount(NULL, NULL, 0, 0)     = -1 EFAULT (Bad address)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

The duplicate https://github.com/rrnewton/hermit/issues/483 recorded the identical strace line (`statmount(NULL, NULL, 0, 0) = -1 EFAULT (Bad address)`) on `the measurement host` (Linux 6.17, x86_64, glibc), traced with `strace -qq -f -e trace=statmount`. Manual reference from the duplicate: [statmount(2)](https://man7.org/linux/man-pages/man2/statmount.2.html).
