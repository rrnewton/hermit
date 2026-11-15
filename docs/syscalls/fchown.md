---
schema: syscall-pages/v1
syscall: fchown
nr_x86_64: 93
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/352
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchown&body=Discussion+for+the+%60fchown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchown%60%2C+or+in+what+the+page+says%3F%0A
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

`fchown(fd, owner, group)` changes the owner and group of the file an open descriptor refers to. Detcore does not forward it: it emulates the call as the success a real root process gets, checks the descriptor with `fcntl(fd, F_GETFL)` (refusing an `O_PATH` descriptor with `EBADF`), and has the kernel apply the other effects of a successful chown by reissuing `fchown(fd, -1, -1)` (`detcore/src/syscalls/files.rs:3353`). The requested owner and group are never applied.

Manual reference: Linux man-pages chown(2).

## Linux semantics

The kernel looks up `fd` with `fdget`, which does not accept an `O_PATH` descriptor, takes write access to the mount (`mnt_want_write_file`), and then runs the same `chown_common`/`notify_change` path as `chown`, through idmapping and LSM hooks. The descriptor's access mode does not matter: a read-only descriptor can be used.

Permission: changing the owner needs `CAP_CHOWN` in the inode's user namespace; the owner may change the group only to a group it belongs to. An id unmapped in the caller's user namespace gives `EINVAL`. The errno and the success therefore depend on credentials and namespace mapping.

A successful call on a non-directory clears `S_ISUID`, clears `S_ISGID` only if the file is group-executable, and drops file capabilities; directories keep set-ID bits; ctime is always updated, even when both ids are `-1`.

### Flags and options

- F1: an id argument of `-1` leaves that id unchanged; with both `-1` no id changes, but F4 still applies.
- F2: an explicit owner and/or group (0, the caller's own id, or a foreign id): changes ownership. Linux returns `EPERM` without the needed privilege, and `EINVAL` for an id unmapped in the caller's user namespace.
- F3: descriptor validity: `EBADF` if `fd` is not open or is an `O_PATH` descriptor; a descriptor opened read-only or write-only is accepted.
- F4: side effects of success: a non-directory loses `S_ISUID`, loses `S_ISGID` only if `S_IXGRP` is set, and loses file capabilities; directories keep set-ID bits; ctime is updated even if nothing else changes.
- F5: write-side error: `EROFS` on a read-only mount in the guest's mount namespace.
- F6: host-config write-side errors: `EPERM` on an inode the host marked immutable or append-only (setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest lacks), and denials by the host's LSM policy.
- F7: host-transient write-side error: `EIO` from failing host storage.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. For a fixed descriptor, fixed ids, fixed credentials and ID mappings, and metadata nothing outside the guest changes, the result, the errno and the metadata transition repeat. They change with the caller's identity (F2), the namespace configuration, and outside changes to the file. The ctime written by F4 is the host clock; Hermit's metadata virtualization keeps a later `stat` stable.

### Nondeterminism sources

- User-namespace ID mappings
- External metadata mutation
- Capability and LSM policy
- Host clock value written to ctime (F4)

### Cross-host variation

- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F6) where the reissued `fchown(fd, -1, -1)` changes the mode, on that host and not on another.
- Host LSM policy: a denial (F6) depends on which LSM the host runs and its rules.

## Hermit design

Target policy: `emulate`, changed from the legacy `passthrough`. Code at hermit f4055eb9cae4:

- `detcore/src/syscall_classification.rs:686` puts `fchown` in the Determinized set (comment with `TODO-HUMAN-REVIEW(#1851)` at `detcore/src/syscall_classification.rs:676`). The arm at `detcore/src/lib.rs:2303-2307` sends it to `handle_ownership_change_noop` (`detcore/src/syscalls/files.rs:3353`).
- Identity half, emulated. The requested ids are never applied; the handler returns 0 after validation (`detcore/src/syscalls/files.rs:3474`), so F2's host-identity `EPERM`/`EINVAL` cannot reach the guest. This matches the virtual-root identity used for `getuid` and for the credential-setting no-ops (`detcore/src/lib.rs:2272-2276`).
- Argument half. `fcntl(fd, F_GETFL)` goes through `record_or_replay` (`detcore/src/syscalls/files.rs:3387-3393`): a closed descriptor gives the kernel's `EBADF`, and a descriptor whose flags include `O_PATH` gets `EBADF` from Detcore (`detcore/src/syscalls/files.rs:3394-3395`), because `fstat` would accept it where `fchown` does not (F3).
- Consequence half, delegated. The handler reissues `fchown(fd, -1, -1)` (`detcore/src/syscalls/files.rs:3443-3445`, `detcore/src/syscalls/files.rs:3473`), so F1 and F4 come from the kernel and an errno from that call propagates.
- Replay does not need the file to exist, because every call goes through `record_or_replay` (`detcore/src/syscalls/files.rs:3320`).

Why `emulate`: `passthrough` returns the host identity's answer, `EPERM` with no user namespace and `EINVAL` for an unmapped id under the default one-uid map (`detcore/src/syscall_classification.rs:1001-1008`), while the guest believes it is root. `model` would need per-file ownership tracking, which Detcore does not have (`detcore/src/syscall_classification.rs:1016-1024`). `refuse` would break programs that chown files they create.

Known differences from Linux:

- The ownership change is not observable through a later `fstat` (`detcore/src/syscalls/files.rs:3323-3328`).
- The doc comment at `detcore/src/syscalls/files.rs:3330-3333` says a read-only mount target passes where Linux returns `EROFS`. By my reading of Linux `fs/open.c`, `ksys_fchown` calls `mnt_want_write_file` before `chown_common`, so the `(-1, -1)` reissue should now return `EROFS` and propagate. Hypothesis from the kernel source; T3 settles it.
- On a file the guest does not own, the reissue can return `EPERM` (`detcore/src/syscalls/files.rs:3339-3347`), which is what pass-through returned.
- Hypothesis, not measured: an immutable or append-only file without set-ID bits may accept the `(-1, -1)` reissue, so Hermit would return 0 where Linux refuses a real ownership change with `EPERM`.

Target (owner decision 6): because `fchown` is emulated, it owes Linux's checks in kernel order on the half it emulates, and the immutable and append-only case is one of them. In `fs/open.c`, `ksys_fchown()` takes the descriptor with `fdget()` (`EBADF`), `vfs_fchown()` calls `mnt_want_write_file()` (`EROFS`), and `chown_common()` sets `ATTR_UID`/`ATTR_GID` for each id that is not -1 before `notify_change()`; there `may_setattr()` (`fs/attr.c`) returns `EPERM` for `ATTR_UID`/`ATTR_GID` on an inode that `IS_IMMUTABLE` or `IS_APPEND`. So when either id is not -1 and the inode carries one of those flags, Hermit returns `EPERM`, after the descriptor checks (`detcore/src/syscalls/files.rs:3387-3395`) and the write-access check, instead of 0. With both ids -1, the reissued `fchown(fd, -1, -1)` (`detcore/src/syscalls/files.rs:3473`) already gives Linux's answer. Today the handler applies no such check, so the case returns 0 when the reissue succeeds; the change is a read of the inode flags (for example `FS_IOC_GETFLAGS` on the descriptor, with a filesystem that does not support it treated as having neither flag) before the reissue. Only the host can set those flags, so F6 stays a `host-config` item (U1), and the flag read is the place where Hermit takes that host state. Whether the requested ids should be applied at all stays with the owner (https://github.com/rrnewton/hermit/pull/1851); this target does not change the identity half.

## Recommended tests

- T1 (covers F1, F2, F4): under `hermit run` on ptrace, in both namespace configurations, `fchown` an open descriptor of a 06755 file to a foreign uid and to -1:-1. Both return 0, `fstat` shows the owner unchanged, the mode becomes 0755 and ctime moves (metadata virtualization off). Repeat on a directory descriptor, whose set-ID bits must survive.
  Existing partial coverage: `chown_family_keeps_virtual_root_identity_and_real_errors` at `hermit-cli/tests/chown_virtual_root_identity.rs:185` and `chown_family_keeps_virtual_root_identity_without_a_user_namespace` at `hermit-cli/tests/chown_virtual_root_identity.rs:197`; the guest calls `fchown(fd, FOREIGN_UID, FOREIGN_GID)` at `tests/c/chown_virtual_root_identity.c:148`. Its mode and ctime read-back comes after several family calls, so it does not show that `fchown` alone cleared the bits, and it never calls `fchown` with -1:-1.
- T2 (covers F3): `EBADF` for fd -1, for an unopened fd, and for an `O_PATH` descriptor; success on a descriptor opened `O_RDONLY`.
  Existing partial coverage: `tests/c/chown_virtual_root_identity.c:170`, `tests/c/chown_virtual_root_identity.c:172` and `tests/c/chown_virtual_root_identity.c:181` (no read-only descriptor case).
- T3 (covers F5): `fchown` a read-only descriptor of a file on a read-only bind mount and record Hermit's errno next to the native `EROFS`, which confirms or refutes the reissue hypothesis.
- T4 (covers F2): under `hermit run --strict --verify` on ptrace, `fchown(fd, getuid(), getgid())` returns 0 in both runs.
  Existing candidates: cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`, verify ci on ptrace) and `deterministic_file_metadata_syscalls_verify` at `hermit-cli/tests/syscall_file_metadata.rs:51`; the guest call is at `tests/c/syscall_file_metadata.c:148`.
- T5 (covers F1, F2, F3): record and replay a guest that calls `fchown` with -1:-1, with a foreign uid and on a closed descriptor; replay returns the recorded results. No existing test.
- U1 (covers F6; host-config): only the host can mark an inode immutable or append-only or install an LSM policy, so CI cannot produce these errors on demand. Hermit hands the kernel's errno from the reissued `fchown(fd, -1, -1)` to the guest unchanged (`detcore/src/syscalls/files.rs:3473`).
- U2 (covers F7; host-transient): `EIO` comes from failing host storage shared with other processes. The reissued `fchown(fd, -1, -1)` (`detcore/src/syscalls/files.rs:3473`) hands it to the guest today. F7 counts toward done only once Hermit hides this error or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/352#issuecomment-5872979699
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/352: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_ownership_change_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; fchown records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/352 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> emulate (Detcore emulates the identity half as a virtual-root success, validates the descriptor with F_GETFL and refuses O_PATH with EBADF, and delegates set-ID clearing and ctime to an fchown(fd, -1, -1) reissue; see detcore/src/syscalls/files.rs:3353); linux_determinism stays conditional; 5 flag behaviours and 5 tests listed; no coverage token because no test has been run against this list, so tracking_status stays todo; two kernel-reading hypotheses (EROFS now surfaces, immutable files) are left as open questions for T3
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (inode flags on host files, host LSM policy); F5 narrowed to EROFS, with immutable and LSM EPERM split out as new F6 (host-config U1) and EIO as new F7 (host-transient U2); no open question is resolved by decisions 1 or 2 (the EROFS and immutable questions are about Hermit's emulated result, not reachability)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied to the emulated half: target is EPERM for an explicit id on an immutable or append-only inode, in Linux's order (fdget EBADF, mnt_want_write_file EROFS, then may_setattr EPERM); today the (-1, -1) reissue at detcore/src/syscalls/files.rs:3473 can succeed there, so Hermit returns 0; target_policy stays emulate; immutable/append-only open question removed; the EROFS measurement and https://github.com/rrnewton/hermit/pull/1851 questions kept
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T4 covered by tests/c/syscall_file_metadata.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fchown`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchown&body=Discussion+for+the+%60fchown%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchown.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchown%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fchown" (https://github.com/rrnewton/hermit/issues/352), legacy variant A2. It was ranked 73 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/352.

The determinization was implemented by https://github.com/rrnewton/hermit/pull/1851. The membership unit test `ownership_change_family_is_determinized_not_passthrough` (`detcore/src/syscall_classification.rs:2785`) pins only set membership.

### Open questions

- Is the `EROFS` residual in the doc comment at `detcore/src/syscalls/files.rs:3330-3333` stale now that the reissue goes through `mnt_want_write_file`? T3 measures it.
- The design carries `TODO-HUMAN-REVIEW(#1851)` (`detcore/src/syscall_classification.rs:676`), so the owner has not yet signed off on emulating the identity half.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fchown -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(93, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fchown(-1, -1, -1)              = -1 EBADF (Bad file descriptor)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/352#issuecomment-5872979699
