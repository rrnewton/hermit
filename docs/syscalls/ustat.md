---
schema: syscall-pages/v1
syscall: ustat
nr_x86_64: 136
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/509
  - https://github.com/rrnewton/hermit/issues/530
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ustat&body=Discussion+for+the+%60ustat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fustat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ustat%60%2C+or+in+what+the+page+says%3F%0A
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

`ustat(dev, ubuf)` is the obsolete interface that returns the free-block and free-inode counts of the mounted filesystem whose device number is `dev`. Hermit refuses it: Detcore answers every call with `ENOSYS` before looking at the arguments and never calls the kernel (`detcore/src/lib.rs:2110-2114` at hermit f4055eb9cae4), which sends callers to `statfs`/`fstatfs`.

Manual reference: Linux man-pages ustat(2).

## Linux semantics

`statfs(2)` and `fstatfs(2)` replaced `ustat` long ago; glibc stopped providing the `ustat` wrapper and `<ustat.h>` in 2.28, so only raw `syscall(SYS_ustat, ...)` callers and old binaries reach it. The kernel takes `dev` as a 32-bit value, decodes it into a device number, looks for a mounted superblock with that device, and asks that filesystem for its `statfs` data. Only `f_tfree` (free blocks) and `f_tinode` (free inodes) carry information; `f_fname` and `f_fpack` are always zero. The call has no side effects.

The device lookup comes first. A device number that names no mounted superblock fails with `EINVAL` before `ubuf` is checked. Host strace on Linux 6.17 shows this both for device 0 and for the root directory's `st_dev` (`makedev(0, 0x20)`), so a device number taken from `stat()` does not always name a superblock `ustat` can find. Both strace lines are under Additional information.

### Flags and options

- F1: `dev` names a mounted filesystem whose superblock implements `statfs`: returns 0 and writes the live free-block count (`f_tfree`) and free-inode count (`f_tinode`); `f_fname` and `f_fpack` are zeroed.
- F2: `dev` names no mounted superblock, including device 0 and a `st_dev` the kernel cannot map back to a superblock: `EINVAL`. This check runs before `ubuf` is touched.
- F3: `dev` is valid but `ubuf` is not writable: `EFAULT`.
- F4: the filesystem behind `dev` does not implement `statfs`: `ENOSYS`, the same errno Hermit returns for every call.

## Determinism analysis

`linux_determinism: nondeterministic`. The success result (F1) is a snapshot of how full a host filesystem is. Any write, deletion or allocation on that filesystem, by any process inside or outside the guest, changes it between runs and within one run. Which device numbers succeed at all depends on the host's mount table and on how its filesystems assign device numbers. The error results (F2, F3) repeat for a fixed host, but a program calls `ustat` for its success value, so the classification follows that path.

The duplicate issue https://github.com/rrnewton/hermit/issues/530 classified `ustat` as CONDITIONAL, while this issue said nondeterministic. Nondeterministic is kept: no argument value makes the success path repeatable, and "conditional" there meant only that the error paths repeat.

### Nondeterminism sources

- Free-block and free-inode counts, which change with any concurrent allocation on the host filesystem, and with quotas and reserved blocks
- The host mount table and device-number assignment, which decide whether a given `dev` finds a superblock (F2)
- Whether the filesystem implements `statfs` (F4)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` lists `ustat` among the Determinized syscalls (`detcore/src/syscall_classification.rs:385`), and `is_mount_introspection_enosys_syscall` includes it (`detcore/src/syscall_classification.rs:1190-1200`). The doc comment at `detcore/src/syscall_classification.rs:1182-1189` gives the reason: the live capacity counters are host state outside the guest's model, and a fixed `ENOSYS` directs callers to `statfs`.
- In `handle_syscall_event` (`detcore/src/lib.rs:1892`), the dispatch match (`detcore/src/lib.rs:2028`) returns `Err(Errno::ENOSYS)` for this predicate (`detcore/src/lib.rs:2110-2114`). It does not decode `dev` or `ubuf` and does not inject the syscall, so the guest's buffer is never written and the result is the same for every argument (F1-F4).
- Unlike the `rseq` arm (`detcore/src/lib.rs:2031-2037`), this arm does not consult `panic_on_unsupported_syscalls`: the answer is `ENOSYS` with and without fail-closed mode, matching `refused_only_when_fail_closed: false`.
- The predicate is part of the deterministic refusal set (`detcore/src/syscall_classification.rs:1331`), and the unit test `mount_introspection_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2377`) pins both the classification and the predicate membership.

Why `refuse`:

- `ENOSYS` is a result a real kernel gives (F4), and the replacement interface is handled: `statfs` and `fstatfs` run the real call and canonicalize the host-varying fields, including free blocks and inodes (`detcore/src/lib.rs:2794-2801`). A caller that falls back from `ustat` to `statfs` therefore gets a deterministic answer.
- `passthrough` would leak live host capacity (F1).
- `model` or `emulate` would mean inventing canonical free counts for an interface glibc no longer exposes. The `statfs` canonicalization could be reused, but no known workload needs it.

The legacy recommendations were NEEDS-INVESTIGATION (this issue) and NEEDS-HANDLER (the duplicate, which suggested "deterministically refuse or record"). The refusal branch of that suggestion is what the code does; recording is not needed because the refusal never consults the host.

Note: `ENOSYS` from `ustat` usually means "not supported by this filesystem" (F4), whereas natively a bad device gives `EINVAL` (F2). A program that treats `EINVAL` specially sees a different errno under Hermit. No such program is known.

## Recommended tests

- T1 (covers F2): `syscall(SYS_ustat, 0, NULL)`, which natively fails with `EINVAL`, returns `ENOSYS` under `hermit run --verify` on ptrace, in both default and fail-closed (`--panic-on-unsupported-syscalls`) runs.
  Existing candidates: `tests/c/ustat_enosys.c` through cell `c-programs/ustat-enosys` (`tests/e2e/manifests/c-programs.yaml:10599`, verify `ci: ptrace: true`), and `sysv_and_legacy_filesystem_features_fall_back_deterministically` at `hermit-cli/tests/sysv_legacy_fallbacks.rs:30`, which runs that guest with `--backend=ptrace`, `--strict`, `--verify` and `--panic-on-unsupported-syscalls`.
- T2 (covers F1, F3, F4): call `ustat` with the `st_dev` of `/`, of the current directory and of a `/proc` file, each with a writable buffer pre-filled with a sentinel, and once with an unwritable buffer. Every call must return `ENOSYS`, the sentinel must be unchanged, and the two `--verify` runs must agree.
- T3 (covers F1): a guest that tries `ustat` on the root `st_dev`, falls back to `statfs("/")` on `ENOSYS`, and prints the free counts. Under `--verify` both runs must print the same values.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/509: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_mount_introspection_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/530 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns ENOSYS for every call without touching the kernel, detcore/src/lib.rs:2110-2114, and statfs/fstatfs are the canonicalized replacement); linux_determinism stays nondeterministic (duplicate's CONDITIONAL reconciled); merged https://github.com/rrnewton/hermit/issues/530; 4 flag behaviours and 3 tests listed; existing ptrace candidates for T1 named but no run result seen and only one argument shape exercised, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `ustat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ustat&body=Discussion+for+the+%60ustat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fustat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ustat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: ustat" (https://github.com/rrnewton/hermit/issues/509), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/530, filed 2026-07-24 with the same title (legacy variant A1, batch 5 of the classification sweep), was merged into this body on 2026-10-03; its comment history (0 comments) stays on that issue, and this issue's comment history stays here.

Both legacy bodies said Hermit routed `ustat` through the UNCLASSIFIED temporary pass-through block of `detcore/src/syscall_classification.rs` (the duplicate cites `dd60278f`). That was true when they were filed and is superseded: at hermit f4055eb9cae4 `ustat` is Determinized and refused, as described under Hermit design.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=ustat` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `ustat` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
ustat(makedev(0, 0), 0x407000)  = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Host strace evidence from the duplicate https://github.com/rrnewton/hermit/issues/530

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness, traced with `strace -qq -f -e trace=ustat`. The probe passed the root directory's `st_dev`, which reached the kernel and returned `EINVAL`:

```text
ustat(makedev(0, 0x20), 0x7ffe3f78e270) = -1 EINVAL (Invalid argument)
```

Manual reference from the duplicate: [ustat(2)](https://man7.org/linux/man-pages/man2/ustat.2.html).
