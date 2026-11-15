---
schema: syscall-pages/v1
syscall: setregid
nr_x86_64: 114
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/452
  - https://github.com/rrnewton/hermit/issues/454
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setregid&body=Discussion+for+the+%60setregid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetregid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setregid%60%2C+or+in+what+the+page+says%3F%0A
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

`setregid(rgid, egid)` sets the calling thread's real and effective group IDs; `-1` leaves either one unchanged, and the call may also update the saved gid. Hermit never forwards the call. Today it returns 0 for every argument and changes nothing, so the guest always presents as the fixed virtual root (`getgid` and friends return 0). The target (owner decision 4) is a per-process credential record in Detcore that `setregid` updates under Linux's permission rules and the getters read back, without kernel enforcement. See [setregid(2)](https://man7.org/linux/man-pages/man2/setregid.2.html).

## Linux semantics

The kernel (`__sys_setregid`) maps each argument that is not `-1` into the caller's user namespace, then checks each against the caller's privilege:

- With `CAP_SETGID` in that namespace, any mapped values are accepted.
- Without it, the real gid may be set only to the current real or effective gid, and the effective gid only to the current real, effective or saved gid.

The saved gid is then set to the new effective gid if the real gid was given, or if the effective gid was set to a value other than the previous real gid. The filesystem gid always follows the new effective gid.

The new credentials are installed with `commit_creds`, which resets the dumpable flag and clears the parent-death signal when the effective or filesystem gid changes. Group changes do not touch capabilities or the supplementary group list.

The syscall acts on the calling thread only; glibc's `setregid()` wrapper repeats it on every thread through an internal signal. The call returns 0 or a negative errno and reports no other value.

### Flags and options

- F1: `-1` in either argument leaves that ID unchanged; `setregid(-1, -1)` succeeds and changes nothing (verified on the host on 2026-07-24)
- F2: caller has `CAP_SETGID` in its user namespace: any mapped `rgid` and `egid` are accepted
- F3: caller lacks `CAP_SETGID`: `rgid` must equal the current real or effective gid, otherwise `EPERM`
- F4: caller lacks `CAP_SETGID`: `egid` must equal the current real, effective or saved gid, otherwise `EPERM`
- F5: saved gid: becomes the new effective gid when `rgid` is not `-1`, or when `egid` is set to a value other than the previous real gid; otherwise it is unchanged
- F6: the filesystem gid becomes the new effective gid
- F7: an argument other than `-1` that has no mapping in the caller's user namespace: `EINVAL`
- F8: identity call, `setregid(getgid(), getegid())`: succeeds and leaves every gid as it was (verified on the host on 2026-07-24)
- F9: side effects: when the effective or filesystem gid changes, the dumpable flag is reset to the `fs.suid_dumpable` sysctl and the parent-death signal is cleared; capabilities and supplementary groups are unchanged
- F10: thread scope: the raw syscall changes only the calling thread; glibc's `setregid()` applies it to every thread through its internal set-id signal
- F11: observers: after a change, `getgid`/`getegid`/`getresgid` and the `Gid:` line of `/proc/self/status` report the new IDs; new files take the new filesystem gid as their group (unless the parent directory is set-group-ID); group permission checks use the new IDs

## Determinism analysis

`linux_determinism: conditional`. The result depends only on the caller's current real, effective and saved gids and capabilities, the user-namespace mapping and the two arguments. Nothing in the call races and nothing outside the calling process affects it. It repeats when every run starts with the same identity and mapping.

### Nondeterminism sources

- The real, effective and saved gids and the capabilities the guest is started with, inherited from whoever launched Hermit; the 2026-07-24 host probe ran with gid 100.
- The user-namespace mapping, which decides F7.
- `CAP_SETGID`, which decides between F2 and F3/F4, and through F9 and F11 which later operations succeed.

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04): `setregid` updates the per-process credential record described under "Target: the credential record" below. The code at hermit f4055eb9cae4 has no such record. Today `setregid` is an argument-blind no-op:

- `setregid` is classified `Determinized` at `detcore/src/syscall_classification.rs:652`. The batch-38 rationale for the whole credential-setting family is at `detcore/src/syscall_classification.rs:636` (tagged `TODO-HUMAN-REVIEW(#787)` at line 630).
- The dispatcher answers it before the typed match, in the arm at `detcore/src/lib.rs:2272`-`2276`: `SyscallClassification::Determinized if is_credential_identity_noop_syscall(call.number()) => { Ok(0) }`. The arm reads no argument and never reaches the host; its comment is at `detcore/src/lib.rs:2262`.
- The predicate is defined at `detcore/src/syscall_classification.rs:974`, with `setregid` at `detcore/src/syscall_classification.rs:982`.
- The getters return a constant 0: `getgid`/`getegid` at `detcore/src/lib.rs:2760`-`2763`, and `getresgid` writes 0 to every result pointer (`detcore/src/syscalls/misc.rs:702`). So the guest always presents as root with real, effective and saved gid 0, whatever it set.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`, membership loop at lines 2276-2297) pins the classification and the predicate membership. It cannot see what the arm returns; the chown family's test file records the same limitation (`hermit-cli/tests/chown_virtual_root_identity.rs:14`-`17`). No test names `setregid` at this SHA.

### Reconciling the two legacy filings

- https://github.com/rrnewton/hermit/issues/452 said **PASS-THRU**: the call "can be forwarded under the same fixed container-identity assumptions as other simple credential calls".
- Its duplicate https://github.com/rrnewton/hermit/issues/454 said **NEEDS-HANDLER**: a "credential change gated by host privileges; operates on identity hermit virtualizes".

**https://github.com/rrnewton/hermit/issues/454 won.** The handler exists (above), and the gid getters that 452 implicitly compared against are no longer forwarded: they are emulated as 0 (`detcore/src/lib.rs:2762`-`2763`). Forwarding `setregid` next to them would split the guest's view from the kernel's.

One Linux claim in 452 was imprecise and is corrected here: "An unprivileged process can select only its current real, effective, or saved group IDs" holds for the effective gid only. The real gid may be set only to the current real or effective gid (F3, F4).

Both filings agreed on `conditional` and that saved ids, capabilities, the namespace mapping and the initial credentials decide the outcome; that is kept.

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

**What `setregid` must do against the record.**

- `-1` leaves that id unchanged (F1, F8).
- With `CAP_SETGID` in the record's effective set, accept any valid `rgid` and `egid` (F2).
- Without `CAP_SETGID`, `rgid` must equal the record's real or effective gid (F3), and `egid` its real, effective or saved gid (F4). Otherwise return `EPERM` and leave the record unchanged.
- Set the saved gid to the new effective gid when `rgid` is not `-1` or `egid` differs from the previous real gid (F5), and the filesystem gid to the new effective gid (F6). Capabilities and the group list do not change (F9).
- Afterwards `getgid`, `getegid` and `getresgid` read the record back (F11).

### Differences from Linux

Under the target (documented differences, owner decision 4):

- **No enforcement (F11).** After `setregid(1000, 1000)` the kernel still uses the guest's real group: new files get the kernel's group, group permission checks follow the kernel's credentials, and the `Gid:` line of `/proc/self/status` disagrees with `getresgid`.
- **No `EINVAL` (F7).** Every id other than `-1` is valid in the record, and `-1` means "unchanged" here, so `EINVAL` cannot arise. The no-op today behaves the same way.
- **No dumpable or parent-death effects (F9).** The kernel's credentials do not change.
- **Per-process scope (F10).** A raw `SYS_setregid` on one thread changes what every thread reads back. On Linux only the calling thread changes, and only glibc's wrapper reaches the others.

Today, until the record lands (the no-op at `detcore/src/lib.rs:2272`):

- **F1 and F8 match Linux.** `-1` arguments and identity calls succeed and change nothing, which is exactly what the no-op does.
- **The change is invisible (F2, F5, F6, F11).** After `setregid(1000, 1000)`, `getresgid` still reports 0, 0, 0.
- **F3 and F4 cannot arise.** The guest never stops being the virtual root, so the unprivileged branches are unreachable.

## Recommended tests

- T1 (covers F1, F8): under `hermit run --strict --verify` on ptrace, raw `syscall(SYS_setregid, -1, -1)` and `syscall(SYS_setregid, getgid(), getegid())` return 0 and `getresgid` reports the same values before and after.
- T2 (covers F2, F5, F6, F11): from the starting record:
  - `setregid(-1, 1000)` returns 0; `getresgid` reports 0, 1000, 1000 (the saved gid follows, because the effective gid differs from the real one) and `setfsgid(-1)` returns 1000;
  - `setregid(1000, 2000)` returns 0; `getresgid` reports 1000, 2000, 2000 and `setfsgid(-1)` returns 2000.

  The `Gid:` line of `/proc/self/status` and the group of a newly created file are recorded and listed as documented differences. Today every call returns 0 and `getresgid` reports 0, 0, 0.
- T3 (covers F3, F4, F7): continuing from T2 (1000, 2000, 2000), `setuid(1000)` drops every capability from the record. Then `setregid(0, -1)` and `setregid(-1, 3000)` return `EPERM` and leave the record unchanged; `setregid(2000, -1)` returns 0 (the real gid may become the effective gid) and `getresgid` reports 2000, 2000, 2000. The test states that `EINVAL` (F7) cannot arise because every id other than `-1` is valid in the record.
- T4 (covers F9): set `prctl(PR_SET_PDEATHSIG, SIGUSR1)`, call `setregid(1000, 1000)`, then read `PR_GET_PDEATHSIG`, `PR_GET_DUMPABLE` and `capget`. All stay unchanged (the first two are a documented difference; capabilities are unchanged on Linux too), and results are identical across runs.
- T5 (covers F10): in a process with two threads, raw `SYS_setregid(1000, 1000)` from one thread; then `getresgid` reports 1000, 1000, 1000 in both threads (the per-process record; on Linux the other thread keeps 0, a documented difference). Repeat in a fresh process with glibc `setregid()`. Also assert that the set-id signal broadcast does not change the run's schedule between runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/452: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_credential_identity_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/454 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged https://github.com/rrnewton/hermit/issues/454 into this issue; target_policy passthrough -> emulate (NEEDS-HANDLER from 454 wins over PASS-THRU from 452: the arm at detcore/src/lib.rs:2272 answers setregid as a no-op returning 0 and the gid getters are emulated); linux_determinism stays conditional; 11 flag behaviours, 5 tests; the invisible change listed as a difference from Linux; corrected 452's unprivileged real-gid rule
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record, Linux permission rules, no enforcement); Hermit design target, Differences from Linux and T1-T5 rewritten to assert read-back (T2: saved-gid rule; T3: EPERM once CAP_SETGID is gone); no new F/T ids; removed the open question on modelling credentials; no reduced-fidelity line, the result does not depend on the mode

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setregid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setregid&body=Discussion+for+the+%60setregid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetregid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setregid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setregid" (https://github.com/rrnewton/hermit/issues/452), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/454 was filed the same day with the same title, also legacy variant A1, and was merged into this body section by section on 2026-10-03.

Comment histories:

- https://github.com/rrnewton/hermit/issues/452: the comment history stays on that issue.
- https://github.com/rrnewton/hermit/issues/454: 0 comments.

### Where the merged text went

- Summaries: 452 "Sets the real and effective group IDs of the calling process" and 454 "Sets the real and effective group IDs of the process" agree; both are replaced by the Summary.
- Linux notes: from 452, "An unprivileged process can select only its current real, effective, or saved group IDs. The no-op current-ID probe succeeded" is now F3, F4 (corrected, see Hermit design) and F8. From 454, "-1 leaves a value unchanged. Verified: setregid(-1,-1) (no-op) returned 0" is now F1 and T1.
- Determinism: from 452, "follows deterministic rules for fixed initial IDs and capabilities" and "Saved IDs, capability state, user namespace mappings, and initial credentials control which transitions are permitted"; from 454, "Credential change gated by host privileges" and "credential identity should be virtualized". All kept under Determinism analysis.
- Recommended action: 452 PASS-THRU, 454 NEEDS-HANDLER, reconciled under Hermit design.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/452)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setregid`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setregid` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setregid(100, 100)              = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/454)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setregid` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `setregid` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
setregid(-1, -1)                = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
