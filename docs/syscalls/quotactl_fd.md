---
schema: syscall-pages/v1
syscall: quotactl_fd
nr_x86_64: 443
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/392
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+quotactl_fd&body=Discussion+for+the+%60quotactl_fd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fquotactl_fd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60quotactl_fd%60%2C+or+in+what+the+page+says%3F%0A
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

`quotactl_fd(fd, cmd, id, addr)` is `quotactl` with the filesystem named by an open file descriptor on it instead of a block-device path (Linux 5.14 and later). Hermit refuses it: Detcore answers every call with a fixed `EPERM` and never forwards it to the host kernel (`detcore/src/lib.rs:2164`), so the descriptor is never examined.

## Linux semantics

Added in Linux 5.14. `cmd` is the same `QCMD(subcmd, type)` multiplex as for `quotactl`, and the subcommands behave the same once the filesystem is found. The differences are in how the filesystem is named:

1. `fd` is checked first: a descriptor that is not open gives `EBADF`. Any open descriptor on the filesystem works, including an `O_PATH` one. The host strace under Additional information shows the `EBADF` case.
2. Then the quota type: `type >= MAXQUOTAS` gives `EINVAL`.
3. Write subcommands need write access to the descriptor's mount (`EROFS` on a read-only mount).
4. The filesystem support check (`ENOSYS`, `EINVAL`), the permission check (`EPERM`), and the subcommand follow, as for `quotactl`.

No block device is involved, so the `ENOENT`/`ENOTBLK`/`ENODEV` path errors of `quotactl` do not occur, and there is no "every filesystem" `Q_SYNC`. The kernel passes no quota-file path for `Q_QUOTAON`, so that subcommand works only on filesystems that keep quota data in hidden inodes.

### Flags and options

- F1: quota type, the low byte of `cmd` (`QCMD(subcmd, type)`): `USRQUOTA`, `GRPQUOTA` or `PRJQUOTA`. A type of `MAXQUOTAS` or more gives `EINVAL` before anything else is checked; a type the filesystem does not support gives `EINVAL` later.
- F2: `Q_QUOTAON`: turn quotas on; `id` is the quota format (`QFMT_VFS_OLD`, `QFMT_VFS_V0`, `QFMT_VFS_V1`) and `addr` the quota file's path. Needs `CAP_SYS_ADMIN`. Through `quotactl_fd` no path is passed, so filesystems that need a quota file give `EINVAL`; only filesystems with hidden quota inodes can turn quotas on this way.
- F3: `Q_QUOTAOFF`: turn quotas off. Needs `CAP_SYS_ADMIN`.
- F4: `Q_GETQUOTA`: read limits and current usage for `id` into a `struct if_dqblk`. Unprivileged for the caller's own uid (`USRQUOTA`) or a group it belongs to (`GRPQUOTA`); any other `id` needs `CAP_SYS_ADMIN`.
- F5: `Q_GETNEXTQUOTA`: like `Q_GETQUOTA` for the first `id` at or above the given one that has a quota; returns the `id` found. Needs `CAP_SYS_ADMIN`.
- F6: `Q_SETQUOTA`: set limits (and optionally usage) for `id`. Needs `CAP_SYS_ADMIN`.
- F7: `Q_GETINFO` and `Q_SETINFO`: read or set grace times and flags (`struct if_dqinfo`). `Q_GETINFO` is unprivileged; `Q_SETINFO` needs `CAP_SYS_ADMIN`.
- F8: `Q_GETFMT`: read the quota format in use. Unprivileged.
- F9: `Q_SYNC`: write the filesystem's in-memory quota data to disk. Unprivileged.
- F10: XFS-style state commands `Q_XQUOTAON`, `Q_XQUOTAOFF` and `Q_XQUOTARM`: change accounting and enforcement flags or free quota storage. Need `CAP_SYS_ADMIN`.
- F11: XFS-style per-id commands `Q_XGETQUOTA`, `Q_XGETNEXTQUOTA` and `Q_XSETQLIM` (`struct fs_disk_quota`). `Q_XGETQUOTA` follows the `Q_GETQUOTA` rule; the other two need `CAP_SYS_ADMIN`.
- F12: XFS-style status commands `Q_XGETQSTAT`, `Q_XGETQSTATV` and `Q_XQUOTASYNC`. Unprivileged.
- F13: unknown subcommand: an unprivileged caller gets `EPERM` from the permission check, a privileged one `EINVAL`.
- F14: filesystem state: no quota support on the filesystem gives `ENOSYS`; quotas supported but not turned on gives `ESRCH` for most queries; a bad `addr` gives `EFAULT`.
- F15: `fd`: a descriptor that is not open gives `EBADF`, checked before everything else. Any open descriptor on the filesystem, including `O_PATH`, names it.
- F16: write subcommands (`Q_QUOTAON`, `Q_QUOTAOFF`, `Q_SETQUOTA`, `Q_SETINFO`, the XFS setters) on a read-only mount give `EROFS`.

## Determinism analysis

`linux_determinism: conditional`, kept from the migration. The same profile as `quotactl`: argument errors (F1, F13, F15) repeat for fixed arguments and descriptors, while the unprivileged queries (F4 for the caller's own id, F7 `Q_GETINFO`, F8, F12) return live usage counters and grace state of a host filesystem. The legacy note that "the fd argument must additionally be virtualized" no longer applies: Hermit's refusal never reads `fd`.

### Nondeterminism sources

- Quota usage and limit reads reflect live host filesystem state (F4, F5, F7, F11, F12)
- Availability depends on the host filesystem's quota support and whether quotas are on (F14), and on privilege (F2, F3, F6, F10)
- Which filesystem the descriptor refers to and whether its mount is writable (F15, F16)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it. `classify_syscall` lists `quotactl_fd` as Determinized with the rest of the privileged system-administration family (`detcore/src/syscall_classification.rs:289` to `detcore/src/syscall_classification.rs:315`). `Detcore::handle_syscall_event` then matches it on the `Sysno`, before the typed match, with the guard `is_privileged_admin_refused_syscall` (`detcore/src/lib.rs:2164` to `detcore/src/lib.rs:2168`; the set itself is at `detcore/src/syscall_classification.rs:902`), and returns `Err(Error::Errno(Errno::EPERM))` (`detcore/src/lib.rs:2167`). The arm reads no argument and never forwards the call to the host kernel. It does not consult `panic_on_unsupported_syscalls`, so the answer is the same with and without fail-closed mode, and the comment above the arm (`detcore/src/lib.rs:2154`) states the intent that it be identical across `--verify` and record/replay. The same set feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), which is what `code_facts.refused: true` reports.

Classification-only evidence: the unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks that `quotactl_fd` is Determinized and in the refusal set, and that the set holds nothing else. It does not start Hermit, so it is not coverage for any test below.

How close this is to Linux: the fixed `EPERM` matches an unprivileged Linux caller only for a privileged subcommand with a valid descriptor on a quota-enabled filesystem. It differs, deterministically, for a bad descriptor (Linux `EBADF`, F15; the host strace under Additional information is that case), for an invalid type (Linux `EINVAL`, F1), and for the unprivileged subcommands (F4 for the caller's own id, F7 `Q_GETINFO`, F8, F9, F11 `Q_XGETQUOTA`, F12), where Linux returns data or `ENOSYS`/`ESRCH` (F14).

Why `refuse`: the reasons are those of `quotactl`. Quota queries expose live host filesystem counters and setters change host-global limits, so `passthrough` is ruled out. A descriptor on a guest-visible file names a host filesystem whose quota state Hermit does not own, so there is nothing to `model`.

Target (owner decision 6): keep the one fixed `EPERM`, returned before any argument check, for every `quotactl_fd` call. Linux's own order (`SYSCALL_DEFINE4(quotactl_fd)` in `fs/quota/quota.c`: the descriptor (`EBADF`) and quota-type (`EINVAL`) checks, then `mnt_want_write()` for write subcommands, then `check_quotactl_permission()` inside `do_quotactl()`) puts argument errors first, and the target deliberately does not reproduce them: F1, F13 for a privileged caller, F14, F15 and F16 all become `EPERM`. The unprivileged subcommands (F4 for the caller's own id, F7 `Q_GETINFO`, F8, F9, F11 `Q_XGETQUOTA`, F12) also get `EPERM`: they are not among the decision's Linux-unprivileged exceptions, and answering them as Linux does would hand the guest host quota state. A guest sees a system where quota administration is not permitted; `quota`/`repquota` report the error and exit. The code already does this (`detcore/src/lib.rs:2164-2168`), so no Hermit change is needed.

The code realizes `refuse`. No test names `SYS_quotactl_fd` (`tests_naming: 0`), so verification on the default backend stays untested.

## Recommended tests

- T1 (covers F1, F13, F15): under `hermit run --verify` on ptrace, call raw `syscall(SYS_quotactl_fd, ...)` with `fd = -1`, with an `O_PATH` descriptor on `/` and `cmd = 0x12345678`, and with `QCMD(Q_GETQUOTA, 7)` (an invalid type). Assert -1 with `errno == EPERM`, the fixed answer owner decision 6 keeps, and identical verify runs. Native Linux returns `EBADF` or `EINVAL` here, so this case also tells the refusal from forwarding.
- T2 (covers F2, F3, F5, F6, F7, F10, F16): with a descriptor on the guest's root directory, issue `Q_QUOTAON`, `Q_QUOTAOFF`, `Q_GETNEXTQUOTA`, `Q_SETQUOTA`, `Q_SETINFO`, `Q_XQUOTAON`, `Q_XQUOTAOFF` and `Q_XQUOTARM`. Assert `EPERM` for each and identical verify runs.
- T3 (covers F4, F7, F8, F9, F11, F12, F14): issue the unprivileged subcommands `Q_GETQUOTA` and `Q_XGETQUOTA` for the guest's own uid, `Q_GETINFO`, `Q_GETFMT`, `Q_SYNC`, `Q_XGETQSTAT`, `Q_XGETQSTATV` and `Q_XQUOTASYNC`. Assert `EPERM` for each (owner decision 6) and that no output depends on host quota state.
- T4 (covers F15): record a guest that calls `quotactl_fd` under `hermit record`, replay it, and assert the replay returns the same answer at the same point and the run reproduces.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/392: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (code returns a fixed EPERM before reading arguments, detcore/src/lib.rs:2164-2168; host quota state must not reach the guest); linux_determinism stays conditional; 16 flag behaviours and 4 tests listed; left todo with an owner question: EPERM differs from Linux for a bad fd (EBADF), invalid type and unprivileged subcommands
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied: the fixed EPERM before any argument check is the target (code already realizes it, detcore/src/lib.rs:2164-2168), including the unprivileged subcommands; target_policy stays refuse; T1 and T3 now assert EPERM; open question on the refusal answer removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `quotactl_fd`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+quotactl_fd&body=Discussion+for+the+%60quotactl_fd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fquotactl_fd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60quotactl_fd%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: quotactl_fd" (https://github.com/rrnewton/hermit/issues/392), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=quotactl_fd` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `quotactl_fd` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
quotactl_fd(-1, QCMD(0 /* Q_??? */, USRQUOTA), 0, NULL) = -1 EBADF (Bad file descriptor)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
