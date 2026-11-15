---
schema: syscall-pages/v1
syscall: fchownat
nr_x86_64: 260
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/353
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchownat&body=Discussion+for+the+%60fchownat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchownat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchownat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_ownership_change_noop_syscall"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`fchownat(dirfd, path, owner, group, flags)` changes the owner and group of a file named relative to a directory descriptor, with flags that control symlink following and allow acting on `dirfd` itself. Detcore does not forward it: it checks the flags, emulates the call as the success a real root process gets, validates the target with a `newfstatat` using the same `dirfd`, `path` and flags, and has the kernel apply the other effects of a successful chown by reissuing the call with ids `-1, -1` (`detcore/src/syscalls/files.rs:3353`). The requested owner and group are never applied.

Manual reference: Linux man-pages chown(2).

## Linux semantics

`do_fchownat` first rejects any flag other than `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH` with `EINVAL`, before looking at the path. It then resolves `path` relative to `dirfd` (following the last symlink unless `AT_SYMLINK_NOFOLLOW`), takes write access to the mount (`mnt_want_write`), and runs `chown_common`/`notify_change`, through idmapping and LSM hooks. `chown`, `lchown` and `fchown` are special cases of this path.

Permission: changing the owner needs `CAP_CHOWN` in the inode's user namespace; the owner may change the group only to a group it belongs to. An id unmapped in the caller's user namespace gives `EINVAL`. The errno and the success therefore depend on credentials and namespace mapping.

A successful call on a non-directory clears `S_ISUID`, clears `S_ISGID` only if the file is group-executable, and drops file capabilities; directories and symlinks keep their mode; ctime always moves, even when both ids are `-1`.

### Flags and options

- F1: an id argument of `-1` leaves that id unchanged; with both `-1` no id changes, but F8 still applies.
- F2: an explicit owner and/or group (0, the caller's own id, or a foreign id): changes ownership. Linux returns `EPERM` without the needed privilege, and `EINVAL` for an id unmapped in the caller's user namespace.
- F3: `dirfd` selection: a relative `path` is resolved from `dirfd`, or from the working directory for `AT_FDCWD`; an absolute `path` ignores `dirfd`; a relative path with an invalid `dirfd` gives `EBADF`, and with a `dirfd` that is not a directory gives `ENOTDIR`.
- F4: `flags == 0`: a symlink as the last component is followed and the target is changed; a dangling symlink gives `ENOENT`.
- F5: `AT_SYMLINK_NOFOLLOW`: the last symlink is not followed; the link itself is changed and a dangling symlink succeeds.
- F6: `AT_EMPTY_PATH` with an empty `path`: acts on the file `dirfd` refers to, including an `O_PATH` descriptor, or on the working directory for `AT_FDCWD`. Without the flag an empty `path` gives `ENOENT`.
- F7: any other flag bit (`AT_REMOVEDIR`, `AT_SYMLINK_FOLLOW`, `AT_NO_AUTOMOUNT`, undefined bits) gives `EINVAL`, checked before the path.
- F8: side effects of success: a non-directory, non-symlink loses `S_ISUID`, loses `S_ISGID` only if `S_IXGRP` is set, and loses file capabilities; ctime is updated even if nothing else changes.
- F9: path-resolution errors: `ENOENT`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG`, `EFAULT` (bad `path` pointer), `EACCES` (no search permission on a prefix).
- F10: write-side error after lookup: `EROFS` on a read-only mount in the guest's mount namespace.
- F11: host-config write-side errors: `EPERM` on an inode the host marked immutable or append-only (setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest lacks), and denials by the host's LSM policy.
- F12: host-transient write-side error: `EIO` from failing host storage.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. It repeats for fixed paths, dirfds, ids, credentials, flags, and filesystem state that nothing outside the guest changes. It changes with the caller's identity (F2), the namespace configuration, and outside changes to the path. The flag check (F7) is purely a function of the arguments. The ctime written by F8 is the host clock; Hermit's metadata virtualization keeps a later `stat` stable.

### Nondeterminism sources

- External path/metadata mutation
- User-namespace mappings
- Filesystem and LSM policy
- Host clock value written to ctime (F8)

### Cross-host variation

- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F11) where the reissued `fchownat(dirfd, path, -1, -1, flags)` changes the mode, on that host and not on another.
- Host LSM policy: a denial (F11) depends on which LSM the host runs and its rules.

## Hermit design

Target policy: `emulate`, changed from the legacy `passthrough`. Code at hermit f4055eb9cae4:

- `detcore/src/syscall_classification.rs:687` puts `fchownat` in the Determinized set (comment with `TODO-HUMAN-REVIEW(#1851)` at `detcore/src/syscall_classification.rs:676`). The arm at `detcore/src/lib.rs:2303-2307` sends it to `handle_ownership_change_noop` (`detcore/src/syscalls/files.rs:3353`).
- Flags first, as in the kernel. Any bit outside `AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW` returns `EINVAL` before any lookup (`detcore/src/syscalls/files.rs:3374-3380`), so F7 matches Linux and a flag `newfstatat` would accept, such as `AT_NO_AUTOMOUNT`, is still refused.
- Identity half, emulated. The requested ids are never applied; the handler returns 0 after validation (`detcore/src/syscalls/files.rs:3474`). F2's host-identity `EPERM`/`EINVAL` cannot reach the guest, matching the virtual-root identity used for `getuid` and the credential-setting no-ops (`detcore/src/lib.rs:2272-2276`).
- Argument half, delegated. The call becomes `newfstatat(dirfd, path, stat, flags)` with the guest's own `dirfd`, `path` and (already checked) flags (`detcore/src/syscalls/files.rs:3417-3422`), so F3, F4, F5, F6 and the F9 errnos come from the kernel's own path walk. The errno is returned and the scratch stat buffer is zeroed (`detcore/src/syscalls/files.rs:3430-3434`).
- Consequence half, delegated. The handler reissues `fchownat(dirfd, path, -1, -1, flags)` (`detcore/src/syscalls/files.rs:3443`, `detcore/src/syscalls/files.rs:3463-3469`, `detcore/src/syscalls/files.rs:3473`), so F1 and F8 come from the kernel; an errno from that call propagates. Because the reissue is the same syscall with the same target arguments, any argument error the real call would return before it looks at the ids also comes back from it.
- Every call goes through `record_or_replay`, so replay does not need the files (`detcore/src/syscalls/files.rs:3320`).

Why `emulate`: `passthrough` gives the host identity's answer, `EPERM` with no user namespace and `EINVAL` for an unmapped id under the default one-uid map (`detcore/src/syscall_classification.rs:1001-1008`), while the guest believes it is root. `model` would need per-file ownership tracking, which Detcore does not have (`detcore/src/syscall_classification.rs:1016-1024`). `refuse` would break coreutils `chown`, `install`, `tar` and other tools that set ownership on files they create.

Known differences from Linux:

- The ownership change is not observable through a later `stat` (`detcore/src/syscalls/files.rs:3323-3328`).
- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, path resolution checks search permission against the launching host user, so `fchownat` can return `EACCES` on a path prefix the guest, told it is root, expects to pass (`detcore/src/syscalls/files.rs:3333-3337`), and the `(-1, -1)` reissue returns `EPERM` on every file that user does not own, root-owned ones included (`detcore/src/syscalls/files.rs:3339-3347`).
- The doc comment at `detcore/src/syscalls/files.rs:3330-3333` says a read-only mount target passes where Linux returns `EROFS`. By my reading of Linux `fs/open.c`, `do_fchownat` calls `mnt_want_write` before `chown_common`, so the reissue should now return `EROFS`. Hypothesis from the kernel source; T7 settles it.
- On a target the guest does not own, the reissue can return `EPERM` (`detcore/src/syscalls/files.rs:3339-3347`), which is what pass-through returned.
- Hypothesis, not measured: an immutable or append-only file without set-ID bits may accept the `(-1, -1)` reissue, so Hermit would return 0 where Linux refuses a real ownership change.
- Target (owner decision 6, 2026-10-04): the emulation owes Linux's immutable and append-only check, because it claims Linux behaviour. In the kernel, `do_fchownat` (fs/open.c) resolves the path, then `mnt_want_write` gives `EROFS`, then `notify_change` (fs/attr.c) gives `EPERM` for an immutable or append-only inode whenever ids change. The flag check already comes first, as in the kernel (`detcore/src/syscalls/files.rs:3375`-`3380`). So after the probe (`detcore/src/syscalls/files.rs:3417`, path errors) and before the `(-1, -1)` reissue (`detcore/src/syscalls/files.rs:3473`), Detcore reads the inode's flags, for example by making the probe a `statx` with the same target arguments and reading `STATX_ATTR_IMMUTABLE` and `STATX_ATTR_APPEND`, and returns `EPERM` for either flag, except on a read-only mount, where `EROFS` still comes first. A program that expects `fchownat` on a locked file to fail then gets Linux's answer instead of a success, and the reissue no longer moves the ctime of an immutable file.
- Owner decision 4 models the credential record without enforcement, so after a modelled `setuid(1000)` that leaves no `CAP_CHOWN`, `fchownat` still returns the virtual-root success where Linux returns `EPERM`: neither the emulation nor the kernel consults the record. `target_policy` stays `emulate`.

## Recommended tests

- T1 (covers F1, F2, F4, F8): under `hermit run --strict` on ptrace in its container, `fchownat(AT_FDCWD, file, ...)` with flags 0 on a 06755 file, to a foreign uid and to -1:-1, and through a symlink to it. All return 0, `stat` shows the owner unchanged, the target's mode becomes 0755 and its ctime moves (metadata virtualization off).
  Existing partial coverage: `chown_family_keeps_virtual_root_identity_and_real_errors` at `hermit-cli/tests/chown_virtual_root_identity.rs:185` and `chown_family_keeps_virtual_root_identity_without_a_user_namespace` at `hermit-cli/tests/chown_virtual_root_identity.rs:197`, whose guest calls `fchownat(AT_FDCWD, path, FOREIGN_UID, FOREIGN_GID, 0)` at `tests/c/chown_virtual_root_identity.c:151-153`. No -1:-1 call, no symlink, and the mode read-back follows several family calls. Of these, `chown_family_keeps_virtual_root_identity_without_a_user_namespace` exercises a reduced-fidelity mode (owner decision 3) and does not count toward `done`.
- T2 (covers F3): a relative path from an open directory descriptor succeeds; an absolute path with `dirfd` -1 succeeds; a relative path with `dirfd` -1 gives `EBADF`; a relative path with a regular-file `dirfd` gives `ENOTDIR`. No existing test.
- T3 (covers F5): `AT_SYMLINK_NOFOLLOW` on a symlink returns 0 and leaves the target's mode and ctime alone; on a dangling symlink it returns 0. No existing test.
- T4 (covers F6): `AT_EMPTY_PATH` with `""` on a regular-file descriptor, on an `O_PATH` descriptor and with `AT_FDCWD` returns 0; `""` without the flag gives `ENOENT`. No existing test.
- T5 (covers F7): flag `0x4000` gives `EINVAL`; so do `AT_REMOVEDIR`, `AT_SYMLINK_FOLLOW` and `AT_NO_AUTOMOUNT`, also on a missing path (`EINVAL` must win over `ENOENT`).
  Existing partial coverage: `tests/c/chown_virtual_root_identity.c:190-194` (flag `0x4000` on an existing file).
- T6 (covers F9): `ENOENT`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG` and `EFAULT` for a bad `path` pointer.
  Existing partial coverage: `ENOENT` at `tests/c/chown_virtual_root_identity.c:165-168`.
- T7 (covers F10): `fchownat` on a file on a read-only bind mount; record Hermit's errno next to the native `EROFS`, which confirms or refutes the reissue hypothesis.
- T8 (covers F1, F2, F3): under `hermit run --strict --verify` and under record and replay, `fchownat(AT_FDCWD, path, getuid(), getgid(), 0)` and a -1:-1 call return the same results in every run and in replay.
- U1 (covers F11; host-config): only the host can mark an inode immutable or append-only or install an LSM policy, so CI cannot produce these errors on demand. Hermit hands the kernel's errno from the reissued `fchownat(dirfd, path, -1, -1, flags)` to the guest unchanged (`detcore/src/syscalls/files.rs:3473`).
- U2 (covers F12; host-transient): `EIO` comes from failing host storage shared with other processes. The probe `newfstatat` (`detcore/src/syscalls/files.rs:3430`) and the reissued call (`detcore/src/syscalls/files.rs:3473`) hand it to the guest today. F12 counts toward done only once Hermit hides this error or reports it as an infrastructure error.
  Existing candidates for the verify half: cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`, verify ci on ptrace) and `deterministic_file_metadata_syscalls_verify` at `hermit-cli/tests/syscall_file_metadata.rs:51`, guest call at `tests/c/syscall_file_metadata.c:149`. No record/replay test calls `fchownat`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/353#issuecomment-5872982001
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/353: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_ownership_change_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; fchownat records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/353 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> emulate (Detcore rejects flags other than AT_EMPTY_PATH|AT_SYMLINK_NOFOLLOW with EINVAL, emulates the identity half as a virtual-root success, validates the target with a newfstatat using the same dirfd, path and flags, and delegates set-ID clearing and ctime to an fchownat(dirfd, path, -1, -1, flags) reissue; see detcore/src/syscalls/files.rs:3353); linux_determinism stays conditional; 10 flag behaviours and 8 tests listed; no coverage token because no test has been run against this list, so tracking_status stays todo; two kernel-reading hypotheses (EROFS now surfaces, immutable files) are left as open questions for T7
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (inode flags on host files, host LSM policy); F10 narrowed to EROFS, with immutable and LSM EPERM split out as new F11 (host-config U1) and EIO as new F12 (host-transient U2); no open question is resolved by decisions 1 or 2 (the EROFS and immutable questions are about Hermit's emulated result, not reachability)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): the --no-namespace EACCES known difference became the reduced-fidelity line (host-user path search and reissue EPERM under --no-namespace and namespace-less backends); noted that a modelled setuid does not change the emulated success; target_policy unchanged (emulate); T1 narrowed from both namespace configurations to the strict ptrace container; the namespace-less candidate test does not count toward done; no new F/T ids; no open question removed (EROFS, immutable and #1851 questions are outside decisions 3 and 4)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (the emulation owes notify_change's immutable/append-only EPERM, after path errors and EROFS as in the kernel; Target bullet added); target_policy stays emulate; no new F/T ids (F11 stays host-config under U1); removed the immutable-target question; kept the EROFS measurement and #1851 questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fchownat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchownat&body=Discussion+for+the+%60fchownat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchownat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchownat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fchownat" (https://github.com/rrnewton/hermit/issues/353), legacy variant A2. It was ranked 74 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/353.

The determinization was implemented by https://github.com/rrnewton/hermit/pull/1851. The membership unit test `ownership_change_family_is_determinized_not_passthrough` (`detcore/src/syscall_classification.rs:2785`) pins only set membership.

The compat cell `chown` (`tests/e2e/manifests/compat.yaml:234`) runs coreutils `chown --reference` under `--verify` on ptrace. Which syscall of the family coreutils issues there has not been checked.

### Open questions

- Is the `EROFS` residual in the doc comment at `detcore/src/syscalls/files.rs:3330-3333` stale now that the reissue goes through `mnt_want_write`? T7 measures it.
- The design carries `TODO-HUMAN-REVIEW(#1851)` (`detcore/src/syscall_classification.rs:676`), so the owner has not yet signed off on emulating the identity half.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fchownat -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(260, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fchownat(-1, 0xffffffffffffffff, -1, -1, AT_SYMLINK_NOFOLLOW|AT_REMOVEDIR|AT_SYMLINK_FOLLOW|AT_NO_AUTOMOUNT|AT_EMPTY_PATH|AT_RECURSIVE|0xffff60ff) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism. The `EINVAL` here is the flag check (F7), which runs before the bad `path` pointer could give `EFAULT`.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/353#issuecomment-5872982001
