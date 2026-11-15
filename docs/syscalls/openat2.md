---
schema: syscall-pages/v1
syscall: openat2
nr_x86_64: 437
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/315
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+openat2&body=Discussion+for+the+%60openat2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopenat2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60openat2%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "sysno-guard"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`openat2(dirfd, path, how, size)` is `openat` with an extensible `struct open_how` that adds strict flag checking and `RESOLVE_*` path-resolution restrictions. Hermit always returns `ENOSYS` without consulting the kernel, under every mode, so callers fall back to the `openat` path that Detcore already models.

## Linux semantics

Added in Linux 5.6. `struct open_how` holds `flags`, `mode` and `resolve`. Unlike `openat`, unknown bits in `flags` or `resolve` are errors rather than ignored, and `mode` must be 0 unless the open can create a file. The `size` argument versions the struct: the kernel accepts a larger struct only if the extra bytes are zero. On success the result is the lowest free descriptor, exactly as for `openat`. glibc does not route `open`/`openat` through `openat2`; programs call it directly and the usual pattern is to fall back to `openat` on `ENOSYS`.

### Flags and options

- F1: `how.flags`: the `O_*` open flags of `openat`, including `O_CREAT`, `O_TMPFILE`, `O_PATH` and `O_DIRECTORY`; unknown bits return `EINVAL`.
- F2: `how.mode`: must be 0 unless `O_CREAT` or `O_TMPFILE` is set, and may not have bits outside `07777`; otherwise `EINVAL`.
- F3: `RESOLVE_NO_XDEV` (0x01): fail with `EXDEV` if resolution crosses a mount point.
- F4: `RESOLVE_NO_MAGICLINKS` (0x02): fail with `ELOOP` on procfs-style magic links.
- F5: `RESOLVE_NO_SYMLINKS` (0x04): fail with `ELOOP` on any symbolic link, including magic links.
- F6: `RESOLVE_BENEATH` (0x08): fail with `EXDEV` if resolution would leave `dirfd` (absolute paths, `..` escapes, absolute symlinks).
- F7: `RESOLVE_IN_ROOT` (0x10): treat `dirfd` as the root for absolute paths, `..` and symlinks; combined with `RESOLVE_BENEATH` it returns `EINVAL`.
- F8: `RESOLVE_CACHED` (0x20): succeed only if the lookup can be served from the kernel's dentry cache, otherwise `EAGAIN`; invalid with `O_CREAT`, `O_TRUNC` or `O_TMPFILE`.
- F9: `size`: below 24 (`OPEN_HOW_SIZE_VER0`) returns `EINVAL`; larger than the kernel's struct with nonzero trailing bytes returns `E2BIG`.
- F10: Unknown `resolve` bits return `EINVAL`.

## Determinism analysis

`linux_determinism: conditional`. For a fixed filesystem, mount table and descriptor table, the result is the same as the equivalent `openat` plus the fixed `RESOLVE_*` checks.

### Nondeterminism sources

- The descriptor number, the file's existence and contents depend on filesystem and descriptor-table state, as for `openat`.
- `RESOLVE_NO_XDEV` and `RESOLVE_BENEATH` outcomes depend on the mount table at the time of the call.
- `RESOLVE_CACHED` depends on whether the host kernel happens to have the path in its dentry cache, which no guest-visible state controls.

The call repeats when the filesystem, mounts and descriptor table are fixed and `RESOLVE_CACHED` is not used.

## Hermit design

Target policy: `refuse`. A fixed `ENOSYS` is host-independent, and it routes callers onto the already-determinized `openat` path.

What the code does at hermit f4055eb9cae4:

- Dispatch: a Sysno-guarded arm returns `ENOSYS` unconditionally (`detcore/src/lib.rs:2258-2260`). The comment explains the choice: openat2's "callers must fall back to openat when it returns ENOSYS (kernels before 5.6 lack openat2), so a fixed -ENOSYS routes them onto the already-determinized openat path with no host dependency" (`detcore/src/lib.rs:2252-2257`).
- Classification: listed with the determinized rows (`detcore/src/syscall_classification.rs:646`) and in the always-refused aggregate (`detcore/src/syscall_classification.rs:1335-1337`), not in the strict-only set, so the `--allow-unsupported-syscalls` opt-out does not forward it either. The unit test `deterministic_refusal_aggregate_includes_fixed_error_families` lists it among refused members (`detcore/src/syscall_classification.rs:2709-2710`, `detcore/src/syscall_classification.rs:2731`) and asserts that it is not strict-only (`detcore/src/syscall_classification.rs:2777-2778`).

The refusal precedes every argument check, so all of F1 through F10 report `ENOSYS`, including the `EINVAL`, `E2BIG`, `EXDEV`, `ELOOP` and `EAGAIN` cases Linux would return. That is a consistent contract: the guest sees a kernel without openat2.

Why not model: the legacy recommendation was NEEDS-HANDLER, on the grounds that openat2 is "openat with richer options". Modeling it would mean reproducing every `RESOLVE_*` rule against Detcore's view of the filesystem, and `RESOLVE_CACHED` (F8) cannot be made host-independent at all. The refusal gives the same determinism at no cost for every caller that falls back. A program that requires openat2 and has no fallback fails under Hermit.

## Recommended tests

- T1 (covers F1, F5, F6): Basic `O_RDONLY`, `RESOLVE_NO_SYMLINKS` and `RESOLVE_BENEATH` requests on `/` each return -1 with `ENOSYS`. Existing candidate: `tests/c/openat2_refusal.c` (cell `c-programs/openat2-refusal`, `tests/e2e/manifests/c-programs.yaml:12899`, verify CI at `tests/e2e/manifests/c-programs.yaml:12917`, backends at `tests/e2e/manifests/c-programs.yaml:12918-12921`).
- T2 (covers F2, F9, F10): `mode = 0644` without `O_CREAT`, `size = 8`, `size = 32` with a nonzero tail, and an unknown `resolve` bit each return `ENOSYS` (not `EINVAL` or `E2BIG`).
- T3 (covers F3, F4, F7, F8): `RESOLVE_NO_XDEV`, `RESOLVE_NO_MAGICLINKS`, `RESOLVE_IN_ROOT`, `RESOLVE_BENEATH | RESOLVE_IN_ROOT` and `RESOLVE_CACHED` each return `ENOSYS`.
- T4 (covers F1): A program that tries `openat2(O_CREAT | O_WRONLY)` and falls back to `openat` on `ENOSYS` creates the file, writes it, and reports the same descriptor number in every `--verify` run and under record/replay.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/315#issuecomment-5872894795
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/315: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch sysno-guard, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; openat2 records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/315 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (unconditional ENOSYS at detcore/src/lib.rs:2258-2260, in every mode, so callers fall back to the modeled openat path; RESOLVE_CACHED cannot be made host-independent); linux_determinism stays conditional; 10 flag behaviours and 4 tests listed; T1 has a ptrace-CI candidate (tests/c/openat2_refusal.c) but F2-F4 and F7-F10 are uncovered, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/openat2_refusal.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `openat2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+openat2&body=Discussion+for+the+%60openat2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopenat2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60openat2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: openat2" (https://github.com/rrnewton/hermit/issues/315), legacy variant A1.

The legacy Determinism analysis said the returned descriptor "must be virtualized (as for open/openat)" and that openat2 had no handler while open and openat did. At f4055eb9cae4 the call never reaches the kernel, so no descriptor is returned.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/315.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=openat2`, and by confirming hermit currently routes `openat2` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
openat2(-1, "/nonexistent-xyz", NULL, 0) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/315#issuecomment-5872894795
