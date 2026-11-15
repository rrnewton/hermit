---
schema: syscall-pages/v1
syscall: setresgid
nr_x86_64: 119
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/453
  - https://github.com/rrnewton/hermit/issues/458
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setresgid&body=Discussion+for+the+%60setresgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetresgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setresgid%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_credential_identity_noop_syscall"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`setresgid(rgid, egid, sgid)` sets the calling thread's real, effective and saved group IDs; `-1` leaves any of them unchanged. Hermit never forwards the call. Today it returns 0 for every argument and changes nothing, so the guest always presents as the fixed virtual root (`getresgid` reports 0, 0, 0). The target (owner decision 4) is a per-process credential record in Detcore that `setresgid` updates under Linux's permission rules and `getresgid` reads back, without kernel enforcement. See [setresgid(2)](https://man7.org/linux/man-pages/man2/setresgid.2.html).

## Linux semantics

The kernel (`__sys_setresgid`) maps each argument that is not `-1` into the caller's user namespace (`EINVAL` if one has no mapping), then checks privilege:

- With `CAP_SETGID` in that namespace, any mapped values are accepted.
- Without it, each argument that is not `-1` must equal one of the current real, effective or saved gids; otherwise the call fails with `EPERM`.

The filesystem gid always follows the new effective gid. The new credentials are installed with `commit_creds`, which resets the dumpable flag and clears the parent-death signal when the effective or filesystem gid changes. Group changes do not touch capabilities or the supplementary group list.

The syscall acts on the calling thread only; glibc's `setresgid()` wrapper repeats it on every thread through an internal signal. The call returns 0 or a negative errno and reports no other value.

### Flags and options

- F1: `-1` in any argument leaves that ID unchanged; `setresgid(-1, -1, -1)` succeeds and changes nothing (verified on the host on 2026-07-24)
- F2: caller has `CAP_SETGID` in its user namespace: any mapped values are set
- F3: caller lacks `CAP_SETGID`: each argument that is not `-1` must equal the current real, effective or saved gid, otherwise `EPERM`
- F4: an argument other than `-1` with no mapping in the caller's user namespace: `EINVAL`
- F5: the filesystem gid becomes the new effective gid
- F6: identity call, `setresgid(r, e, s)` with the current values: succeeds and changes nothing (verified on the host on 2026-07-24)
- F7: side effects: when the effective or filesystem gid changes, the dumpable flag is reset to the `fs.suid_dumpable` sysctl and the parent-death signal is cleared; capabilities and supplementary groups are unchanged
- F8: thread scope: the raw syscall changes only the calling thread; glibc's `setresgid()` applies it to every thread through its internal set-id signal
- F9: observers: after a change, `getgid`/`getegid`/`getresgid` and the `Gid:` line of `/proc/self/status` report the new IDs; new files take the new filesystem gid as their group (unless the parent directory is set-group-ID); group permission checks use the new IDs

## Determinism analysis

`linux_determinism: conditional`. The result depends only on the caller's current gids and capabilities, the user-namespace mapping and the three arguments. Nothing in the call races and nothing outside the calling process affects it. It repeats when every run starts with the same identity and mapping.

### Nondeterminism sources

- The real, effective and saved gids and the capabilities the guest is started with, inherited from whoever launched Hermit; the 2026-07-24 host probe ran with gid 100.
- The user-namespace mapping, which decides F4.
- `CAP_SETGID`, which decides between F2 and F3, and through F7 and F9 which later operations succeed.

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04): `setresgid` updates the per-process credential record described under "Target: the credential record" below. The code at hermit f4055eb9cae4 has no such record. Today `setresgid` is an argument-blind no-op:

- `setresgid` is classified `Determinized` at `detcore/src/syscall_classification.rs:650`. The batch-38 rationale for the whole credential-setting family is at `detcore/src/syscall_classification.rs:636` (tagged `TODO-HUMAN-REVIEW(#787)` at line 630).
- The dispatcher answers it before the typed match, in the arm at `detcore/src/lib.rs:2272`-`2276`: `SyscallClassification::Determinized if is_credential_identity_noop_syscall(call.number()) => { Ok(0) }`. The arm reads no argument and never reaches the host; its comment is at `detcore/src/lib.rs:2262`.
- The predicate is defined at `detcore/src/syscall_classification.rs:974`, with `setresgid` at `detcore/src/syscall_classification.rs:980`.
- The matching getter `getresgid` writes 0 to every result pointer (`detcore/src/syscalls/misc.rs:702`-`717`), and `getgid`/`getegid` return 0 (`detcore/src/lib.rs:2760`-`2763`). So the guest always presents as root, whatever it set.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`, membership loop at lines 2276-2297) pins the classification and the predicate membership. It cannot see what the arm returns; the chown family's test file records the same limitation (`hermit-cli/tests/chown_virtual_root_identity.rs:14`-`17`). No test names `setresgid` at this SHA.

### Reconciling the two legacy filings

- https://github.com/rrnewton/hermit/issues/453 said **PASS-THRU**: the call "can be forwarded under Hermit's fixed container-identity assumptions".
- Its duplicate https://github.com/rrnewton/hermit/issues/458 said **NEEDS-HANDLER**: "Same as setregid with three IDs; privilege-gated; identity virtualized by hermit."

**https://github.com/rrnewton/hermit/issues/458 won.** The handler exists (above), and `getresgid`, which reports exactly the three IDs this call sets, is emulated rather than forwarded. Forwarding `setresgid` next to it would split the guest's view from the kernel's.

Both filings agreed on `conditional` and that saved ids, capabilities, the namespace mapping and the initial group identity decide the outcome; that is kept.

### Target: the credential record (owner decision 4)

The owner accepted this target on 2026-10-04. Every credential body (the set*id family, `setgroups`, `capset`, the get*id family, `getgroups`, `capget`) describes it the same way:

- **Record.** Detcore keeps, per guest process, the real, effective, saved and filesystem uid and gid, the supplementary group list and the capability sets.
- **Calls.** The set*id calls, `setgroups` and `capset` update the record. The get*id calls, `getgroups` and `capget` report it.
- **Starting state.** Every id is 0, as `getuid` reports today, and the group list is empty. The capability sets start as the guest holds them at launch in the ptrace container, as uid 0 in a fresh user namespace: permitted and effective full, inheritable empty.
- **Inheritance.** fork and clone copy the record; execve keeps it.
- **Permission rules.** Each setter applies Linux's own rules against the record, never against host state, so the checks are deterministic. With the matching capability (`CAP_SETUID`, `CAP_SETGID`) in the record's effective set, a caller may set any valid id; without it, only the ids Linux allows an unprivileged caller. Linux's capability fixups for uid and filesystem-uid changes apply to the record. So once the guest has dropped to uid 1000 with no `CAP_SETUID` left, `setuid(0)` returns `EPERM`, which is what OpenSSH's `permanently_set_uid` checks.
- **Valid ids.** Every id except `(uid_t)-1` and `(gid_t)-1` is valid in the record, as in the initial user namespace. Where `-1` does not mean "unchanged", Linux rejects it: `EINVAL`, or for `setfsuid`/`setfsgid` no change.
- **No enforcement.** The kernel goes on checking the real credentials: the guest stays root in the container's user namespace. After `setuid(1000)` the guest can still open root-only files, and new files keep the kernel's owner and group. That is the accepted cost.
- **Not covered.** The `Uid:`, `Gid:`, `Groups:` and `Cap*:` lines of `/proc/<pid>/status` still show the kernel's credentials and disagree with the record; that stays open for `/proc` virtualization. Effects outside the record (the dumpable flag, the parent-death signal, `RLIMIT_NPROC` accounting) do not follow a change.
- **Thread scope.** Linux credentials are per thread in the kernel, and glibc's set*id and `setgroups` wrappers broadcast a change to every thread through an internal signal. The record is per process (owner-approved), so a raw syscall on one thread of a multithreaded guest changes the credentials every thread reads back. That is a documented difference. `setfsuid`, `setfsgid` and `capset` have no glibc broadcast, so for them the glibc call differs from Linux too.

**What `setresgid` must do against the record.**

- `-1` leaves that id unchanged (F1, F6).
- With `CAP_SETGID` in the record's effective set, set any valid values (F2).
- Without `CAP_SETGID`, each argument that is not `-1` must equal the record's current real, effective or saved gid; otherwise return `EPERM` and leave the record unchanged (F3).
- Set the filesystem gid to the new effective gid (F5). Capabilities and the group list do not change (F7).
- Afterwards `getresgid`, `getgid` and `getegid` read the record back (F9).

### Differences from Linux

Under the target (documented differences, owner decision 4):

- **No enforcement (F9).** After `setresgid(1000, 1000, 1000)` the kernel still uses the guest's real group: new files get the kernel's group, group permission checks follow the kernel's credentials, and the `Gid:` line of `/proc/self/status` disagrees with `getresgid`.
- **No `EINVAL` (F4).** Every id other than `-1` is valid in the record, and `-1` means "unchanged" here, so `EINVAL` cannot arise. The no-op today behaves the same way.
- **No dumpable or parent-death effects (F7).** The kernel's credentials do not change.
- **Per-process scope (F8).** A raw `SYS_setresgid` on one thread changes what every thread reads back. On Linux only the calling thread changes, and only glibc's wrapper reaches the others.

Today, until the record lands (the no-op at `detcore/src/lib.rs:2272`):

- **F1 and F6 match Linux.** `-1` arguments and identity calls succeed and change nothing, which is exactly what the no-op does.
- **The change is invisible (F2, F5, F9).** After `setresgid(1000, 1000, 1000)`, `getresgid` still reports 0, 0, 0 (`detcore/src/syscalls/misc.rs:707`-`715`).
- **F3 cannot arise.** The guest never stops being the virtual root.

## Recommended tests

- T1 (covers F1, F6): under `hermit run --strict --verify` on ptrace, raw `syscall(SYS_setresgid, -1, -1, -1)` and `setresgid` with the values `getresgid` just reported both return 0, and `getresgid` reports the same values before and after.
- T2 (covers F2, F5, F9): `syscall(SYS_setresgid, 1000, 2000, 3000)` returns 0; then `getresgid` reports 1000, 2000, 3000 and `setfsgid(-1)` returns 2000. The `Gid:` line of `/proc/self/status` and the group of a newly created file are recorded and listed as documented differences. Today `getresgid` reports 0, 0, 0.
- T3 (covers F3, F4): continuing from T2, `setuid(1000)` drops every capability from the record. Then `setresgid(3000, 1000, 2000)` returns 0 and `getresgid` reports 3000, 1000, 2000; `setresgid(0, 0, 0)` and `setresgid(4000, -1, -1)` return `EPERM` and leave the record unchanged. The test states that `EINVAL` (F4) cannot arise because every id other than `-1` is valid in the record. Today every call returns 0.
- T4 (covers F7): set `prctl(PR_SET_PDEATHSIG, SIGUSR1)`, call `setresgid(1000, 1000, 1000)`, then read `PR_GET_PDEATHSIG`, `PR_GET_DUMPABLE` and `capget`. All stay unchanged (the first two are a documented difference; capabilities are unchanged on Linux too), and results are identical across runs.
- T5 (covers F8): in a process with two threads, raw `SYS_setresgid(1000, 1000, 1000)` from one thread; then `getresgid` reports 1000, 1000, 1000 in both threads (the per-process record; on Linux the other thread keeps 0, a documented difference). Repeat in a fresh process with glibc `setresgid()`. Also assert that the set-id signal broadcast does not change the run's schedule between runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/453: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_credential_identity_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/458 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged https://github.com/rrnewton/hermit/issues/458 into this issue; target_policy passthrough -> emulate (NEEDS-HANDLER from 458 wins over PASS-THRU from 453: the arm at detcore/src/lib.rs:2272 answers setresgid as a no-op returning 0 and getresgid is emulated to 0,0,0); linux_determinism stays conditional; 9 flag behaviours, 5 tests; the invisible change listed as a difference from Linux
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record, Linux permission rules, no enforcement); Hermit design target, Differences from Linux and T1-T5 rewritten to assert read-back (T2: setresgid(1000,2000,3000) reads back 1000,2000,3000; T3: EPERM once CAP_SETGID is gone); no new F/T ids; removed the open question on modelling credentials; no reduced-fidelity line, the result does not depend on the mode

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setresgid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setresgid&body=Discussion+for+the+%60setresgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetresgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setresgid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setresgid" (https://github.com/rrnewton/hermit/issues/453), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/458 was filed the same day with the same title, also legacy variant A1, and was merged into this body section by section on 2026-10-03.

Comment histories:

- https://github.com/rrnewton/hermit/issues/453: the comment history stays on that issue.
- https://github.com/rrnewton/hermit/issues/458: 0 comments.

### Where the merged text went

- Summaries: 453 "Sets the real, effective, and saved group IDs of the calling process" and 458 "Sets the real, effective, and saved group IDs of the process" agree; both are replaced by the Summary.
- Linux notes: from 453, "The syscall updates any subset whose argument is not -1. The no-op probe supplied the current IDs and succeeded" is now F1 and F6. From 458, "-1 leaves a value unchanged. Verified: setresgid(-1,-1,-1) returned 0" is now F1 and T1.
- Determinism: from 453, "deterministic for fixed initial credentials and capabilities" and "Saved IDs, capabilities, user namespace mappings, and initial group identity affect success and later permission checks"; from 458, "Success depends on host privileges; credential identity should be virtualized". All kept under Determinism analysis.
- Recommended action: 453 PASS-THRU, 458 NEEDS-HANDLER, reconciled under Hermit design.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/453)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setresgid`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setresgid` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setresgid(100, 100, 100)        = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/458)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setresgid` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `setresgid` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
setresgid(-1, -1, -1)           = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
