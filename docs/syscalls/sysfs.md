---
schema: syscall-pages/v1
syscall: sysfs
nr_x86_64: 139
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/494
  - https://github.com/rrnewton/hermit/issues/515
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sysfs&body=Discussion+for+the+%60sysfs%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsysfs.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sysfs%60%2C+or+in+what+the+page+says%3F%0A
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

`sysfs(option, ...)` is the obsolete System V interface for enumerating the filesystem types registered in the kernel (it is unrelated to the `/sys` filesystem). Hermit refuses it: Detcore answers every call with `ENOSYS` before looking at the arguments and never calls the kernel (`detcore/src/lib.rs:2110-2114` at hermit f4055eb9cae4), which is also what a kernel built without `CONFIG_SYSFS_SYSCALL` returns.

Manual reference: Linux man-pages sysfs(2).

## Linux semantics

`sysfs` multiplexes three read-only queries on `option`. The kernel walks its list of registered filesystem types, the same list `/proc/filesystems` prints. An index is a position in that list: built-in types in registration order, then module-provided types in the order their modules were loaded. Option 2 copies the name without a length argument, so the caller's buffer must be large enough. glibc provides no wrapper; callers use `syscall(SYS_sysfs, ...)`.

The syscall exists only when the kernel is built with `CONFIG_SYSFS_SYSCALL`. Without it, every call fails with `ENOSYS`. On the 2026-07-24 host (Linux 6.17), option 3 returned `ENOSYS` (strace under Additional information), so that kernel was built without it.

### Flags and options

- F1: option 1, `sysfs(1, const char *fsname)`: returns the index of the registered filesystem type named `fsname`; `EINVAL` if no such type is registered; `EFAULT` if `fsname` is not readable.
- F2: option 2, `sysfs(2, unsigned fs_index, char *buf)`: copies the NUL-terminated name of the type at `fs_index` into `buf` and returns 0; `EINVAL` if `fs_index` is out of range; `EFAULT` if `buf` is not writable.
- F3: option 3, `sysfs(3)`: returns the number of registered filesystem types.
- F4: any other `option`: `EINVAL`.
- F5: kernel built without `CONFIG_SYSFS_SYSCALL`: every option fails with `ENOSYS`, the same errno Hermit returns for every call.

## Determinism analysis

`linux_determinism: conditional`. For one kernel build with a fixed set of loaded modules, every option repeats exactly. Across hosts the answers differ with kernel version, configuration and module set. Within one run they can change too: loading a filesystem module, which the kernel can do automatically on the first mount of a new type, appends a type, shifting the count (F3) and adding an index (F1, F2). Both legacy bodies said CONDITIONAL; they agree.

### Nondeterminism sources

- Kernel version and configuration (which types are built in, and whether `CONFIG_SYSFS_SYSCALL` is set at all)
- Module loading, by administrators or on demand, which adds types and changes indices during a run
- The command index chosen by the caller, which selects which of the above is observed

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` lists `sysfs` among the Determinized syscalls (`detcore/src/syscall_classification.rs:377`), and `is_mount_introspection_enosys_syscall` includes it (`detcore/src/syscall_classification.rs:1190-1200`). The doc comment (`detcore/src/syscall_classification.rs:1182-1189`) says `sysfs` reads the host's filesystem-type table, which is not part of a deterministic guest's modeled state.
- In `handle_syscall_event` (`detcore/src/lib.rs:1892`), the dispatch match (`detcore/src/lib.rs:2028`) returns `Err(Errno::ENOSYS)` for this predicate (`detcore/src/lib.rs:2110-2114`). It does not decode `option` or the pointers and does not inject the syscall, so no guest buffer is written and the answer is the same for F1-F5.
- The arm does not consult `panic_on_unsupported_syscalls` (contrast the `rseq` arm at `detcore/src/lib.rs:2031-2037`), so the answer is `ENOSYS` with and without fail-closed mode, matching `refused_only_when_fail_closed: false`.
- The predicate is in the deterministic refusal set (`detcore/src/syscall_classification.rs:1331`); the unit test `mount_introspection_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2377`) pins the classification and the membership.

Why `refuse`:

- `ENOSYS` is exactly what a kernel without `CONFIG_SYSFS_SYSCALL` returns (F5), and the 2026-07-24 host kernel already behaves that way, so callers must already cope with it.
- `passthrough` would expose the host's type table, which differs across hosts and changes with module loading.
- `model` or `emulate` would need a fixed canonical type table, for an interface glibc does not wrap and that no known workload uses.

The legacy recommendations were NEEDS-INVESTIGATION (this issue) and NEEDS-HANDLER (the duplicate, which said "Hermit should return a fixed refusal rather than depend on kernel configuration"). The duplicate's stated intent is what the code now does.

## Recommended tests

- T1 (covers F1, F5): `syscall(SYS_sysfs, 1, "ext4")` returns `ENOSYS` under `hermit run --verify` on ptrace, in both default and fail-closed (`--panic-on-unsupported-syscalls`) runs.
  Existing candidates: `tests/c/sysfs_enosys.c` through cell `c-programs/sysfs-enosys` (`tests/e2e/manifests/c-programs.yaml:9403`, verify `ci: ptrace: true`), and `mount_introspection_syscalls_fall_back_deterministically` at `hermit-cli/tests/mount_introspection.rs:30`, which runs that guest with `--backend=ptrace`, `--strict`, `--verify` and `--panic-on-unsupported-syscalls`.
- T2 (covers F2, F3, F4): call options 2 (index 0, with a buffer pre-filled with a sentinel), 3, and an undefined option such as 7. Every call must return `ENOSYS`, the sentinel must be unchanged, and the two `--verify` runs must agree.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/494: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_introspection_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/515 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns ENOSYS for every call without touching the kernel, detcore/src/lib.rs:2110-2114, the same answer as a kernel without CONFIG_SYSFS_SYSCALL); linux_determinism stays conditional; merged https://github.com/rrnewton/hermit/issues/515; 5 flag behaviours and 2 tests listed; existing ptrace candidates for T1 named but no run result seen and only option 1 exercised, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sysfs`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sysfs&body=Discussion+for+the+%60sysfs%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsysfs.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sysfs%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sysfs" (https://github.com/rrnewton/hermit/issues/494), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/515, filed 2026-07-24 with the same title (legacy variant A1, batch 5 of the classification sweep), was merged into this body on 2026-10-03; its comment history (0 comments) stays on that issue, and this issue's comment history stays here.

Both legacy bodies said Hermit routed `sysfs` through the UNCLASSIFIED temporary pass-through block of `detcore/src/syscall_classification.rs` (the duplicate cites `dd60278f`). That was true when they were filed and is superseded: at hermit f4055eb9cae4 `sysfs` is Determinized and refused, as described under Hermit design.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sysfs` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `sysfs` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
sysfs(0x3, 0, 0)                = -1 ENOSYS (Function not implemented)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Host strace evidence from the duplicate https://github.com/rrnewton/hermit/issues/515

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness, traced with `strace -qq -f -e trace=sysfs`. Command 3 returned `ENOSYS`, consistent with a kernel built without `CONFIG_SYSFS_SYSCALL`:

```text
sysfs(0x3, 0x7ffe4ba2f7d0, 0)   = -1 ENOSYS (Function not implemented)
```

Manual reference from the duplicate: [sysfs(2)](https://man7.org/linux/man-pages/man2/sysfs.2.html).
