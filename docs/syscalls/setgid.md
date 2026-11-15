---
schema: syscall-pages/v1
syscall: setgid
nr_x86_64: 106
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/428
  - https://github.com/rrnewton/hermit/issues/444
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setgid&body=Discussion+for+the+%60setgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setgid%60%2C+or+in+what+the+page+says%3F%0A
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

`setgid(gid)` changes the calling thread's group IDs. A privileged caller sets all of them; an unprivileged caller can only switch its effective gid to its real or saved gid. Hermit never forwards the call. Today it returns 0 for every argument and changes nothing, so the guest always presents as the fixed virtual root (`getgid` and friends return 0). The target (owner decision 4) is a per-process credential record in Detcore that `setgid` updates under Linux's permission rules and the getters read back, without kernel enforcement. See [setgid(2)](https://man7.org/linux/man-pages/man2/setgid.2.html).

## Linux semantics

The kernel (`__sys_setgid`) maps `gid` into the caller's user namespace and then branches on privilege:

- If the caller has `CAP_SETGID` in that namespace, it sets the real, effective, saved and filesystem gids.
- Otherwise it may only set the effective and filesystem gids, and only to the current real or saved gid.

The new credentials are installed with `commit_creds`. When the effective or filesystem gid changes, that resets the process's dumpable flag and clears its parent-death signal. Group changes do not touch capabilities (the capability fixups apply to uid changes only) and do not touch the supplementary group list, which only `setgroups` changes.

The syscall acts on the calling thread only. POSIX requires the change for the whole process, so glibc's `setgid()` wrapper repeats it on every thread through an internal signal. The call returns 0 or a negative errno and reports no other value; the result is seen later through the credential getters, through file group ownership and through permission checks.

### Flags and options

- F1: caller has `CAP_SETGID` in its user namespace: real, effective, saved and filesystem gid all become `gid`
- F2: caller lacks `CAP_SETGID` and `gid` equals its real or saved gid: only the effective and filesystem gids change
- F3: caller lacks `CAP_SETGID` and `gid` is any other value: `EPERM`
- F4: `gid` has no mapping in the caller's user namespace, including `(gid_t)-1`: `EINVAL`
- F5: identity call, `setgid(getgid())`: succeeds and leaves every gid as it was (verified on the host on 2026-07-24)
- F6: side effects: when the effective or filesystem gid changes, the dumpable flag is reset to the `fs.suid_dumpable` sysctl and the parent-death signal (`PR_SET_PDEATHSIG`) is cleared
  Capabilities and the supplementary group list are unchanged.
- F7: thread scope: the raw syscall changes only the calling thread. glibc's `setgid()` applies it to every thread through its internal set-id signal, so the two entry points differ in a multithreaded process.
- F8: observers: after a change, `getgid`/`getegid`/`getresgid` and the `Gid:` line of `/proc/self/status` report the new IDs.
  New files take the new filesystem gid as their group (unless the parent directory is set-group-ID). Group permission checks use the new IDs.

## Determinism analysis

`linux_determinism: conditional`. The result depends only on three inputs:

- the caller's current credentials and capabilities,
- the user-namespace mapping,
- `gid`.

Nothing in the call races and nothing outside the calling process affects it. It repeats when every run starts with the same identity and mapping.

### Nondeterminism sources

- The real and saved gids and the capabilities the guest is started with. They are inherited from whoever launched Hermit; the 2026-07-24 host probe ran with gid 100.
- The user-namespace mapping, which decides F4.
- `CAP_SETGID`, which decides between F1 and F2/F3, and through F6 and F8 which later file operations succeed and who may `ptrace` the process.

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04): `setgid` updates the per-process credential record described under "Target: the credential record" below. The code at hermit f4055eb9cae4 has no such record. Today `setgid` is an argument-blind no-op:

- `setgid` is classified `Determinized` at `detcore/src/syscall_classification.rs:648`. The batch-38 rationale for the whole credential-setting family is at `detcore/src/syscall_classification.rs:636` (tagged `TODO-HUMAN-REVIEW(#787)` at line 630).
- The dispatcher answers it before the typed match, in the arm at `detcore/src/lib.rs:2272`-`2276`: `SyscallClassification::Determinized if is_credential_identity_noop_syscall(call.number()) => { Ok(0) }`. The arm reads no argument and never reaches the host; its comment is at `detcore/src/lib.rs:2262`.
- The predicate is defined at `detcore/src/syscall_classification.rs:974`, with `setgid` at `detcore/src/syscall_classification.rs:978`.
- The getters return a constant 0: `getgid`/`getegid` at `detcore/src/lib.rs:2760`-`2763`, and `getresgid` writes 0 to every result pointer (`detcore/src/syscalls/misc.rs:702`). So the guest always presents as root, whatever it set.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`, membership loop at lines 2276-2297) pins the classification and the predicate membership. It cannot see what the arm returns. The chown family's test file records the same limitation for its own unit tests (`hermit-cli/tests/chown_virtual_root_identity.rs:14`-`17`) and adds a behavioural test for that reason; the credential family has no such test.

### Reconciling the two legacy filings

- https://github.com/rrnewton/hermit/issues/428 said **PASS-THRU**: the call "can remain forwarded under the same container-identity assumptions as getgid".
- Its duplicate https://github.com/rrnewton/hermit/issues/444 said **NEEDS-HANDLER**: "a deterministic run must present virtualized, reproducible credentials".

**https://github.com/rrnewton/hermit/issues/444 won.** The handler it asked for exists (above), and the premise of https://github.com/rrnewton/hermit/issues/428 no longer holds: `getgid` is emulated as 0, not forwarded (`detcore/src/lib.rs:2762`). Forwarding `setgid` next to an emulated `getgid` would split the guest's view from the kernel's. Under the ptrace backend it would also fail for most values: the guest runs in a `CLONE_NEWUSER` namespace that maps the real uid to 0 (comment at `detcore/src/syscall_classification.rs:661`-`663`), and the chown test documents that the default `uid_map` has one entry, so an unmapped id gets `EINVAL` (`hermit-cli/tests/chown_virtual_root_identity.rs:36`-`38`). The gid map was not checked here (`hermit-cli/src` is not in the export).

Both filings agreed on `conditional` and on the inputs listed under Nondeterminism sources, and that agreement is kept.

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

**What `setgid` must do against the record.**

- With `CAP_SETGID` in the record's effective set, set the real, effective, saved and filesystem gid to `gid` (F1, F5). Capabilities and the group list do not change.
- Without `CAP_SETGID`, accept `gid` only if it equals the record's real or saved gid, and then change only the effective and filesystem gid (F2). Any other value returns `EPERM` and leaves the record unchanged (F3).
- `(gid_t)-1` returns `EINVAL` for every caller (F4).
- Afterwards `getgid`, `getegid` and `getresgid` read the record back (F8).

### Differences from Linux

Under the target (documented differences, owner decision 4):

- **No enforcement (F8).** After `setgid(1000)` the kernel still uses the guest's real group: new files get the kernel's group, group permission checks follow the kernel's credentials, and the `Gid:` line of `/proc/self/status` disagrees with `getresgid`.
- **No dumpable or parent-death effects (F6).** The kernel's credentials do not change, so the dumpable flag is not reset and `PR_SET_PDEATHSIG` is not cleared.
- **Per-process scope (F7).** A raw `SYS_setgid` on one thread changes what every thread reads back. On Linux only the calling thread changes, and only glibc's wrapper reaches the others.

Today, until the record lands (the no-op at `detcore/src/lib.rs:2272`):

- **No `EINVAL` (F4).** `setgid((gid_t)-1)` returns 0 under Hermit; Linux returns `EINVAL` for any caller, because `(gid_t)-1` is never a valid gid.
- **The change is invisible (F1, F8).** After `setgid(1000)`, `getgid()` still returns 0.
- **F2 and F3 cannot arise.** The guest never stops being the virtual root, so the unprivileged branches are unreachable.
- rr's `setgid` program is run by `rr_setgid` at `hermit-cli/tests/rr_suite.rs:436`. The rr copy in the working hermit checkout (`third-party/rr/src/test/setgid.c`, not part of the pinned export) only calls `setgid(getgid())` and asserts it returns 0, which Hermit satisfies. The test is `#[ignore]`d by the `rr_test!` macro (`hermit-cli/tests/rr_suite.rs:233`) and listed at `hermit-cli/tests/fail_closed_allowed_ignores.tsv:177`; it was not run here.

## Recommended tests

- T1 (covers F1, F5, F8): under `hermit run --strict --verify` on ptrace, in the container:
  - raw `syscall(SYS_setgid, 0)` and `syscall(SYS_setgid, getgid())` return 0, and `getresgid` still reports 0, 0, 0;
  - then `syscall(SYS_setgid, 1000)` returns 0; `getgid` and `getegid` return 1000, `getresgid` reports 1000, 1000, 1000, and `setfsgid(-1)` returns 1000;
  - the `Gid:` line of `/proc/self/status` and the group of a file created afterwards are recorded, listed as documented differences (they show the kernel's credentials), and identical across runs.

  Existing candidate for F5 only: `rr_setgid` (`hermit-cli/tests/rr_suite.rs:436`), which is `#[ignore]`d.
- T2 (covers F2, F3): `setresgid(1000, 2000, 3000)`, then `setuid(1000)`, which drops every capability from the record. Then `setgid(3000)` returns 0 and `getresgid` reports 1000, 3000, 3000; `setgid(2000)` and `setgid(0)` return `EPERM` and leave the record unchanged. Today all of them return 0 and `getresgid` reports 0, 0, 0.
- T3 (covers F4): `syscall(SYS_setgid, (gid_t)-1)` returns `EINVAL`. Today Hermit returns 0, so this test fails until the model lands.
- T4 (covers F6): set `prctl(PR_SET_PDEATHSIG, SIGUSR1)`, call `setgid(1000)`, then read `PR_GET_PDEATHSIG` and `PR_GET_DUMPABLE`. Both stay unchanged, a documented difference, and the results are identical across runs.
- T5 (covers F7): in a process with two threads, raw `SYS_setgid(1000)` from one thread; then `getgid` returns 1000 in both threads (the per-process record; on Linux the other thread keeps 0, a documented difference). Repeat in a fresh process with glibc `setgid()`, which gives 1000 in both threads on Linux too. Also assert that glibc's set-id signal broadcast does not change the run's schedule between runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/428: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_credential_identity_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/444 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged https://github.com/rrnewton/hermit/issues/444 into this issue; target_policy passthrough -> emulate (NEEDS-HANDLER from 444 wins over PASS-THRU from 428: the arm at detcore/src/lib.rs:2272 answers setgid as a no-op returning 0 and getgid is emulated); linux_determinism stays conditional; 8 flag behaviours, 5 tests; missing EINVAL for (gid_t)-1 and the invisible change listed as differences from Linux
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record, Linux permission rules, no enforcement); Hermit design target, Differences from Linux and T1-T5 rewritten to assert read-back (T2: EPERM once CAP_SETGID is gone); no new F/T ids; removed both open questions (modelling credentials; argument validation, now Linux's rules against the record); no reduced-fidelity line, the result does not depend on the mode

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setgid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setgid&body=Discussion+for+the+%60setgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setgid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setgid" (https://github.com/rrnewton/hermit/issues/428), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/444 was filed the same day with the same title, also legacy variant A1, and was merged into this body section by section on 2026-10-03.

Comment histories:

- https://github.com/rrnewton/hermit/issues/428: the comment history stays on that issue.
- https://github.com/rrnewton/hermit/issues/444: 0 comments.

### Where the merged text went

- Summaries: https://github.com/rrnewton/hermit/issues/428 "Sets the real and effective group identity according to the callers privilege state" and https://github.com/rrnewton/hermit/issues/444 "Sets the real (and, if privileged, effective/saved) group ID of the process". Both are replaced by the Summary and F1/F2. The wording in 444 had the privilege split backwards: a privileged caller sets all IDs, an unprivileged caller only the effective (and filesystem) gid.
- Linux notes: from 428, "An unprivileged process may set its effective GID only to permitted existing IDs; the no-op probe setting the current GID succeeded" is now F2 and F5. From 444, "Setuid-program semantics apply. Verified: setgid(getgid()) (identity no-op) returned 0" is now F5 and a test idea in T1.
- Determinism: from 428, "deterministic Linux rules once the initial identity and capabilities are fixed" and "Initial GID values, saved IDs, user namespace mappings, and CAP_SETGID determine success and the resulting identities"; from 444, "Credential changes depend on the process's host privileges (EPERM otherwise) and operate on identity that hermit virtualizes" and "credential identity should be virtualized". All kept under Determinism analysis.
- Recommended action: 428 PASS-THRU, 444 NEEDS-HANDLER, reconciled under Hermit design.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/428)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setgid`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setgid` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setgid(100)                     = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/444)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setgid` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `setgid` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
setgid(100)                     = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
