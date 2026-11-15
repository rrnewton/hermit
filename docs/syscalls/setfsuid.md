---
schema: syscall-pages/v1
syscall: setfsuid
nr_x86_64: 122
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/426
  - https://github.com/rrnewton/hermit/issues/443
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setfsuid&body=Discussion+for+the+%60setfsuid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetfsuid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setfsuid%60%2C+or+in+what+the+page+says%3F%0A
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

`setfsuid(fsuid)` sets the calling thread's filesystem user ID, which the kernel uses instead of the effective uid for filesystem permission checks and new-file ownership. It always returns the previous filesystem uid and never reports an error. Hermit never forwards the call. Today it returns 0 for every argument and changes nothing. 0 is the previous filesystem uid of the fixed virtual root, so `setfsuid(-1)` no longer leaks the host uid. The target (owner decision 4) is a per-process credential record in Detcore whose filesystem uid `setfsuid` updates under Linux's permission rules and returns, without kernel enforcement. See [setfsuid(2)](https://man7.org/linux/man-pages/man2/setfsuid.2.html).

## Linux semantics

The kernel (`__sys_setfsuid`) first records the current filesystem uid, as seen from the caller's user namespace, as the return value. Then:

- If the argument has no mapping in the caller's namespace (`-1` never has one), nothing changes.
- Otherwise the change is allowed if the caller has `CAP_SETUID` in that namespace, or if the value equals the current real, effective, saved or filesystem uid. If the change is not allowed, nothing changes.
- In every case the call returns the previous filesystem uid. The caller cannot tell from the return value whether the change happened; the idiom is to call `setfsuid(-1)` afterwards, which changes nothing and returns the current value.

A filesystem uid change is installed with `commit_creds`, which resets the dumpable flag and clears the parent-death signal. The capability fixup for filesystem-uid changes moves the filesystem-related capabilities (`CAP_CHOWN`, `CAP_DAC_OVERRIDE`, `CAP_DAC_READ_SEARCH`, `CAP_FOWNER`, `CAP_FSETID` and others in `CAP_FS_MASK`) out of the effective set when the filesystem uid leaves 0, and back from the permitted set when it returns to 0, unless `SECBIT_NO_SETUID_FIXUP` is set.

Every later change of the effective uid (`setuid`, `setreuid`, `setresuid`, `execve` of a set-user-ID file) resets the filesystem uid to the new effective uid. The call acts on the calling thread only, and unlike the other set-id calls, glibc's `setfsuid()` wrapper does not repeat it on other threads.

### Flags and options

- F1: caller has `CAP_SETUID` in its user namespace and passes a mapped uid: the filesystem uid becomes that value; the return is the previous filesystem uid
- F2: caller lacks `CAP_SETUID` and passes a value equal to its current real, effective, saved or filesystem uid: the filesystem uid becomes that value
- F3: caller lacks `CAP_SETUID` and passes any other mapped value: nothing changes; the return is the current filesystem uid and no errno is set
- F4: an argument with no mapping in the caller's user namespace, including `-1`: nothing changes; `setfsuid(-1)` is the query idiom (the host probe on 2026-07-24 returned 212630, the caller's uid)
- F5: return value: always the previous filesystem uid as seen from the caller's namespace; a filesystem uid with no mapping there is reported as the overflow uid (65534 by default); the call never returns `-1` with an errno
- F6: capabilities: a change of the filesystem uid from 0 to nonzero drops the `CAP_FS_MASK` capabilities from the effective set; a change back to 0 restores them from the permitted set; `SECBIT_NO_SETUID_FIXUP` suppresses both
- F7: side effects: a change of the filesystem uid resets the dumpable flag to the `fs.suid_dumpable` sysctl and clears the parent-death signal
- F8: implicit reset: every later change of the effective uid sets the filesystem uid to the new effective uid
- F9: thread scope: the change applies to the calling thread only; glibc does not broadcast it to other threads
- F10: observers: the owner of newly created files, filesystem permission checks, and the fourth field of the `Uid:` line in `/proc/self/status`

## Determinism analysis

`linux_determinism: conditional`. The result depends only on the caller's current uids and capabilities, the user-namespace mapping and the argument. Nothing in the call races. It repeats when every run starts with the same identity and mapping. But with the identity passed through, the return value of the common query `setfsuid(-1)` is the host uid of whoever launched the program, so it differs between users and hosts.

### Nondeterminism sources

- The previous filesystem uid, which is the return value (F4, F5). Without virtualization it is the launching user's uid; the 2026-07-24 host probe returned 212630.
- `CAP_SETUID` and the current real, effective and saved uids, which decide F1/F2 against F3.
- The user-namespace mapping, which decides F4 and the overflow-uid report in F5.
- Ownership of the files the guest later creates or opens, which the filesystem uid decides (F10).

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04): `setfsuid` updates the per-process credential record described under "Target: the credential record" below. The code at hermit f4055eb9cae4 has no such record. Today `setfsuid` is an argument-blind no-op:

- `setfsuid` is classified `Determinized` at `detcore/src/syscall_classification.rs:654`. The batch-38 rationale for the whole credential-setting family is at `detcore/src/syscall_classification.rs:636` (tagged `TODO-HUMAN-REVIEW(#787)` at line 630).
- The dispatcher answers it before the typed match, in the arm at `detcore/src/lib.rs:2272`-`2276`: `SyscallClassification::Determinized if is_credential_identity_noop_syscall(call.number()) => { Ok(0) }`. The arm reads no argument and never reaches the host.
- The arm's comment states the intent for this call at `detcore/src/lib.rs:2268`-`2269`: the returned 0 is "the previous fs-id, virtual 0, for setfsuid/setfsgid". It matches `geteuid`, which returns 0 (`detcore/src/lib.rs:2761`), since on Linux the filesystem uid follows the effective uid (F8).
- The predicate is defined at `detcore/src/syscall_classification.rs:974`, with `setfsuid` at `detcore/src/syscall_classification.rs:984`.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`, membership loop at lines 2276-2297) pins the classification and the predicate membership. It cannot see what the arm returns; the chown family's test file records the same limitation (`hermit-cli/tests/chown_virtual_root_identity.rs:14`-`17`). No test names `setfsuid` at this SHA.

### Reconciling the two legacy filings

- https://github.com/rrnewton/hermit/issues/426 said **PASS-THRU**: "forwarding this process-local state is compatible with existing credential passthrough assumptions".
- Its duplicate https://github.com/rrnewton/hermit/issues/443 said **NEEDS-HANDLER**: the returned previous filesystem uid "is host-dependent and hermit already virtualizes process identity (getuid etc.), so the returned/observed credential should be virtualized too".

**https://github.com/rrnewton/hermit/issues/443 won.** Its argument is the decisive one for this call: the return value itself carries the host uid, which a forwarded `setfsuid(-1)` would leak (212630 in the probe) while `geteuid` reports 0. The handler that 443 asked for exists, and the leak it raised is resolved: the guest now gets 0. Both filings agreed on `conditional`; that is kept.

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

**What `setfsuid` must do against the record.**

- Return the record's previous filesystem uid in every case (F5). The call never fails with an errno.
- `(uid_t)-1` changes nothing, so `setfsuid(-1)` returns the current filesystem uid (F4).
- With `CAP_SETUID` in the record's effective set, set the filesystem uid to any other value (F1).
- Without `CAP_SETUID`, set it only if the value equals the record's real, effective, saved or filesystem uid (F2); any other value changes nothing (F3).
- Apply the filesystem-uid capability fixup (F6): leaving filesystem uid 0 removes the `CAP_FS_MASK` capabilities from the record's effective set, and returning to 0 restores them from the permitted set, unless the guest has set `SECBIT_NO_SETUID_FIXUP` (prctl state the model must consult).
- Every later change of the record's effective uid through `setuid`, `setreuid` or `setresuid` sets the filesystem uid to the new effective uid (F8).

### Differences from Linux

Under the target (documented differences, owner decision 4):

- **No enforcement (F10).** After `setfsuid(1000)` the kernel still checks file permissions and assigns new-file ownership with the guest's real credentials, and the fourth field of the `Uid:` line of `/proc/self/status` disagrees with `setfsuid(-1)`.
- **No overflow report (F5).** Every id is valid in the record, so the previous filesystem uid is never reported as 65534.
- **No dumpable or parent-death effects (F7).** The kernel's credentials do not change.
- **Per-process scope (F9).** The filesystem uid is part of the per-process record, so a change on one thread is what every thread's `setfsuid(-1)` returns. On Linux only the calling thread changes, and glibc does not broadcast `setfsuid`, so this differs for the glibc call too.

Today, until the record lands (the no-op at `detcore/src/lib.rs:2272`):

- **F4 and F5 match a root process with filesystem uid 0.** `setfsuid(-1)` returns 0, which is what Linux returns to a process in the ptrace backend's namespace that maps the real uid to 0 (comment at `detcore/src/syscall_classification.rs:661`-`663`).
- **The change is invisible (F1, F2, F10).** After `setfsuid(1000)`, `setfsuid(-1)` returns 0 under Hermit, where Linux returns 1000.
- **No capability effects (F6).** `capget` is passed through (`detcore/src/syscall_classification.rs:716`) and reports the host capability sets.
- **F3 cannot arise, and F8 and F9 are moot.** The guest never stops being the virtual root, and since no filesystem uid is stored, there is nothing to reset or to share between threads.

## Recommended tests

- T1 (covers F4, F5): under `hermit run --strict --verify` on ptrace, raw `syscall(SYS_setfsuid, -1)` returns 0 in every run, and a second call returns 0 again. This is the leak 443 raised, so the test also asserts the result is not the host uid.
- T2 (covers F1, F2, F10): `syscall(SYS_setfsuid, 1000)` returns 0 and `setfsuid(-1)` then returns 1000; `setfsuid(0)` returns 1000 and `setfsuid(-1)` returns 0. The fourth field of the `Uid:` line of `/proc/self/status` and the owner of a newly created file are recorded and listed as documented differences (they show the kernel's credentials). Today the second query returns 0.
- T3 (covers F3): after `setresuid(1000, 1000, 1000)` (which leaves the record without `CAP_SETUID`), `setfsuid(2000)` returns 1000 and `setfsuid(-1)` still returns 1000. Today both return 0.
- T4 (covers F6, F7): from the starting record, `setfsuid(1000)` removes the `CAP_FS_MASK` capabilities (`CAP_CHOWN`, `CAP_DAC_OVERRIDE` and the rest) from the effective set `capget` reports, and `setfsuid(0)` restores them. Set `prctl(PR_SET_PDEATHSIG, SIGUSR1)` before the change and read `PR_GET_PDEATHSIG` and `PR_GET_DUMPABLE` after it; both stay unchanged, a documented difference. Results are identical across runs.
- T5 (covers F8): `setfsuid(1000)`, then `setresuid(-1, 2000, -1)`, then `setfsuid(-1)` returns 2000, as on Linux. Today it returns 0.
- T6 (covers F9): in a process with two threads, one thread calls `setfsuid(1000)`; then `setfsuid(-1)` returns 1000 in the other thread too (the per-process record; on Linux the other thread keeps 0, a documented difference). Results are identical across runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/426: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_credential_identity_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/443 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged https://github.com/rrnewton/hermit/issues/443 into this issue; target_policy passthrough -> emulate (NEEDS-HANDLER from 443 wins over PASS-THRU from 426: the arm at detcore/src/lib.rs:2272 answers setfsuid with 0, the virtual root's previous fs-id, so the host-uid leak 443 raised is resolved); linux_determinism stays conditional; 10 flag behaviours, 6 tests; the invisible change listed as a difference from Linux
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record; setfsuid returns the previous record fsuid, never an errno); Hermit design target, Differences from Linux and T1-T6 rewritten to assert read-back through setfsuid(-1) (T3: unprivileged change silently refused; T4: CAP_FS_MASK fixup on the record); no new F/T ids; removed the open question on modelling credentials; no reduced-fidelity line, the result does not depend on the mode

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setfsuid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setfsuid&body=Discussion+for+the+%60setfsuid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetfsuid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setfsuid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setfsuid" (https://github.com/rrnewton/hermit/issues/426), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/443 was filed the same day with the same title, also legacy variant A1, and was merged into this body section by section on 2026-10-03.

Comment histories:

- https://github.com/rrnewton/hermit/issues/426: the comment history stays on that issue.
- https://github.com/rrnewton/hermit/issues/443: 0 comments.

### Where the merged text went

- Summaries: 426 "Changes the calling process filesystem user ID used for permission checks" and 443 "Sets the filesystem user ID used for filesystem permission checks" agree; both are replaced by the Summary, which also corrects the scope to the calling thread (F9).
- Linux notes: from 426, "Passing -1 queries the current filesystem UID without changing it. The probe returned the host/container UID" is now F4. From 443, "Historically used by NFS servers. Verified: setfsuid(-1) (query) returned the host uid" is in F4 and the Determinism analysis.
- Determinism: from 426, "Credential transitions are deterministic given the initial credentials and capabilities" and "The previous filesystem UID, capability set, user namespace, and filesystem ownership affect returns and later access checks"; from 443, "The returned previous fsuid is the host credential; leaks host uid unless virtualized". All kept under Determinism analysis.
- Recommended action: 426 PASS-THRU, 443 NEEDS-HANDLER, reconciled under Hermit design.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/426)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setfsuid`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setfsuid` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setfsuid(-1)                    = 212630
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/443)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setfsuid` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `setfsuid` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
setfsuid(-1)                    = 212630
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
