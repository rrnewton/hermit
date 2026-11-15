---
schema: syscall-pages/v1
syscall: setgroups
nr_x86_64: 116
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/429
  - https://github.com/rrnewton/hermit/issues/445
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setgroups&body=Discussion+for+the+%60setgroups%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetgroups.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setgroups%60%2C+or+in+what+the+page+says%3F%0A
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

`setgroups(size, list)` replaces the calling thread's supplementary group list, which the kernel uses for group permission checks. Hermit never forwards the call. Today it returns 0 for every argument without reading `list`, which matches the fixed virtual root (a root process may set any list). But `getgroups` is still passed through, so the guest reads back the kernel's real list, not the one it set. The target (owner decision 4) is a per-process credential record in Detcore whose supplementary group list `setgroups` replaces under Linux's permission and argument rules and `getgroups` reads back, without kernel enforcement. See [setgroups(2)](https://man7.org/linux/man-pages/man2/setgroups.2.html).

## Linux semantics

The kernel (`setgroups` in `kernel/groups.c`) checks permission before it looks at the arguments:

- The caller needs `CAP_SETGID` in its user namespace, and the namespace must allow `setgroups`: `/proc/<pid>/setgroups` must read `allow` and the `gid_map` must already be written. Otherwise the call fails with `EPERM`. Both 2026-07-24 host probes, including the one with an invalid size, got `EPERM` for this reason.
- `size` is then compared, as an unsigned value, with `NGROUPS_MAX` (65536). A larger or negative size fails with `EINVAL`.
- The list is copied from user memory (`EFAULT` if it cannot be read), and each gid is mapped into the caller's namespace (`EINVAL` if one has no mapping).
- The kernel sorts the list and installs it with `commit_creds`.

A size of 0 clears the list, and `list` may then be `NULL`. Changing the supplementary groups does not reset the dumpable flag and does not touch capabilities.

The syscall acts on the calling thread only; glibc's `setgroups()` wrapper repeats it on every thread through an internal signal. Privilege-dropping programs call it, typically as `setgroups(0, NULL)`, before `setgid` and `setuid`.

### Flags and options

- F1: privileged caller (F3 and F4 satisfied): the supplementary list becomes the `size` gids at `list`
- F2: `size` 0: the list is cleared; `list` may be `NULL`
- F3: caller lacks `CAP_SETGID` in its user namespace: `EPERM`, checked before any argument
- F4: user namespace whose `/proc/<pid>/setgroups` is `deny`, or whose `gid_map` is not yet written: `EPERM` even with `CAP_SETGID`
- F5: `size` greater than `NGROUPS_MAX` (65536), or negative: `EINVAL`
- F6: `list` not readable for `size` entries: `EFAULT`
- F7: a gid in `list` with no mapping in the caller's namespace, including `(gid_t)-1`: `EINVAL`
- F8: ordering: the kernel stores the list sorted, so `getgroups` returns the gids in ascending order, not in the order passed
- F9: thread scope: the raw syscall changes only the calling thread; glibc's `setgroups()` applies it to every thread through its internal set-id signal
- F10: observers: `getgroups` and the `Groups:` line of `/proc/self/status` report the new list; file permission checks against a file's group use it

## Determinism analysis

`linux_determinism: conditional`. The result depends only on the caller's capabilities, the user namespace's `setgroups` control and gid mapping, and the arguments. Nothing in the call races. It repeats when every run starts with the same identity, namespace configuration and initial group list.

### Nondeterminism sources

- `CAP_SETGID` and the namespace's `setgroups` control, which decide F3/F4 against F1. The 2026-07-24 host probes ran unprivileged and got `EPERM`.
- The gid mapping, which decides F7.
- The initial supplementary list, inherited from whoever launched Hermit. It is what `getgroups` reports until a forwarded `setgroups` replaces it, and it decides group permission checks on files.

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04): `setgroups` updates the per-process credential record described under "Target: the credential record" below. The code at hermit f4055eb9cae4 has no such record. Today `setgroups` is an argument-blind no-op:

- `setgroups` is classified `Determinized` at `detcore/src/syscall_classification.rs:653`. The batch-38 rationale for the whole credential-setting family is at `detcore/src/syscall_classification.rs:636` (tagged `TODO-HUMAN-REVIEW(#787)` at line 630).
- The dispatcher answers it before the typed match, in the arm at `detcore/src/lib.rs:2272`-`2276`: `SyscallClassification::Determinized if is_credential_identity_noop_syscall(call.number()) => { Ok(0) }`. The arm reads neither `size` nor `list` and never reaches the host; its comment is at `detcore/src/lib.rs:2262`.
- The predicate is defined at `detcore/src/syscall_classification.rs:974`, with `setgroups` at `detcore/src/syscall_classification.rs:983`.
- The unit test `batch38_openat2_and_credential_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2265`, membership loop at lines 2276-2297) pins the classification and the predicate membership. It cannot see what the arm returns; the chown family's test file records the same limitation (`hermit-cli/tests/chown_virtual_root_identity.rs:14`-`17`).
- `getgroups` is **not** emulated: it is in the pass-through block at `detcore/src/syscall_classification.rs:718`, under the comment at lines 714-715 that it observes "kernel credential state that starts from the fixed container identity on each run".

### Reconciling the two legacy filings

- https://github.com/rrnewton/hermit/issues/429 said **PASS-THRU**: "Forwarding is acceptable only under Hermit's documented fixed container credentials and filesystem assumptions."
- Its duplicate https://github.com/rrnewton/hermit/issues/445 said **NEEDS-HANDLER**: it "changes credential state that hermit virtualizes".

**https://github.com/rrnewton/hermit/issues/445 won**, because the handler exists and the rest of the identity is emulated: the guest is the virtual root (`getuid`/`getgid` return 0 at `detcore/src/lib.rs:2760`-`2763`). A forwarded `setgroups` would also depend on the namespace's `setgroups` control (F4), which the guest cannot see. Both filings agreed on `conditional`, and 445's point that `EPERM` without `CAP_SETGID` is itself deterministic is kept as F3.

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

**What `setgroups` must do against the record.**

- Without `CAP_SETGID` in the record's effective set, return `EPERM` before looking at any argument (F3).
- A `size` above `NGROUPS_MAX` (65536) as an unsigned value, which includes a negative `size`, returns `EINVAL` (F5).
- Read `size` gids from `list`, returning `EFAULT` if the memory cannot be read (F6). A gid of `(gid_t)-1` returns `EINVAL` (F7).
- Store the list sorted in ascending order (F1, F8). `size` 0 empties it, and `list` may then be `NULL` (F2).
- `getgroups` reads the record back (F10). Its starting value is the empty list, so the KVM test `run_kvm_pipe_pipe2_and_getgroups_round_trip` (`hermit-cli/tests/cli.rs:4445`), which today expects the overflow gid 65534 (`hermit-cli/tests/cli.rs:4494`), must expect `[]` when the model lands.

### Differences from Linux

Under the target (documented differences, owner decision 4):

- **No enforcement (F10).** File permission checks against a file's group still use the kernel's group list, and the `Groups:` line of `/proc/self/status` disagrees with `getgroups`.
- **No user-namespace `setgroups` control (F4).** The record behaves like the initial user namespace: there is no `/proc/<pid>/setgroups` deny and no unwritten `gid_map`, so this `EPERM` cannot arise. For the same reason the only gid with no mapping in F7 is `(gid_t)-1`.
- **Per-process scope (F9).** A raw `SYS_setgroups` on one thread changes what every thread reads back. On Linux only the calling thread changes, and only glibc's wrapper reaches the others.

Today, until the record lands (the no-op at `detcore/src/lib.rs:2272`):

- **The new list is invisible, and the old one leaks (F1, F2, F8, F10).** After `setgroups(0, NULL)`, `getgroups` still returns the kernel's real list, because `getgroups` is passed through (`detcore/src/syscall_classification.rs:718`). That list comes from whoever launched Hermit, so it can differ between hosts and users; under the ptrace backend's user namespace, a host gid without a mapping is reported as the overflow gid.
- rr's `setgroups` program is run by `rr_setgroups` at `hermit-cli/tests/rr_suite.rs:437`. In the rr copy in the working hermit checkout (`third-party/rr/src/test/setgroups.c`, not part of the pinned export), it sets every current group plus one and, unless `setgroups` fails with `EPERM`, asserts that `getgroups` returns the new list. Reading the code, it should fail under Hermit: `setgroups` returns 0 and `getgroups` returns the old list. It is `#[ignore]`d by the `rr_test!` macro (`hermit-cli/tests/rr_suite.rs:233`), listed at `hermit-cli/tests/fail_closed_allowed_ignores.tsv:178`, and was not run here.
- **No argument errors (F5, F6, F7).** `setgroups(-1, NULL)` and `setgroups(65537, list)` return 0 where a root process gets `EINVAL`; a bad `list` pointer returns 0 where Linux returns `EFAULT`; `(gid_t)-1` in the list returns 0 where Linux returns `EINVAL`.
- **F3 and F4 cannot arise.** The guest never stops being the virtual root.

## Recommended tests

- T1 (covers F1, F8, F10): under `hermit run --strict --verify` on ptrace, `setgroups(3, {300, 100, 200})` returns 0; then `getgroups` returns 3 and the list {100, 200, 300}, in ascending order. The `Groups:` line of `/proc/self/status` is recorded and listed as a documented difference (it shows the kernel's list). Results are identical across runs. Today `getgroups` returns the pre-call list.
  Existing candidate: `rr_setgroups` (`hermit-cli/tests/rr_suite.rs:437`), which is `#[ignore]`d and expected to fail until the record lands.
- T2 (covers F2): at start `getgroups(0, NULL)` returns 0 (the empty starting list). `setgroups(1, {5})` and then `setgroups(0, NULL)` both return 0, and `getgroups(0, NULL)` returns 0 again.
- T3 (covers F3, F4): after `setuid(1000)`, which drops `CAP_SETGID` from the record, `setgroups(0, NULL)` returns `EPERM` and `getgroups` reports the list from before the call. The test states that the namespace-control `EPERM` (F4) cannot arise, because the record has no user-namespace control. Today the call returns 0.
- T4 (covers F5, F6, F7): `setgroups(-1, NULL)` and `setgroups(65537, list)` return `EINVAL`, `setgroups(1, (gid_t *)1)` returns `EFAULT`, and `setgroups(1, {(gid_t)-1})` returns `EINVAL`; each leaves `getgroups` unchanged. Today Hermit returns 0 for all four, so this test fails until the model lands.
- T5 (covers F9): in a process with two threads, raw `SYS_setgroups(1, {7})` from one thread; then `getgroups` returns {7} in both threads (the per-process record; on Linux the other thread keeps its list, a documented difference). Repeat in a fresh process with glibc `setgroups()`. Also assert that the set-id signal broadcast does not change the run's schedule between runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/429: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_credential_identity_noop_syscall.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/445 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged https://github.com/rrnewton/hermit/issues/445 into this issue; target_policy passthrough -> emulate (NEEDS-HANDLER from 445 wins over PASS-THRU from 429: the arm at detcore/src/lib.rs:2272 answers setgroups as a no-op returning 0); linux_determinism stays conditional; 10 flag behaviours, 5 tests; getgroups is still passed through so the set list is invisible and the host list leaks, and argument errors are not checked; open questions recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record; supplementary list starts empty, stored sorted, read back by getgroups; Linux EPERM/EINVAL/EFAULT rules against the record); Hermit design target, Differences from Linux and T1-T5 rewritten to assert read-back; the KVM test at hermit-cli/tests/cli.rs:4445 must expect [] once the model lands; no new F/T ids; removed all three open questions (getgroups passthrough, modelling credentials, argument validation); no reduced-fidelity line, the result does not depend on the mode

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `setgroups`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+setgroups&body=Discussion+for+the+%60setgroups%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsetgroups.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60setgroups%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: setgroups" (https://github.com/rrnewton/hermit/issues/429), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/445 was filed the same day with the same title, also legacy variant A1, and was merged into this body section by section on 2026-10-03.

Comment histories:

- https://github.com/rrnewton/hermit/issues/429: the comment history stays on that issue.
- https://github.com/rrnewton/hermit/issues/445: 0 comments.

### Where the merged text went

- Summaries: 429 "Replaces the supplementary group list used for filesystem permission checks" and 445 "Sets the supplementary group list of the process" agree; both are replaced by the Summary.
- Linux notes: from 429, "The kernel requires privilege in the governing user namespace and may also reject writes disabled by the gid_map setgroups control. The safe oversized probe was refused" is now F3, F4 and the note on check order. From 445, "Used during privilege drop. Verified: setgroups(0,NULL) fails EPERM in this container" is in Linux semantics and F3.
- Determinism: from 429, "deterministic for a fixed credential and user-namespace configuration" and "CAP_SETGID, user namespace mappings, the initial group list, and filesystem group ownership affect the observable result"; from 445, "Requires CAP_SETGID (EPERM otherwise, deterministically)" and "mutates credential identity that should be virtualized". All kept under Determinism analysis.
- Recommended action: 429 PASS-THRU, 445 NEEDS-HANDLER, reconciled under Hermit design.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/429)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=setgroups`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `setgroups` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
setgroups(-1, NULL)             = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/445)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=setgroups` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `setgroups` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
setgroups(0, NULL)              = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
