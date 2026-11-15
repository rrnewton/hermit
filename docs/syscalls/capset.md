---
schema: syscall-pages/v1
syscall: capset
nr_x86_64: 126
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1938
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+capset&body=Discussion+for+the+%60capset%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcapset.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60capset%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 11
---

## Summary

`capset(header, data)` replaces the calling thread's effective, permitted and inheritable capability sets, within limits the kernel enforces. Today Hermit forwards it to the host kernel. On the ptrace backend that changes only the guest thread's own credentials inside its user namespace, and the kernel then enforces the change on the guest's later syscalls. The target (owner decision 4) is `model`: `capset` updates the capability sets of a per-process credential record in Detcore under Linux's rules, and `capget` reads them back. The record is not enforced, so a capability dropped only in the record is still held in the kernel; that is the accepted cost, and whether accepted requests should also be forwarded is left as an open question (under Additional information).

Manual reference: [capset(2)](https://man7.org/linux/man-pages/man2/capset.2.html).

## Linux semantics

The kernel (`kernel/capability.c`) validates `header->version` exactly as `capget` does: version 3 (`0x20080522`) and the deprecated version 2 (`0x20071026`) take two 32-bit data elements, version 1 (`0x19980330`) takes one, and any other version is rewritten to 3 in the header and fails with `EINVAL`. Versions 1 and 2 also log a one-time kernel warning. `header->pid` must be 0 or the caller's own thread id; anything else, including another thread of the same process, gives `EPERM`. The data is copied in (`EFAULT` on a fault) and each set is masked to the capabilities the running kernel defines, so unknown high bits are dropped silently; with version 1 the upper 32 bits are zero, so capabilities 32 and above are cleared from all three sets. The LSM hook `security_capset` then runs; the default `cap_capset` (`security/commoncap.c`) refuses with `EPERM` when the new inheritable set adds bits outside the old inheritable and permitted sets and the caller lacks `CAP_SETPCAP` in its effective set, when it adds bits outside the old inheritable set and the bounding set, when the new permitted set is not a subset of the old one, or when the new effective set is not a subset of the new permitted set. On success the ambient set is reduced to bits still in both the new permitted and inheritable sets, and the new credentials apply to the calling thread only. Threads and children created afterwards inherit them; `execve` recomputes them, and for uid 0 without `SECBIT_NOROOT` the permitted set is refilled from the bounding set, so a capability dropped only with `capset` comes back after an exec.

### Flags and options

- F1: version 3 (`_LINUX_CAPABILITY_VERSION_3`): two data elements carrying 64 bits per set.
- F2: version 2 (`_LINUX_CAPABILITY_VERSION_2`): treated as version 3; the kernel logs a deprecation warning.
- F3: version 1 (`_LINUX_CAPABILITY_VERSION_1`): one data element; capabilities 32 and above are cleared from all three sets; the kernel logs a legacy-use warning.
- F4: unknown version: `EINVAL`, and `header->version` is rewritten to version 3.
- F5: `pid` 0 or the caller's own thread id: the change applies to the calling thread.
- F6: any other `pid`, including another thread of the same process: `EPERM`.
- F7: new permitted set not a subset of the old permitted set (an attempt to raise): `EPERM`.
- F8: new effective set not a subset of the new permitted set: `EPERM`.
- F9: new inheritable set adding bits outside old inheritable and permitted without effective `CAP_SETPCAP`, or outside old inheritable and the bounding set: `EPERM`.
- F10: bits above the kernel's highest capability number are dropped silently.
- F11: on success the ambient set is reduced to its intersection with the new permitted and inheritable sets.
- F12: unreadable header, unwritable header during the version fix-up, or unreadable `data`: `EFAULT`.
- F13: an LSM may refuse the change through `security_capset`.
- F14: the new sets apply to the calling thread only, are inherited by threads and children it creates later, and are recomputed by `execve` (for uid 0, permitted is refilled from the bounding set).

## Determinism analysis

Every input is guest data checked against the thread's current sets, which start from the launch and change only through the guest's own serialized `capset`, `execve` and `prctl` calls. So on one host the result repeats. Two things come from outside the guest. The starting sets depend on the launch path: on ptrace the guest is root in its own user namespace (`uid_map` `0 <caller-uid> 1`, per the comment at `detcore/src/syscall_classification.rs:1004`) and starts with every capability the host kernel defines, while in-process backends "have no namespace at all" (`detcore/src/syscall_classification.rs:1007`) and would start from the launching user's sets, usually empty, so a raise that succeeds on one path fails with `EPERM` (F7) on another. And the set of defined capabilities is the host kernel's (`/proc/sys/kernel/cap_last_cap` reads 40 on the host this body was written on), which decides which bits F10 drops. The kernel-log warnings of F2 and F3 are host-side effects the guest cannot read back. So the call is `conditional`: repeatable for a fixed host kernel and launch path.

### Nondeterminism sources

- The launch path: the starting capability sets, full inside a user namespace or the launching user's own without one.
- The host kernel's highest capability number, which decides which requested bits are kept.
- Host LSM policy (F13), fixed for a given host.

### Cross-host variation

- Host kernel's highest capability number (`/proc/sys/kernel/cap_last_cap`): which requested bits the kernel keeps and which it drops silently, visible through a later `capget` (F10). Hermit forwards the call, so this is not hidden.
- Host LSM policy: whether `security_capset` refuses a change (F13).

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04): `capset` updates the capability sets of the per-process credential record described under "Target: the credential record" below. The code at hermit f4055eb9cae4 has no such record. Today `capset` is forwarded:

- It is classified `PassThrough` at `detcore/src/syscall_classification.rs:717`, under the comment at `detcore/src/syscall_classification.rs:714` that "capget/capset/getgroups observe or update kernel credential state that starts from the fixed container identity on each run".
- It is listed in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1683` and pinned as `PassThrough` by `representative_policies_stay_in_their_reviewed_sections` at `detcore/src/syscall_classification.rs:2043`.
- It reaches the host through the generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843`.
- The setter family is an argument-blind no-op (`is_credential_identity_noop_syscall`, `detcore/src/syscall_classification.rs:974`), so today no `setuid` ever clears a capability set the way the kernel would.
- Reduced fidelity (owner decision 3): under `--no-namespace` and on backends without a user namespace, such as the in-process ones (`detcore/src/syscall_classification.rs:1007`), there is no container root. Today the forwarded `capset` then works against the launching user's capability sets, usually empty, so a raise that succeeds in the ptrace container fails with `EPERM` (F7). Once the record lands the result comes from the record in every mode, but the kernel goes on enforcing the launching user's sets.

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

**What `capset` must do against the record.**

- Validate `header->version` as Linux does (F1-F4): versions 3 and 2 take two data elements, version 1 takes one and clears capabilities 32 and above from all three sets, and any other version is rewritten to version 3 in the guest's header and fails with `EINVAL`.
- Accept `header->pid` 0 or the caller's own thread id as the guest sees it (F5); any other value, including another thread of the same process, returns `EPERM` (F6).
- Read the header and data from guest memory, returning `EFAULT` on a fault (F12).
- Mask each requested set to the capabilities the host kernel defines (F10), the same mask that decides the full starting sets.
- Apply `cap_capset`'s rules against the record (F7-F9): the new permitted set must be a subset of the record's permitted set; the new effective set a subset of the new permitted set; and the new inheritable set may add bits outside the old inheritable and permitted sets only with `CAP_SETPCAP` in the record's effective set, and never outside the old inheritable set and the bounding set, which the container launch fixes (`detcore/src/syscalls/misc.rs:373`). Otherwise return `EPERM` and leave the record unchanged.
- Reduce the ambient set to the new permitted and inheritable sets (F11). On ptrace it starts empty and `PR_CAP_AMBIENT` returns `ENOSYS` (`detcore/src/syscalls/misc.rs:403`), so the reduction has nothing to act on there.
- Afterwards `capget` reads the record back, and the set*id fixups of the `setuid` family act on the same sets.
- Target (owner decision 6, 2026-10-04): the checks above run in the kernel's order (`capset` and `cap_validate_magic` in kernel/capability.c), because the modelled call claims Linux behaviour: read `header->version` (`EFAULT`); an unknown version is rewritten (`EFAULT` if the write faults) and fails with `EINVAL`; read `header->pid` (`EFAULT`); a `pid` other than 0 or the caller's thread id gives `EPERM`; read `data` (`EFAULT`); then mask the sets and apply the `cap_capset` rules (`EPERM`). So a bad version with an unreadable `data` gives `EINVAL`, and a foreign `pid` with an unreadable `data` gives `EPERM`. Today the call is forwarded (`detcore/src/lib.rs:2843`), so the kernel already uses this order; the record handler must keep it.

### Differences from Linux

Under the target (documented differences, owner decision 4):

- **No enforcement.** A capability dropped with `capset` is gone from the record and from `capget`, but the kernel still holds it, so the guest can go on using it. Today, by contrast, the forwarded drop is enforced by the kernel. The `CapEff:`, `CapPrm:` and `CapInh:` lines of `/proc/self/status` show the kernel's sets and disagree with `capget`.
- **No LSM refusal (F13).** The kernel never sees the call, so `security_capset` cannot refuse it, and the deprecation warnings of F2 and F3 are not logged.
- **Per-process scope (F14).** A `capset` on one thread changes what every thread's `capget` reports. On Linux it changes the calling thread only, and glibc has no broadcast for it.
- **`execve` keeps the record (F14).** Linux recomputes the sets at `execve`, and for uid 0 refills the permitted set from the bounding set, so a capability dropped only with `capset` comes back. Decision 4 keeps the record across `execve`, so it does not come back.

Today, until the record lands:

- The emulated `chown` family succeeds regardless of capabilities (`is_ownership_change_noop_syscall`, documented from `detcore/src/syscall_classification.rs:990`), so `chown` still succeeds after `CAP_CHOWN` was dropped. That stays true under the target, which does not enforce the record.
- The `setuid` family is a no-op (`detcore/src/syscall_classification.rs:961`, `detcore/src/syscall_classification.rs:974`), so the sets stay full where the kernel would clear them on a switch from root to a non-zero uid. Under the target the fixups act on the record.
- Privileged administration calls return a fixed `EPERM` because "The deterministic guest does not hold the capabilities these require against the host kernel" (`detcore/src/lib.rs:2158`-`2159`), whatever `capget` reports. That stays true under the target.

Existing evidence: no test at hermit f4055eb9cae4 calls `capset` directly. The only indirect user is the KVM test `run_kvm_setpriv_capability_wrapper_is_deterministic` (`hermit-cli/tests/cli.rs:3724`), which runs `setpriv` under `--strict --verify` and returns early without `/dev/kvm`.

## Recommended tests

- T1 (covers F1, F5, F7, F8): under `hermit run --strict --verify` on ptrace, with raw `syscall(SYS_capset, ...)` version 3 and pid 0: drop `CAP_CHOWN` from the effective set only and confirm with `capget`; raise it back from the permitted set (succeeds); drop it from permitted too and assert raising it again gives `EPERM` and leaves `capget` unchanged; assert an effective set larger than permitted gives `EPERM`.
- T2 (covers F2, F3, F10): assert version 2 behaves like version 3; assert a version 1 `capset` that passes the current low 32 bits leaves `capget` (version 3) showing capabilities 32 and above cleared from all three sets; assert bits above `/proc/sys/kernel/cap_last_cap` in a version 3 request are absent from `capget` afterwards.
- T3 (covers F4, F12): assert version `0x12345678` gives `EINVAL` with `header->version` rewritten to `0x20080522`; a header pointer of `(void *)1` and a `data` pointer of `(void *)1` each give `EFAULT`, and `capget` reports the sets unchanged. To pin the check order (owner decision 6), a bad version with a `data` pointer of `(void *)1` gives `EINVAL`, and pid `INT_MAX` with a `data` pointer of `(void *)1` gives `EPERM`.
- T4 (covers F6): assert pid set to another thread's id, to a `fork` child's pid and to `INT_MAX` each give `EPERM`, and that pid set to the caller's own `gettid()` succeeds.
- T5 (covers F9, F11): after dropping `CAP_SETPCAP` from the effective set, assert adding an inheritable bit that is in permitted succeeds and one that is not gives `EPERM`, each confirmed with `capget`; assert `prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, ...)` gives the documented ptrace answer (`ENOSYS` today), so the ambient set stays empty and the reduction rule has nothing to act on.
- T6 (covers none): run the T1-T5 binary under `--strict --verify` on the CI host and assert no unexpected `EACCES` or `EPERM` beyond those the tests expect. It no longer claims F13: an LSM denial is host policy, which U1 records.
- T7 (covers F14): after dropping `CAP_CHOWN` from effective and permitted, assert a new thread and a `fork` child report the dropped sets via `capget`, and that an `execve`d copy of the program, still uid 0, also reports them dropped: decision 4 keeps the record across `execve`, where Linux would refill `CAP_CHOWN` (a documented difference). Also assert that a drop made on one thread is what a second thread's `capget` reports (the per-process record; on Linux the second thread keeps its sets, a documented difference).
- U1 (covers F13; host-config): a refusal from `security_capset` depends on the host's LSM policy, which the CI host does not set up. Today Hermit forwards the call unchanged (`detcore/src/syscall_classification.rs:717`, `detcore/src/lib.rs:2843`). Under the record target the kernel never sees the call, so no LSM can refuse it (a documented difference) unless the open question above decides to forward accepted requests.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. capset is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1938: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept linux_determinism conditional (guest-driven transitions from launch-path starting sets; defined capabilities follow the host kernel); target_policy investigate -> passthrough (pid is limited to the caller, so only the guest thread changes, and a dropped capability must be enforced by the kernel; a fixed error would break setpriv and the privilege-drop tools the setuid no-ops support); tracking_status todo with an open question for the owner: emulated chown and setuid ignore capabilities dropped with capset, and launch paths without a user namespace start from the launching user's sets; 14 flags; 7 tests; no coverage token recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host cap_last_cap decides which bits F10 drops; host LSM policy); F13 LSM refusal recorded as host-config U1 and T6 no longer claims it; no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy passthrough -> model (capset updates the capability sets of the per-process credential record under cap_capset's rules; capget reads them back; not enforced); Hermit design and T1-T7 rewritten to assert read-back through capget, T7 now asserts execve keeps the record; U1 notes no LSM sees the call under the target; reduced-fidelity line added (--no-namespace and in-process backends start from the launching user's sets); removed both open questions (emulations respecting dropped capabilities, launch path without a user namespace); added one: whether to also forward accepted requests so the kernel keeps enforcing drops
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (record handler keeps the kernel's check order: header version EFAULT/EINVAL, pid EPERM, data EFAULT, then cap_capset EPERM; T3 now pins the order); target_policy stays model; no new F/T ids; no open question removed (the forward-accepted-requests question is not answered by 6-15)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `capset`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+capset&body=Discussion+for+the+%60capset%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcapset.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60capset%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: capset" (https://github.com/rrnewton/hermit/issues/1938), legacy variant C.

### Open questions

- Owner decision 4 makes the record the answer and does not enforce it. Should a `capset` that the record accepts also be forwarded, so the kernel keeps enforcing drops as it does today? In the ptrace container the kernel's sets start equal to the record's and can only be larger afterwards (the no-op set*id calls never clear them), so the kernel would accept every request the record accepts, except for an LSM refusal (F13). The same choice decides whether `execve` should apply Linux's capability recomputation to the record (F14, T7).

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

capset is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.

### Legacy strategy draft (2026-08-07)

Emulate capability transitions in the virtual credential model, enforcing Linux subset and permission rules deterministically. If mutation cannot be modeled safely, return a fixed documented error instead of mutating host credentials.

### Legacy required tests (2026-08-07)

- Exercise legal drops, illegal raises, version negotiation, invalid targets, and invalid pointers.
- Verify capget observes the exact modeled transition and that exec/clone inheritance is deterministic.
- Assert identical return values and errno across strict runs and record/replay.
- Prove no host capability changes in a negative isolation fixture and cover every supporting backend.

### Legacy acceptance text (2026-08-07)

Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.
