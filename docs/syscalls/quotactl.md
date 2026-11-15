---
schema: syscall-pages/v1
syscall: quotactl
nr_x86_64: 179
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/391
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+quotactl&body=Discussion+for+the+%60quotactl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fquotactl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60quotactl%60%2C+or+in+what+the+page+says%3F%0A
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

`quotactl(cmd, special, id, addr)` manipulates disk quotas (turn on/off, get/set limits, query usage) for the filesystem on the block device named by `special`. Hermit refuses it: Detcore answers every call with a fixed `EPERM` and never forwards it to the host kernel (`detcore/src/lib.rs:2164`).

## Linux semantics

`cmd` multiplexes a subcommand and a quota type: `QCMD(subcmd, type)`. The kernel checks in this order:

1. The quota type: `type >= MAXQUOTAS` gives `EINVAL`.
2. `special == NULL`: `Q_SYNC` then syncs quotas on every filesystem and returns 0; any other subcommand gives `ENODEV`. The host strace under Additional information shows that `ENODEV` case.
3. `special` is resolved as a path to a block device whose filesystem is mounted: `ENOENT`, `ENOTBLK` or `ENODEV` when that fails. Write subcommands also need the mount to be writable.
4. The filesystem must support quotas (`ENOSYS`) and the quota type (`EINVAL`).
5. The permission check (F13 covers unknown subcommands): read-only status subcommands are unprivileged, a query of the caller's own id is unprivileged, and everything else needs `CAP_SYS_ADMIN`; otherwise `EPERM`.
6. The subcommand itself, which can still fail with `ESRCH` when quotas are not turned on.

The queries return the live usage counters of a host filesystem. The setters change host-global limits that affect every process writing to that filesystem.

### Flags and options

- F1: quota type, the low byte of `cmd` (`QCMD(subcmd, type)`): `USRQUOTA`, `GRPQUOTA` or `PRJQUOTA`. A type of `MAXQUOTAS` or more gives `EINVAL` before anything else is checked; a type the filesystem does not support gives `EINVAL` later.
- F2: `Q_QUOTAON`: turn quotas on; `id` is the quota format (`QFMT_VFS_OLD`, `QFMT_VFS_V0`, `QFMT_VFS_V1`) and `addr` the quota file's path. Needs `CAP_SYS_ADMIN`.
- F3: `Q_QUOTAOFF`: turn quotas off. Needs `CAP_SYS_ADMIN`.
- F4: `Q_GETQUOTA`: read limits and current usage for `id` into a `struct if_dqblk`. Unprivileged for the caller's own uid (`USRQUOTA`) or a group it belongs to (`GRPQUOTA`); any other `id` needs `CAP_SYS_ADMIN`.
- F5: `Q_GETNEXTQUOTA`: like `Q_GETQUOTA` for the first `id` at or above the given one that has a quota; returns the `id` found. Needs `CAP_SYS_ADMIN`.
- F6: `Q_SETQUOTA`: set limits (and optionally usage) for `id`. Needs `CAP_SYS_ADMIN`.
- F7: `Q_GETINFO` and `Q_SETINFO`: read or set grace times and flags (`struct if_dqinfo`). `Q_GETINFO` is unprivileged; `Q_SETINFO` needs `CAP_SYS_ADMIN`.
- F8: `Q_GETFMT`: read the quota format in use. Unprivileged.
- F9: `Q_SYNC`: write the filesystem's in-memory quota data to disk. Unprivileged. With `special == NULL`, syncs every filesystem and returns 0; any other subcommand with `special == NULL` gives `ENODEV`.
- F10: XFS-style state commands `Q_XQUOTAON`, `Q_XQUOTAOFF` and `Q_XQUOTARM`: change accounting and enforcement flags or free quota storage. Need `CAP_SYS_ADMIN`.
- F11: XFS-style per-id commands `Q_XGETQUOTA`, `Q_XGETNEXTQUOTA` and `Q_XSETQLIM` (`struct fs_disk_quota`). `Q_XGETQUOTA` follows the `Q_GETQUOTA` rule; the other two need `CAP_SYS_ADMIN`.
- F12: XFS-style status commands `Q_XGETQSTAT`, `Q_XGETQSTATV` and `Q_XQUOTASYNC`. Unprivileged.
- F13: unknown subcommand: an unprivileged caller gets `EPERM` from the permission check, a privileged one `EINVAL`.
- F14: filesystem state: no quota support on the filesystem gives `ENOSYS`; quotas supported but not turned on gives `ESRCH` for most queries; a bad `addr` gives `EFAULT`.
- F15: `special` resolution: a missing path gives `ENOENT`, a path that is not a block device `ENOTBLK`, a block device with no mounted filesystem (or a Btrfs volume device) `ENODEV`. These checks run before the permission check, so they apply to unprivileged callers.
- F16: write subcommands (`Q_QUOTAON`, `Q_QUOTAOFF`, `Q_SETQUOTA`, `Q_SETINFO`, the XFS setters) on a read-only mount give `EROFS`.

## Determinism analysis

`linux_determinism: conditional`, kept from the migration. Argument errors (F1, F13, F15) and permission refusals are functions of the arguments and the mount table, and repeat when those are fixed. The unprivileged queries (F4 for the caller's own id, F7 `Q_GETINFO`, F8, F12) return live usage counters and grace state of a host filesystem, which change whenever anything on the host writes to it. `Q_SYNC` (F9) writes host quota files. The privileged setters change host-global state.

### Nondeterminism sources

- Quota usage and limit reads reflect live host filesystem state (F4, F5, F7, F11, F12)
- Availability depends on the host filesystem's quota support and whether quotas are on (F14), and on privilege (F2, F3, F6, F10)
- The host mount table and block devices visible at `special` (F15, F16)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it. `classify_syscall` lists `quotactl` as Determinized with the rest of the privileged system-administration family (`detcore/src/syscall_classification.rs:289` to `detcore/src/syscall_classification.rs:315`). `Detcore::handle_syscall_event` then matches it on the `Sysno`, before the typed match, with the guard `is_privileged_admin_refused_syscall` (`detcore/src/lib.rs:2164` to `detcore/src/lib.rs:2168`; the set itself is at `detcore/src/syscall_classification.rs:902`), and returns `Err(Error::Errno(Errno::EPERM))` (`detcore/src/lib.rs:2167`). The arm reads no argument and never forwards the call to the host kernel. It does not consult `panic_on_unsupported_syscalls`, so the answer is the same with and without fail-closed mode, and the comment above the arm (`detcore/src/lib.rs:2154`) states the intent that it be identical across `--verify` and record/replay. The same set feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), which is what `code_facts.refused: true` reports.

Classification-only evidence: the unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks that `quotactl` is Determinized and in the refusal set, and that the set holds nothing else. It does not start Hermit, so it is not coverage for any test below.

How close this is to Linux: the fixed `EPERM` matches an unprivileged Linux caller only for a privileged subcommand with valid arguments on a quota-enabled filesystem. It differs, deterministically, in these cases:

- argument errors: Linux returns `EINVAL` (F1, F13 for a privileged caller), `ENOENT`, `ENOTBLK` or `ENODEV` (F15);
- `Q_SYNC` with `special == NULL` (F9): Linux returns 0;
- the unprivileged subcommands (F4 for the caller's own id, F7 `Q_GETINFO`, F8, F9, F11 `Q_XGETQUOTA`, F12): Linux returns data, or `ENOSYS`/`ESRCH` for a filesystem without quotas (F14).

Why `refuse`:

- Quota queries expose live usage counters of host filesystems, and setters change host-global limits. Neither is container state Hermit can hold fixed, so `passthrough` is ruled out.
- The guest's own filesystem is a container mount, not a host block device it can name through `special`, so there is no guest-owned quota state to `model`.
- Every candidate answer (`EPERM`, `ENOSYS`, or an emulated "quotas off" with Linux's argument checks) refuses to expose host quota state, which is why the policy is `refuse`.

Target (owner decision 6): keep the one fixed `EPERM`, returned before any argument check, for every `quotactl` call. Linux's own order (`SYSCALL_DEFINE4(quotactl)` in `fs/quota/quota.c`: the type check, the `special == NULL` case, `quotactl_block()`, then `check_quotactl_permission()` inside `do_quotactl()`) puts argument errors before the permission check, and the target deliberately does not reproduce them: F1, F13 for a privileged caller, F14 and F15 all become `EPERM`. The unprivileged subcommands (F4 for the caller's own id, F7 `Q_GETINFO`, F8, F9, F11 `Q_XGETQUOTA`, F12) also get `EPERM`: they are not among the decision's Linux-unprivileged exceptions, and answering them as Linux does would hand the guest host quota state. A guest sees a system where quota administration is not permitted; the `quota` and `repquota` tools report the error and exit. The code already does this (`detcore/src/lib.rs:2164-2168`), so no Hermit change is needed. One consequence: `hermit-cli/tests/rr_suite.rs:399` enables rr's `quotactl` program, which the module header (`hermit-cli/tests/rr_suite.rs:38`) says passes under Hermit, but rr's `src/test/quotactl.c` (read from the `third-party/rr` submodule of the local hermit checkout; the submodule pin at f4055eb9cae4 was not checked) first asserts `errno == ENOTBLK || errno == EINVAL`, which the decided `EPERM` fails. By reading the code, not by a run (the test is `#[ignore]`d by its macro, `hermit-cli/tests/rr_suite.rs:233`), that entry in the enabled list is stale.

The code realizes `refuse`. No test names `SYS_quotactl` (`tests_naming: 0`), so verification on the default backend stays untested.

## Recommended tests

- T1 (covers F1, F13, F15): under `hermit run --verify` on ptrace, call raw `syscall(SYS_quotactl, ...)` with `cmd = 0x12345678` and `special` set to `"/"`, a path that does not exist, and `"/dev/null"`; and with `QCMD(Q_GETQUOTA, 7)` (an invalid type). Assert -1 with `errno == EPERM`, the fixed answer owner decision 6 keeps, and that both verify runs print identical output. Native Linux returns `EINVAL`, `ENOENT` or `ENOTBLK` here, so this case also tells the refusal from forwarding.
- T2 (covers F2, F3, F5, F6, F7, F10, F16): with `special` naming the block device of the guest's root filesystem (found through `/proc/self/mountinfo`, as rr's test does), issue `Q_QUOTAON`, `Q_QUOTAOFF`, `Q_GETNEXTQUOTA`, `Q_SETQUOTA`, `Q_SETINFO`, `Q_XQUOTAON`, `Q_XQUOTAOFF` and `Q_XQUOTARM`. Assert `EPERM` for each and identical verify runs.
- T3 (covers F4, F7, F8, F9, F11, F12, F14): issue the unprivileged subcommands `Q_GETQUOTA` and `Q_XGETQUOTA` for the guest's own uid, `Q_GETINFO`, `Q_GETFMT`, `Q_SYNC` (with a device and with `special == NULL`), `Q_XGETQSTAT`, `Q_XGETQSTATV` and `Q_XQUOTASYNC`. Assert `EPERM` for each (owner decision 6), including `Q_SYNC` with `special == NULL`, where native Linux returns 0, and that no output depends on host quota state.
- T4 (covers F1): record a guest that calls `quotactl` under `hermit record`, replay it, and assert the replay returns the same answer at the same point and the run reproduces.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/391: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (code returns a fixed EPERM before reading arguments, detcore/src/lib.rs:2164-2168; host quota state must not reach the guest); linux_determinism stays conditional; 16 flag behaviours and 4 tests listed; left todo with an owner question: EPERM differs from Linux for argument errors and unprivileged subcommands, and the ignored rr_quotactl test (hermit-cli/tests/rr_suite.rs:399) asserts ENOTBLK/EINVAL
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied: the fixed EPERM before any argument check is the target (code already realizes it, detcore/src/lib.rs:2164-2168), including the unprivileged subcommands; target_policy stays refuse; T1 and T3 now assert EPERM; rr_suite's enabled rr_quotactl entry recorded as stale by reading; both open questions removed (refusal answer; rr_quotactl evidence)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `quotactl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+quotactl&body=Discussion+for+the+%60quotactl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fquotactl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60quotactl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: quotactl" (https://github.com/rrnewton/hermit/issues/391), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=quotactl` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `quotactl` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
quotactl(QCMD(0 /* Q_??? */, USRQUOTA), NULL, 0, NULL) = -1 ENODEV (No such device)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
