---
schema: syscall-pages/v1
syscall: chown
nr_x86_64: 92
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/337
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+chown&body=Discussion+for+the+%60chown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fchown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60chown%60%2C+or+in+what+the+page+says%3F%0A
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

`chown(path, owner, group)` changes the owner and group of the file named by `path`, following a symlink in the last component. Detcore does not forward it: it emulates the call as the success a real root process gets, checks the path with a side-effect-free `newfstatat`, and has the kernel apply the other effects of a successful chown (clearing set-ID bits, moving ctime) by reissuing `chown(path, -1, -1)` (`detcore/src/syscalls/files.rs:3353`). The requested owner and group are never applied, so a later `stat` shows the old owner.

Manual reference: Linux man-pages chown(2).

## Linux semantics

Path lookup follows symlinks in every component, including the last. After lookup the kernel takes write access to the mount (`mnt_want_write`), then `chown_common` builds the attribute change and `notify_change` applies it through the filesystem's setattr, the idmapping and LSM hooks.

Permission: changing the owner needs `CAP_CHOWN` in the inode's user namespace. The file owner may change the group only to a group it belongs to; otherwise `CAP_CHOWN` is needed. An id with no mapping in the caller's user namespace gives `EINVAL` before any permission check. So both the errno and the success depend on the caller's credentials and user-namespace mapping, not just on the arguments.

Even when neither id changes, a successful call on a non-directory sets `ATTR_KILL_SUID | ATTR_KILL_SGID` (and drops file capabilities), and always sets `ATTR_CTIME`. `S_ISUID` is cleared; `S_ISGID` is cleared only if the file is group-executable; directories keep both bits. The guest at `tests/c/chown_virtual_root_identity.c:228-338` records the native measurements: 06755 becomes 0755, 02644 stays 02644, a 06755 directory stays 06755, and ctime moves in every case.

### Flags and options

- F1: an id argument of `-1` leaves that id unchanged; with both `-1` no id changes, but F4 still applies.
- F2: an explicit owner and/or group (0, the caller's own id, or a foreign id): changes ownership. Linux returns `EPERM` without the needed privilege, and `EINVAL` for an id unmapped in the caller's user namespace.
- F3: a symlink as the last path component is followed and the target is changed; a dangling symlink gives `ENOENT`.
- F4: side effects of success: a non-directory loses `S_ISUID`, loses `S_ISGID` only if `S_IXGRP` is set, and loses file capabilities; directories keep set-ID bits; ctime is updated even if nothing else changes.
- F5: path-resolution errors: `ENOENT` (missing component or empty path), `ENOTDIR`, `ELOOP`, `ENAMETOOLONG`, `EFAULT` (bad `path` pointer), `EACCES` (no search permission on a prefix).
- F6: write-side error after lookup: `EROFS` on a read-only mount in the guest's mount namespace.
- F7: host-config write-side errors: `EPERM` on an inode the host marked immutable or append-only (setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest lacks), and denials by the host's LSM policy.
- F8: host-transient write-side error: `EIO` from failing host storage.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. For a fixed path, fixed ids, fixed credentials and user-namespace mapping, and filesystem state that nothing outside the guest changes, the result, the errno and the metadata transition repeat. They change with the caller's identity (F2: `EPERM` versus `EINVAL` versus success), with the namespace configuration, and with outside changes to the path. The ctime written by F4 is the host clock and differs on every run; Hermit's metadata virtualization is what keeps a later `stat` stable, not this call.

### Nondeterminism sources

- External metadata mutation
- User-namespace ID mappings and capability policy
- Filesystem and LSM policy
- Host clock value written to ctime (F4)

### Cross-host variation

- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F7) where the reissued `chown(path, -1, -1)` changes the mode, on that host and not on another.
- Host LSM policy: a denial (F7) depends on which LSM the host runs and its rules.

## Hermit design

Target policy: `emulate`, changed from the legacy `passthrough`. Code at hermit f4055eb9cae4:

- `detcore/src/syscall_classification.rs:685` puts `chown` in the Determinized set (the comment at `detcore/src/syscall_classification.rs:676` carries `TODO-HUMAN-REVIEW(#1851)`). The arm at `detcore/src/lib.rs:2303-2307` sends the four ownership syscalls to `handle_ownership_change_noop` (`detcore/src/syscalls/files.rs:3353`).
- Identity half, emulated. The requested ids are never applied and the handler returns 0 once validation passes (`detcore/src/syscalls/files.rs:3474`). The host-identity errnos of F2 (`EPERM`, `EINVAL` for an unmapped id) cannot reach the guest. This matches the rest of the virtual-root identity: `getuid` and friends report 0, and the credential-setting family succeeds as a no-op (`detcore/src/lib.rs:2272-2276`).
- Argument half, delegated. `chown` becomes `newfstatat(AT_FDCWD, path, stat, 0)` (`detcore/src/syscalls/files.rs:3403`), which follows symlinks like chown, so F3 and F5 errnos come from the kernel. Its errno is returned and the scratch stat buffer is zeroed on both paths (`detcore/src/syscalls/files.rs:3430-3434`).
- Consequence half, delegated. The handler reissues `chown(path, -1, -1)` (`detcore/src/syscalls/files.rs:3443`, `detcore/src/syscalls/files.rs:3451`, `detcore/src/syscalls/files.rs:3473`), so F1 and F4 come from the kernel, and an errno from that call propagates.
- All three calls go through `record_or_replay`, so replay does not need the guest's files (`detcore/src/syscalls/files.rs:3320`).

Why `emulate` and not the alternatives:

- `passthrough` returns whatever the host identity gets: `EPERM` even for `chown(path, 0, 0)` under `--no-namespace`, and `EINVAL` for any uid but 0 under the default one-uid `uid_map` (`detcore/src/syscall_classification.rs:1001-1008`). The guest believes it is root, so that answer is wrong and depends on the configuration.
- `model` would mean tracking per-file ownership so a later `stat` agrees. Detcore does not do that (`detcore/src/syscall_classification.rs:1016-1024`). Under the default namespace, guest-created files already read back as 0:0, so `chown(path, 0, 0)` agrees with `stat`; a chown to a foreign uid followed by a `stat` does not.
- `refuse` would break `install`, `tar`, `cp -p` and package scripts that chown files they created.

Known differences from Linux:

- Ownership changes are not observable (stated boundary, `detcore/src/syscalls/files.rs:3323-3328`).
- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, path resolution checks search permission against the launching host user, so `chown` can return `EACCES` on a path prefix the guest, told it is root, expects to pass (`detcore/src/syscalls/files.rs:3333-3337`), and the `(-1, -1)` reissue returns `EPERM` on every file that user does not own, root-owned ones included (`detcore/src/syscalls/files.rs:3339-3347`).
- The doc comment at `detcore/src/syscalls/files.rs:3330-3333` says a target on a read-only mount passes where Linux returns `EROFS`. My reading of Linux `fs/open.c` is that `do_fchownat` calls `mnt_want_write` before `chown_common`, so the `(-1, -1)` reissue itself should now fail with `EROFS` and propagate, which would make that residual stale. This is a hypothesis from the kernel source, not a measurement; T4 settles it.
- On a target the guest does not own, the reissue can return `EPERM` (`detcore/src/syscalls/files.rs:3339-3347`). That is the same answer pass-through gave.
- Hypothesis, not measured: on an immutable or append-only file, Linux refuses a real ownership change with `EPERM`, but `notify_change` checks immutability only when the change includes mode, ids or explicit times, so the `(-1, -1)` reissue on a file with no set-ID bits may succeed and Hermit would return 0.
- Target (owner decision 6, 2026-10-04): the emulation owes Linux's immutable and append-only check, because it claims Linux behaviour. In the kernel, `do_fchownat` (fs/open.c) resolves the path, then `mnt_want_write` gives `EROFS`, then `notify_change` (fs/attr.c) gives `EPERM` for an immutable or append-only inode whenever ids change. So after the probe (`detcore/src/syscalls/files.rs:3403`, path errors) and before the `(-1, -1)` reissue (`detcore/src/syscalls/files.rs:3473`), Detcore reads the inode's flags, for example by making the probe a `statx` with the same target arguments and reading `STATX_ATTR_IMMUTABLE` and `STATX_ATTR_APPEND`, and returns `EPERM` for either flag, except on a read-only mount, where `EROFS` still comes first. A program that expects `chown` on a locked file to fail then gets Linux's answer instead of a success, and the reissue no longer moves the ctime of an immutable file.
- Owner decision 4 models the credential record without enforcement, so after a modelled `setuid(1000)` that leaves no `CAP_CHOWN`, `chown` still returns the virtual-root success where Linux returns `EPERM`: neither the emulation nor the kernel consults the record. `target_policy` stays `emulate`.

## Recommended tests

- T1 (covers F1, F2, F4): under `hermit run --strict` on ptrace in its container, chown a file the guest created to a foreign uid, to 0:0 and to -1:-1. Every call returns 0, `stat` shows the owner unchanged, a 06755 file becomes 0755, a 02644 file keeps `S_ISGID`, a 06755 directory keeps both bits, and ctime moves (metadata virtualization off).
  Existing candidates: `chown_family_keeps_virtual_root_identity_and_real_errors` at `hermit-cli/tests/chown_virtual_root_identity.rs:185` and `chown_family_keeps_virtual_root_identity_without_a_user_namespace` at `hermit-cli/tests/chown_virtual_root_identity.rs:197`, which run the guest `tests/c/chown_virtual_root_identity.c` (chown calls at lines 146, 147, 155, 282 and 318). The Rust file passes no `--backend` flag. Of these, `chown_family_keeps_virtual_root_identity_without_a_user_namespace` exercises a reduced-fidelity mode (owner decision 3) and does not count toward `done`.
- T2 (covers F3): chown through a symlink: returns 0, the target's set-ID bits are cleared and the link is untouched; chown through a dangling symlink returns `ENOENT`. No existing test calls `chown` on a symlink.
- T3 (covers F5): `ENOENT` for a missing file and for an empty path, `ENOTDIR` for `file/child`, `ELOOP` for a symlink loop, `ENAMETOOLONG`, and `EFAULT` for a bad `path` pointer.
  Existing partial coverage: `ENOENT` at `tests/c/chown_virtual_root_identity.c:161` and `ENOTDIR` at `tests/c/chown_virtual_root_identity.c:185-188`.
- T4 (covers F6): chown a file on a read-only bind mount inside the container and record Hermit's errno next to the native `EROFS`. This confirms or refutes the hypothesis that the `(-1, -1)` reissue now surfaces `EROFS`. The immutable case is F7, recorded as U1 rather than tested; measuring the immutable hypothesis under Hermit design still needs a host with an immutable file the guest can see.
- T5 (covers F1, F2, F5): record and replay a guest that chowns with -1:-1, with a foreign uid, and on a missing path; replay must return the recorded results without the files.
  Existing partial coverage: `record_path_queries_and_legacy_mutations` at `hermit-cli/tests/record_replay.rs:2171`, whose guest calls `chown("b", -1, -1)` at `tests/c/record_replay_path_queries.c:241` (F1 only).
- U1 (covers F7; host-config): only the host can mark an inode immutable or append-only or install an LSM policy, so CI cannot produce these errors on demand. Hermit hands the kernel's errno from the reissued `chown(path, -1, -1)` to the guest unchanged (`detcore/src/syscalls/files.rs:3473`).
- U2 (covers F8; host-transient): `EIO` comes from failing host storage shared with other processes. The probe `newfstatat` (`detcore/src/syscalls/files.rs:3430`) and the reissued call (`detcore/src/syscalls/files.rs:3473`) hand it to the guest today. F8 counts toward done only once Hermit hides this error or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/337#issuecomment-5872944730
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/337: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_ownership_change_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; chown records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/337 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> emulate (Detcore emulates the identity half as a virtual-root success, validates the path with a newfstatat, and delegates set-ID clearing and ctime to a chown(path, -1, -1) reissue; see detcore/src/syscalls/files.rs:3353); linux_determinism stays conditional; 6 flag behaviours and 5 tests listed; no coverage token because no test has been run against this list, so tracking_status stays todo; two kernel-reading hypotheses (EROFS now surfaces, immutable files) are left as open questions for T4
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (inode flags on host files, host LSM policy); F6 narrowed to EROFS, with immutable and LSM EPERM split out as new F7 (host-config U1) and EIO as new F8 (host-transient U2); T4 no longer asks for an immutable file; no open question is resolved by decisions 1 or 2 (the immutable question is about Hermit's emulated success, not reachability)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): the --no-namespace EACCES known difference became the reduced-fidelity line (host-user path search and reissue EPERM under --no-namespace and namespace-less backends); noted that a modelled setuid does not change the emulated success; target_policy unchanged (emulate); T1 narrowed from both namespace configurations to the strict ptrace container; the namespace-less candidate test does not count toward done; no new F/T ids; no open question removed (EROFS, immutable and #1851 questions are outside decisions 3 and 4)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (the emulation owes notify_change's immutable/append-only EPERM, after path errors and EROFS as in the kernel; Target bullet added); target_policy stays emulate; no new F/T ids (F7 stays host-config under U1); removed the immutable-target question; kept the EROFS measurement and #1851 questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `chown`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+chown&body=Discussion+for+the+%60chown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fchown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60chown%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: chown" (https://github.com/rrnewton/hermit/issues/337), legacy variant A2. It was ranked 58 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/337.

The determinization was implemented by https://github.com/rrnewton/hermit/pull/1851. The membership unit test `ownership_change_family_is_determinized_not_passthrough` (`detcore/src/syscall_classification.rs:2785`) pins only set membership; its own comment says it stays green when the arm's result regresses.

The compat cell `chown` (`tests/e2e/manifests/compat.yaml:234`) runs coreutils `chown --reference` under `--verify` on ptrace. Which syscall of the family coreutils issues there has not been checked.

### Open questions

- Is the `EROFS` residual in the doc comment at `detcore/src/syscalls/files.rs:3330-3333` stale now that the `(-1, -1)` reissue goes through `mnt_want_write`? T4 measures it; if it is stale, the comment should be corrected.
- The design carries `TODO-HUMAN-REVIEW(#1851)` (`detcore/src/syscall_classification.rs:676`), so the owner has not yet signed off on emulating the identity half.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=chown -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(92, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
chown(0xffffffffffffffff, -1, -1) = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/337#issuecomment-5872944730
