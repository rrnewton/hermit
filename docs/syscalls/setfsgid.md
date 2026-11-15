---
schema: syscall-pages/v1
syscall: setfsgid
nr_x86_64: 123
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/425
  - https://github.com/rrnewton/hermit/issues/442
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setfsgid&body=Discussion+for+the+%60setfsgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetfsgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setfsgid%60%2C+or+in+what+the+page+says%3F%0A
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

`setfsgid(fsgid)` sets the calling thread's filesystem group ID, the gid Linux uses for filesystem permission checks and for the group of newly created files, and always returns the previous filesystem gid. Hermit never forwards the call. Today it returns 0 for every argument and changes nothing, which is the previous fsgid of a guest that always presents as the fixed virtual root (`getgid` and `getegid` return 0). The target (owner decision 4) is a per-process credential record in Detcore whose filesystem gid `setfsgid` updates under Linux's permission rules and returns, without kernel enforcement. See [setfsgid(2)](https://man7.org/linux/man-pages/man2/setfsgid.2.html).

## Linux semantics

The kernel (`__sys_setfsgid` in `kernel/sys.c`) first records the caller's current fsgid, translated into the caller's user namespace, as the return value. It then maps `fsgid` into that namespace:

- If the mapping fails (the gid is unmapped, including `(gid_t)-1`), nothing changes and the old fsgid is returned.
- Otherwise the change is allowed when `fsgid` equals the caller's real, effective, saved or current filesystem gid, or when the caller has `CAP_SETGID` in its user namespace. An allowed change that actually differs from the current fsgid is installed with `commit_creds`; a disallowed one is silently dropped.

The call has no error return. Success and failure both return the old fsgid, so a caller can only learn whether the change took effect by calling `setfsgid` again (with `-1`, the documented query idiom). The syscall acts on the calling thread only; glibc's `setfsgid()` is a thin wrapper around it.

`commit_creds` resets the process's dumpable flag to `fs.suid_dumpable` and clears the parent-death signal whenever the fsgid changes. The fsgid is also changed implicitly: any later `setgid`, `setregid` or `setresgid` that changes the effective gid sets the fsgid to the new effective gid, and so does `execve` of a set-group-ID binary.

### Flags and options

- F1: `fsgid` maps into the caller's user namespace and equals its real, effective, saved or current fsgid: the fsgid becomes `fsgid` (no change if it already was), and the previous fsgid is returned
- F2: `fsgid` maps and the caller has `CAP_SETGID` in its user namespace: any mapped value is installed, and the previous fsgid is returned
- F3: `fsgid` maps, matches none of the caller's gids, and the caller lacks `CAP_SETGID`: nothing changes, and the previous fsgid is still returned (no errno)
- F4: `fsgid` is unmapped, including `(gid_t)-1`: nothing changes and the current fsgid is returned. `setfsgid(-1)` is the query idiom.
- F5: return value translation: the previous fsgid is reported through the caller's user namespace; a value with no mapping there is reported as the overflow gid (`/proc/sys/kernel/overflowgid`, normally 65534)
- F6: side effects of an actual change: the dumpable flag is reset to `fs.suid_dumpable` (which changes who owns `/proc/<pid>` files and who may `ptrace` the process), and the parent-death signal set by `PR_SET_PDEATHSIG` is cleared
- F7: thread scope: only the calling thread's fsgid changes; other threads keep theirs, and glibc does not broadcast the change
- F8: observers: after a change, the fourth field of the `Gid:` line of `/proc/self/status` reports the new fsgid, new files get it as their group (unless the directory is set-group-ID), and group permission checks for `open` and other pathname operations use it (`access`/`faccessat` without `AT_EACCESS` check the real gid instead)
  `getgid`/`getegid`/`getresgid` do not report the fsgid.
- F9: implicit resets: a later `setgid`/`setregid`/`setresgid` that changes the effective gid, or `execve` of a set-group-ID file, sets the fsgid to the new effective gid

## Determinism analysis

`linux_determinism: conditional`. The result depends only on the caller's current gids, `CAP_SETGID`, the user-namespace gid mapping and `fsgid`. Nothing in the call races, and no host counter or clock is read. It repeats when every run starts with the same credentials, capabilities and mapping.

Both legacy filings classified the call CONDITIONAL and that is kept. https://github.com/rrnewton/hermit/issues/425 stressed that transitions are deterministic given the initial credentials; its duplicate https://github.com/rrnewton/hermit/issues/442 stressed that the returned previous fsgid is the host credential and leaks the host gid unless virtualized. Both are true: the leak is exactly the "initial credentials" input when they are inherited from whoever launched the program.

### Nondeterminism sources

- The previous filesystem gid, which is the return value of every call (F1-F5). On the host it is the launcher's gid: the 2026-07-24 probe returned 100.
- The user-namespace mapping, which decides F4 and the overflow translation in F5.
- `CAP_SETGID` and the caller's real, effective and saved gids, which decide between F1, F2 and F3.
- Filesystem ownership and modes, which decide whether later access checks under the new fsgid succeed (F8).

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04): `setfsgid` updates the per-process credential record described under "Target: the credential record" below. The code at hermit f4055eb9cae4 has no such record. Today `setfsgid` is an argument-blind no-op:

- `setfsgid` is classified `Determinized` at `detcore/src/syscall_classification.rs:655`, inside the batch-38 block whose rationale comment starts at `detcore/src/syscall_classification.rs:630` (tagged `TODO-HUMAN-REVIEW(#787)`).
- The dispatcher answers it before the typed match, in the arm at `detcore/src/lib.rs:2272-2276`: `SyscallClassification::Determinized if is_credential_identity_noop_syscall(call.number()) => { Ok(0) }`. The comment above it (`detcore/src/lib.rs:2269`) states that 0 is the previous fs-id, the virtual 0, for `setfsuid`/`setfsgid`.
- The predicate is defined at `detcore/src/syscall_classification.rs:974`, with `setfsgid` at `detcore/src/syscall_classification.rs:985`. It covers the whole setter family: `setuid`, `setgid`, `setresuid`, `setresgid`, `setreuid`, `setregid`, `setgroups`, `setfsuid` and `setfsgid`.
- `getgid`/`getegid` return a constant 0 (`detcore/src/lib.rs:2762-2763`), so 0 is the fsgid a consistent virtual root would hold. Nothing stores a changed fsgid.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`, `setfsgid` at `detcore/src/syscall_classification.rs:2285`) pins the classification and the predicate membership. It does not start Hermit.

### Reconciling the two legacy filings

- https://github.com/rrnewton/hermit/issues/425 said **PASS-THRU**: "Under Hermit's fixed container identity, forwarding this process-local state is compatible with existing credential passthrough assumptions."
- Its duplicate https://github.com/rrnewton/hermit/issues/442 said **NEEDS-HANDLER**, because the returned previous fsgid is a host credential that Hermit virtualizes.

**https://github.com/rrnewton/hermit/issues/442 won**:

- **The handler it asked for exists.** The arm above answers `setfsgid` in Detcore and never reaches the host, so the host gid cannot leak through the return value.
- **The premise of https://github.com/rrnewton/hermit/issues/425 no longer holds.** The credential getters are no longer passed through; they are emulated as 0. A forwarded `setfsgid` would return the kernel's fsgid, which agrees with the emulated getters only when the backend happens to map the caller's gid to 0. Under ptrace that mapping exists (the comment at `hermit-cli/tests/liteinst_advanced.rs:422` says `Container::map_root` maps the caller's effective GID to zero), but the emulated answer does not depend on it.
- The rendered body's `target_policy: passthrough` came from the legacy PASS-THRU and disagreed with the code; p3 replaced it by `emulate`, and owner decision 4 (2026-10-04) by `model`.

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

**What `setfsgid` must do against the record.**

- Return the record's previous filesystem gid in every case (F5). The call never fails with an errno.
- `(gid_t)-1` changes nothing, so `setfsgid(-1)` returns the current filesystem gid (F4).
- With `CAP_SETGID` in the record's effective set, set the filesystem gid to any other value (F2).
- Without `CAP_SETGID`, set it only if the value equals the record's real, effective, saved or filesystem gid (F1); any other value changes nothing (F3).
- Capabilities do not change: Linux has no filesystem-gid capability fixup.
- Every later change of the record's effective gid through `setgid`, `setregid` or `setresgid` sets the filesystem gid to the new effective gid (F9).

### Differences from Linux

Under the target (documented differences, owner decision 4):

- **No enforcement (F8).** After `setfsgid(1000)` the kernel still checks group permissions and assigns new-file groups with the guest's real credentials, and the fourth field of the `Gid:` line of `/proc/self/status` disagrees with `setfsgid(-1)`.
- **No overflow report (F5).** Every id is valid in the record, so the previous filesystem gid is never reported as 65534.
- **No dumpable or parent-death-signal effects (F6).** The kernel's credentials do not change.
- **Per-process scope (F7).** The filesystem gid is part of the per-process record, so a change on one thread is what every thread's `setfsgid(-1)` returns. On Linux only the calling thread changes, and glibc does not broadcast `setfsgid`, so this differs for the glibc call too.

Today, until the record lands (the no-op at `detcore/src/lib.rs:2272`):

- **Nothing errors, so no errno can differ.** Linux's `setfsgid` never fails, and Hermit's 0 is a valid "previous fsgid".
- **The change is invisible (F1, F2, F8).** After `setfsgid(1000)`, Linux's next `setfsgid(-1)` returns 1000; Hermit's returns 0. A program that verifies an fsgid switch by querying it again (the documented way to detect a silent failure) sees the switch as failed.
- **The query idiom agrees (F4).** `setfsgid(-1)` returns 0 under Hermit, and on Linux it returns the current fsgid, which is 0 for an unchanged virtual root.
- **F3, F7 and F9 cannot be observed.** The guest is always the virtual root with `CAP_SETGID`, and the fsgid never changes, so per-thread divergence and implicit resets have nothing to act on.

## Recommended tests

- T1 (covers F1, F2, F4, F5, F8): under `hermit run --strict --verify` on ptrace:
  - raw `syscall(SYS_setfsgid, -1)` returns 0 (the query idiom; the 2026-07-24 host probe returned 100 here);
  - `syscall(SYS_setfsgid, 0)` returns 0; `syscall(SYS_setfsgid, 1000)` returns 0;
  - a further `syscall(SYS_setfsgid, -1)` then returns 1000, as on Linux (today it returns 0);
  - the fourth field of the `Gid:` line of `/proc/self/status` and the group of a file created afterwards are recorded, listed as documented differences (they show the kernel's credentials), and identical across runs.
- T2 (covers F3): `setresgid(1000, 1000, 1000)` and then `setuid(1000)` leave the record without `CAP_SETGID`. Then `setfsgid(2000)` returns 1000 and `setfsgid(-1)` still returns 1000: the change was silently dropped. Today both return 0.
- T3 (covers F6): set `prctl(PR_SET_PDEATHSIG, SIGTERM)`, call `setfsgid(1000)`, then read `prctl(PR_GET_PDEATHSIG)` and `prctl(PR_GET_DUMPABLE)`. Both stay unchanged, a documented difference, and results are identical across runs.
- T4 (covers F7, F9): in a process with two threads, call raw `SYS_setfsgid(1000)` in one thread; `setfsgid(-1)` then returns 1000 in both threads (the per-process record; on Linux the other thread keeps 0, a documented difference). Then call `setresgid(2000, 2000, 2000)` and query again: both threads get 2000, the new effective gid. Results are identical across runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/425: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_credential_identity_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/442 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged https://github.com/rrnewton/hermit/issues/442 into this issue; target_policy passthrough -> emulate (NEEDS-HANDLER from 442 wins over PASS-THRU from 425: detcore/src/lib.rs:2272-2276 answers setfsgid with Ok(0), the virtual-root previous fsgid, and never forwards it); linux_determinism stays conditional; 9 flag behaviours and 4 tests; the invisible fsgid change and the missing dumpable/pdeath-signal effects are listed as differences from Linux; no test starts hermit for setfsgid, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record; setfsgid returns the previous record fsgid, never an errno); Hermit design target, Differences from Linux and T1-T4 rewritten to assert read-back through setfsgid(-1) (T2: unprivileged change silently dropped; T4: reset by setresgid); no new F/T ids; removed the open question on modelling credentials; no reduced-fidelity line, the result does not depend on the mode

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setfsgid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setfsgid&body=Discussion+for+the+%60setfsgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetfsgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setfsgid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setfsgid" (https://github.com/rrnewton/hermit/issues/425), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/442 was filed the same day with the same title, also legacy variant A1, and was merged into this body section by section on 2026-10-03.

Comment histories:

- https://github.com/rrnewton/hermit/issues/425: 0 comments (local mirror, 2026-10-03).
- https://github.com/rrnewton/hermit/issues/442: 0 comments (local mirror, 2026-10-03).

### Where the merged text went

- Summaries.
  - https://github.com/rrnewton/hermit/issues/425: "Changes the calling process filesystem group ID used for permission checks."
  - https://github.com/rrnewton/hermit/issues/442: "Sets the filesystem group ID used for filesystem permission checks."
  - Both are replaced by the Summary. The fsgid is per thread (F7), not per process as 425 said.
- Linux notes.
  - From https://github.com/rrnewton/hermit/issues/425: "Passing -1 is the documented query idiom because an invalid requested ID leaves the filesystem GID unchanged and returns the current value." This is now F4.
  - From https://github.com/rrnewton/hermit/issues/442: "Counterpart of setfsuid. Verified: setfsgid(-1) (query) returned the host gid (see evidence)." This is F4/F5 and the first step of T1.
- Determinism and nondeterminism sources.
  - From https://github.com/rrnewton/hermit/issues/425: "Credential transitions are deterministic given the initial credentials and capabilities." and "The previous filesystem GID, capability set, user namespace, and filesystem ownership affect returns and later access checks."
  - From https://github.com/rrnewton/hermit/issues/442: "Returns the previous fsgid (host credential); like setfsuid it exposes/relies on host identity that hermit virtualizes." and "The returned previous fsgid is the host credential; leaks host gid unless virtualized."
  - All are kept under Determinism analysis and Nondeterminism sources.
- Recommended action.
  - https://github.com/rrnewton/hermit/issues/425 said **PASS-THRU**; https://github.com/rrnewton/hermit/issues/442 said **NEEDS-HANDLER**.
  - Reconciled under Hermit design: NEEDS-HANDLER won.
- Both filings cited `detcore/src/syscall_classification.rs` at `dd60278f` placing `setfsgid` in the UNCLASSIFIED temporary-passthrough block. That was true on 2026-07-24 and is no longer: at f4055eb9cae4 it is Determinized (see Hermit design).

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/425)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setfsgid`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setfsgid` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setfsgid(-1)                    = 100
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/442)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setfsgid` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `setfsgid` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
setfsgid(-1)                    = 100
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
