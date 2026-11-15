---
schema: syscall-pages/v1
syscall: lchown
nr_x86_64: 94
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/629
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lchown&body=Discussion+for+the+%60lchown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flchown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lchown%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_ownership_change_noop_syscall"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`lchown(path, owner, group)` changes the owner and group of the file named by `path` without following a symlink in the last component, so on a symlink it changes the link itself. Detcore does not forward it: it emulates the call as the success a real root process gets, checks the path with a no-follow `newfstatat`, and has the kernel apply the other effects of a successful chown by reissuing `lchown(path, -1, -1)` (`detcore/src/syscalls/files.rs:3353`). The requested owner and group are never applied.

Manual reference: Linux man-pages chown(2).

## Linux semantics

`lchown` is `chown` with lookup flag `0` instead of `LOOKUP_FOLLOW`: symlinks in earlier components are still followed, the last one is not. The kernel then takes write access to the mount (`mnt_want_write`) and runs `chown_common`/`notify_change`, through idmapping and LSM hooks.

Permission: changing the owner needs `CAP_CHOWN` in the inode's user namespace; the owner may change the group only to a group it belongs to. An id unmapped in the caller's user namespace gives `EINVAL`. So the errno and the success depend on credentials and namespace mapping.

On a symlink the link's own ownership and ctime change, and the target is untouched; a symlink has no set-ID bits to clear. On any other file type `lchown` behaves exactly like `chown`: a non-directory loses `S_ISUID`, loses `S_ISGID` only if group-executable, and ctime moves even when both ids are `-1`.

### Flags and options

- F1: an id argument of `-1` leaves that id unchanged; with both `-1` no id changes, but ctime and F4 still apply.
- F2: an explicit owner and/or group (0, the caller's own id, or a foreign id): changes ownership. Linux returns `EPERM` without the needed privilege, and `EINVAL` for an id unmapped in the caller's user namespace.
- F3: a symlink as the last component is not followed: the link itself is changed (its ctime moves), the target is untouched, and a dangling symlink succeeds.
- F4: a non-symlink target behaves like `chown`: a non-directory loses `S_ISUID`, loses `S_ISGID` only if `S_IXGRP` is set, and loses file capabilities; directories keep set-ID bits; ctime is updated.
- F5: path-resolution errors: `ENOENT` (missing component or empty path), `ENOTDIR`, `ELOOP` (symlink loop in an earlier component), `ENAMETOOLONG`, `EFAULT` (bad `path` pointer), `EACCES` (no search permission on a prefix).
- F6: write-side error after lookup: `EROFS` on a read-only mount in the guest's mount namespace.
- F7: host-config write-side errors: `EPERM` on an inode the host marked immutable or append-only (setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest lacks), and denials by the host's LSM policy.
- F8: host-transient write-side error: `EIO` from failing host storage.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The call repeats under fixed credentials, namespace mappings and filesystem state that nothing outside the guest changes. The result changes with the caller's identity (F2) and with outside changes to the path. The ctime it writes is the host clock; Hermit's metadata virtualization keeps a later `lstat` stable.

### Nondeterminism sources

- User-namespace ID mappings and capability policy
- External path and metadata mutation
- Filesystem and LSM policy
- Host clock value written to ctime

### Cross-host variation

- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F7) where the reissued `lchown(path, -1, -1)` changes the mode, on that host and not on another.
- Host LSM policy: a denial (F7) depends on which LSM the host runs and its rules.

## Hermit design

Target policy: `emulate`, changed from the legacy `passthrough`. Code at hermit f4055eb9cae4:

- `detcore/src/syscall_classification.rs:688` puts `lchown` in the Determinized set (comment with `TODO-HUMAN-REVIEW(#1851)` at `detcore/src/syscall_classification.rs:676`). The arm at `detcore/src/lib.rs:2303-2307` sends it to `handle_ownership_change_noop` (`detcore/src/syscalls/files.rs:3353`).
- Identity half, emulated. The requested ids are never applied; the handler returns 0 after validation (`detcore/src/syscalls/files.rs:3474`). F2's host-identity `EPERM`/`EINVAL` cannot reach the guest, matching the virtual-root identity used for `getuid` and the credential-setting no-ops (`detcore/src/lib.rs:2272-2276`).
- Argument half, delegated. `lchown` becomes `newfstatat(AT_FDCWD, path, stat, AT_SYMLINK_NOFOLLOW)` (`detcore/src/syscalls/files.rs:3410`), which walks the path the same way, so F3's no-follow lookup and the F5 errnos come from the kernel. The errno is returned and the scratch stat buffer is zeroed (`detcore/src/syscalls/files.rs:3430-3434`).
- Consequence half, delegated. The handler reissues `lchown(path, -1, -1)` (`detcore/src/syscalls/files.rs:3443`, `detcore/src/syscalls/files.rs:3457`, `detcore/src/syscalls/files.rs:3473`), so F1, F3's ctime change and F4 come from the kernel; an errno from that call propagates.
- Every call goes through `record_or_replay`, so replay does not need the files (`detcore/src/syscalls/files.rs:3320`).

Why `emulate`: `passthrough` gives the host identity's answer, `EPERM` with no user namespace and `EINVAL` for an unmapped id under the default one-uid map (`detcore/src/syscall_classification.rs:1001-1008`), while the guest believes it is root. `model` would need per-file ownership tracking, which Detcore does not have (`detcore/src/syscall_classification.rs:1016-1024`). `refuse` would break archive and copy tools that set the ownership of symlinks they create.

Known differences from Linux:

- The ownership change is not observable through a later `lstat` (`detcore/src/syscalls/files.rs:3323-3328`).
- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, path resolution checks search permission against the launching host user, so `lchown` can return `EACCES` on a path prefix the guest, told it is root, expects to pass (`detcore/src/syscalls/files.rs:3333-3337`), and the `(-1, -1)` reissue returns `EPERM` on every file that user does not own, root-owned ones included (`detcore/src/syscalls/files.rs:3339-3347`).
- The doc comment at `detcore/src/syscalls/files.rs:3330-3333` says a read-only mount target passes where Linux returns `EROFS`. By my reading of Linux `fs/open.c`, `do_fchownat` (which `lchown` uses) calls `mnt_want_write` before `chown_common`, so the reissue should now return `EROFS`. Hypothesis from the kernel source; T4 settles it.
- On a target the guest does not own, the reissue can return `EPERM` (`detcore/src/syscalls/files.rs:3339-3347`), which is what pass-through returned.
- Hypothesis, not measured: an immutable or append-only file without set-ID bits may accept the `(-1, -1)` reissue, so Hermit would return 0 where Linux refuses a real ownership change.
- Target (owner decision 6, 2026-10-04): the emulation owes Linux's immutable and append-only check, because it claims Linux behaviour. In the kernel, `do_fchownat` (fs/open.c) resolves the path, then `mnt_want_write` gives `EROFS`, then `notify_change` (fs/attr.c) gives `EPERM` for an immutable or append-only inode whenever ids change. So after the probe (`detcore/src/syscalls/files.rs:3410`, path errors) and before the `(-1, -1)` reissue (`detcore/src/syscalls/files.rs:3473`), Detcore reads the inode's flags, for example by making the probe a `statx` with the same target arguments and reading `STATX_ATTR_IMMUTABLE` and `STATX_ATTR_APPEND`, and returns `EPERM` for either flag, except on a read-only mount, where `EROFS` still comes first. A program that expects `lchown` on a locked file to fail then gets Linux's answer instead of a success, and the reissue no longer moves the ctime of an immutable file.
- Owner decision 4 models the credential record without enforcement, so after a modelled `setuid(1000)` that leaves no `CAP_CHOWN`, `lchown` still returns the virtual-root success where Linux returns `EPERM`: neither the emulation nor the kernel consults the record. `target_policy` stays `emulate`.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --strict` on ptrace in its container, `lchown` a symlink to a foreign uid and to -1:-1, and `lchown` a dangling symlink. All return 0; `lstat` shows the link owner unchanged; the target's mode and ctime are untouched.
  Existing partial coverage: `chown_family_keeps_virtual_root_identity_and_real_errors` at `hermit-cli/tests/chown_virtual_root_identity.rs:185` and `chown_family_keeps_virtual_root_identity_without_a_user_namespace` at `hermit-cli/tests/chown_virtual_root_identity.rs:197`, whose guest calls `lchown(link_path, FOREIGN_UID, FOREIGN_GID)` at `tests/c/chown_virtual_root_identity.c:150`. It checks only the return value, not that the target was left alone. Of these, `chown_family_keeps_virtual_root_identity_without_a_user_namespace` exercises a reduced-fidelity mode (owner decision 3) and does not count toward `done`.
- T2 (covers F4): `lchown` a regular 06755 file with -1:-1: returns 0, the mode becomes 0755 and ctime moves; a 06755 directory keeps its bits.
- T3 (covers F5): `ENOENT` for a missing file and for an empty path, `ENOTDIR` for `file/child`, `ELOOP` for a loop in an earlier component, `ENAMETOOLONG`, and `EFAULT` for a bad pointer.
  Existing partial coverage: `ENOENT` at `tests/c/chown_virtual_root_identity.c:163`.
- T4 (covers F6): `lchown` a file on a read-only bind mount and record Hermit's errno next to the native `EROFS`, which confirms or refutes the reissue hypothesis.
- T5 (covers F1, F2, F3): `lchown` a symlink under `hermit run --strict --verify` and under record and replay; both runs, and replay, return the same results.
- U1 (covers F7; host-config): only the host can mark an inode immutable or append-only or install an LSM policy, so CI cannot produce these errors on demand. Hermit hands the kernel's errno from the reissued `lchown(path, -1, -1)` to the guest unchanged (`detcore/src/syscalls/files.rs:3473`).
- U2 (covers F8; host-transient): `EIO` comes from failing host storage shared with other processes. The probe `newfstatat` (`detcore/src/syscalls/files.rs:3430`) and the reissued call (`detcore/src/syscalls/files.rs:3473`) hand it to the guest today. F8 counts toward done only once Hermit hides this error or reports it as an infrastructure error.
  Existing candidates: cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`, verify ci on ptrace) and `deterministic_file_metadata_syscalls_verify` at `hermit-cli/tests/syscall_file_metadata.rs:51`, guest call `lchown(symlink_path, uid, gid)` at `tests/c/syscall_file_metadata.c:161`; and `record_path_queries_and_legacy_mutations` at `hermit-cli/tests/record_replay.rs:2171`, whose guest calls `lchown("d", -1, -1)` on a symlink at `tests/c/record_replay_path_queries.c:242`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/629: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_ownership_change_noop_syscall.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> emulate (Detcore emulates the identity half as a virtual-root success, validates the path with a no-follow newfstatat, and delegates set-ID clearing and ctime to an lchown(path, -1, -1) reissue; see detcore/src/syscalls/files.rs:3353); linux_determinism stays conditional; 6 flag behaviours and 5 tests listed; replaced the stale catch-all Summary; no coverage token because no test has been run against this list, so tracking_status stays todo; two kernel-reading hypotheses (EROFS now surfaces, immutable files) are left as open questions for T4
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (inode flags on host files, host LSM policy); F6 narrowed to EROFS, with immutable and LSM EPERM split out as new F7 (host-config U1) and EIO as new F8 (host-transient U2); no open question is resolved by decisions 1 or 2 (the EROFS and immutable questions are about Hermit's emulated result, not reachability)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): the --no-namespace EACCES known difference became the reduced-fidelity line (host-user path search and reissue EPERM under --no-namespace and namespace-less backends); noted that a modelled setuid does not change the emulated success; target_policy unchanged (emulate); T1 narrowed from both namespace configurations to the strict ptrace container; the namespace-less candidate test does not count toward done; no new F/T ids; no open question removed (EROFS, immutable and #1851 questions are outside decisions 3 and 4)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (the emulation owes notify_change's immutable/append-only EPERM, after path errors and EROFS as in the kernel; Target bullet added); target_policy stays emulate; no new F/T ids (F7 stays host-config under U1); removed the immutable-target question; kept the EROFS measurement and #1851 questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lchown`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lchown&body=Discussion+for+the+%60lchown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flchown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lchown%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: lchown" (https://github.com/rrnewton/hermit/issues/629), legacy variant B.

The determinization was implemented by https://github.com/rrnewton/hermit/pull/1851. The membership unit test `ownership_change_family_is_determinized_not_passthrough` (`detcore/src/syscall_classification.rs:2785`) pins only set membership.

### Open questions

- Is the `EROFS` residual in the doc comment at `detcore/src/syscalls/files.rs:3330-3333` stale now that the reissue goes through `mnt_want_write`? T4 measures it.
- The design carries `TODO-HUMAN-REVIEW(#1851)` (`detcore/src/syscall_classification.rs:676`), so the owner has not yet signed off on emulating the identity half.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
