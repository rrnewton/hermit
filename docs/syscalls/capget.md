---
schema: syscall-pages/v1
syscall: capget
nr_x86_64: 125
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1937
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+capget&body=Discussion+for+the+%60capget%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcapget.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60capget%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 9
---

## Summary

`capget(header, data)` reads the effective, permitted and inheritable capability sets of the calling thread or of another thread named in the header. Today Hermit forwards it to the host kernel, so on the ptrace backend the guest reads its real sets inside its user namespace; because the set*id calls are no-ops, those sets stay full after a privilege drop that Linux would apply. The target (owner decision 4) is `model`: `capget` reports the capability sets of a per-process credential record in Detcore, which `capset` and the set*id calls update under Linux's rules and which is not enforced. The answer repeats on one host, but the full set's width follows the host kernel's highest capability number.

Manual reference: [capget(2)](https://man7.org/linux/man-pages/man2/capget.2.html).

## Linux semantics

The kernel (`kernel/capability.c`) first reads `header->version` (`cap_validate_magic`). Version 3 (`0x20080522`) and the deprecated version 2 (`0x20071026`) use two 32-bit data elements; version 1 (`0x19980330`) uses one, so capabilities 32 and above are silently left out. Versions 1 and 2 also make the kernel write a one-time warning to the kernel log. Any other version makes the kernel write its preferred version (3) back into `header->version` and fail with `EINVAL`, except that with a NULL `data` pointer the call returns 0; with a valid version and NULL `data` it also returns 0 without copying anything, which is how libraries probe the supported version. Then `header->pid` is read: a negative pid gives `EINVAL`; 0 or the caller's own thread id selects the caller; any other id is looked up as a thread in the caller's PID namespace (`ESRCH` if missing) and passed to the LSM hook `security_capget`, which may refuse. The sets are copied to `data`; any fault reading or writing the header or writing `data` gives `EFAULT`. Capability sets belong to each thread's credentials, so threads of one process can differ. The bounding and ambient sets are not reported here; they are read through `prctl`.

### Flags and options

- F1: version 3 (`_LINUX_CAPABILITY_VERSION_3`): two data elements carrying 64 capability bits per set.
- F2: version 2 (`_LINUX_CAPABILITY_VERSION_2`): treated as version 3; the kernel logs a deprecation warning.
- F3: version 1 (`_LINUX_CAPABILITY_VERSION_1`): one data element; capabilities 32 and above are dropped from the result; the kernel logs a legacy-use warning.
- F4: unknown version: the kernel writes version 3 into `header->version` and returns `EINVAL`, or 0 when `data` is NULL.
- F5: valid version with `data == NULL`: returns 0 and copies nothing.
- F6: `pid` 0 or the caller's own thread id: report the calling thread's sets.
- F7: `pid` of another thread or process visible in the caller's PID namespace: report that thread's sets; `ESRCH` if not found.
- F8: `pid < 0`: `EINVAL`.
- F9: unreadable header, unwritable header during the version fix-up, or unwritable `data`: `EFAULT`.
- F10: an LSM refusing the query through `security_capget` (for example an SELinux policy without the `getcap` permission).
- F11: uid changes alter the sets (capabilities(7), "Effect of user ID changes on capabilities"). When the real, effective and saved uids go from at least one 0 to all nonzero, permitted, effective and ambient are cleared, except that `SECBIT_KEEP_CAPS` (`PR_SET_KEEPCAPS`) keeps permitted. An effective uid change from 0 to nonzero clears effective; one from nonzero to 0 copies permitted into effective. A filesystem uid change from 0 to nonzero clears `CAP_CHOWN`, `CAP_DAC_OVERRIDE`, `CAP_DAC_READ_SEARCH`, `CAP_FOWNER`, `CAP_FSETID`, `CAP_LINUX_IMMUTABLE`, `CAP_MAC_OVERRIDE` and `CAP_MKNOD` from effective; one from nonzero to 0 restores those of them that are permitted. `SECBIT_NO_SETUID_FIXUP` turns all of this off.
- F12: after a successful `capset` on the calling thread, `capget` reports the new sets. A `capset` that breaks the kernel's rules fails with `EPERM` and leaves them unchanged: permitted may only shrink, effective must stay within the new permitted set, and inheritable must stay within the old inheritable plus the bounding set, and also within the old inheritable plus old permitted unless `CAP_SETPCAP` is effective.
- F13: `execve` recomputes the sets (capabilities(7), "Transformation of capabilities during execve()"). For a caller whose real or new effective uid is 0 (unless `SECBIT_NOROOT`), the new permitted set is the old inheritable set plus the bounding set, and effective equals it when the new effective uid is 0. For other callers executing a file without file capabilities, permitted and effective become the ambient set. Inheritable is kept.

## Determinism analysis

The sets are credential state that starts from how the guest was launched and changes only through the guest's own `capset`, `execve` and `prctl` calls, which Hermit serializes. On the ptrace backend the guest runs as uid 0 in a fresh user namespace (the comment at `detcore/src/syscall_classification.rs:1004` gives its `uid_map` as `0 <caller-uid> 1`), and a root process in a user namespace it owns holds every capability the host kernel defines, inside that namespace. So the first answer is the same on every run of one host. It differs across hosts in one way: the full mask has one bit per capability up to the host kernel's highest number, `/proc/sys/kernel/cap_last_cap`, which reads 40 on the host this body was written on; an older kernel reports a narrower mask. It also differs by launch path: with `--no-namespace` there is no user namespace (`detcore/src/syscall_classification.rs:1001`), and the same comment says in-process backends "have no namespace at all" (`detcore/src/syscall_classification.rs:1007`); on those paths a forwarded `capget` would report the launching user's own sets, typically empty, while `getuid` reports 0. The kernel-log warnings of F2 and F3 are host-side effects the guest cannot read back. A target named by pid (F7) is a guest pid, repeatable under the fresh PID namespace and serialized process creation. So the call is `conditional`: repeatable for a fixed host kernel and launch path.

### Nondeterminism sources

- The host kernel's highest capability number, which sets the width of a full mask.
- The launch path: user namespace or none, and the launching user's own capabilities when there is none.
- Ordering of other guest threads' `capset` and `execve` against a query of them (F7); Hermit's serialization fixes it.

### Cross-host variation

- Host kernel's highest capability number (`/proc/sys/kernel/cap_last_cap`): the width of the full effective and permitted masks the guest reads (F1, F2, F6, F7). Hermit forwards the call, so the value is not replaced. The decision 4 record starts from the same sets, so the width stays host-dependent after the model lands.
- Host LSM policy: whether `security_capget` refuses a query (F10).

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04; changed from `passthrough`). The 2026-08-07 strategy draft (kept under Additional information) proposed this record; the p3 pass had rejected it because the record is not enforced, and decision 4 accepts that cost. Detcore keeps a per-process credential record: real, effective, saved and filesystem uid and gid, the supplementary group list and the capability sets. `capget` reports the record's effective, permitted and inheritable sets for the named process. The capability sets start as the container presents them at launch (effective and permitted full up to the host's highest capability, inheritable empty, as T1 describes). `fork` and `clone` copy the record and `execve` keeps it. `capset` updates the sets (its tracking body describes the update side), and the set*id calls apply Linux's capability side effects to them (F11), which is what makes `setuid(0)` fail with `EPERM` after a full drop. The record is not enforced: the kernel goes on checking the guest's real capabilities.

Target (owner decision 6, 2026-10-04): the record handler keeps the kernel's check order described under Linux semantics (`capget` and `cap_validate_magic` in kernel/capability.c), because the modelled call claims Linux behaviour: read `header->version` (`EFAULT`); an unknown version is rewritten and fails with `EINVAL`, or returns 0 when `data` is NULL; a valid version with NULL `data` returns 0; read `header->pid` (`EFAULT`); a negative pid gives `EINVAL`; an id that names no guest thread gives `ESRCH`; then write `data` (`EFAULT`). Today the call is forwarded (`detcore/src/lib.rs:2843`), so the kernel already uses this order.

What the code does at hermit f4055eb9cae4. `capget` is classified `PassThrough` at `detcore/src/syscall_classification.rs:716`, under the comment at `detcore/src/syscall_classification.rs:714` that "capget/capset/getgroups observe or update kernel credential state that starts from the fixed container identity on each run". It is listed in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1682`, pinned as `PassThrough` by `representative_policies_stay_in_their_reviewed_sections` at `detcore/src/syscall_classification.rs:2042`, and forwarded by the generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843`. So on ptrace the guest reads the kernel's sets today:

- `capset` is forwarded too (`detcore/src/syscall_classification.rs:717`), so a `capset` already reads back through the kernel (F12).
- The set*id calls are no-ops (`is_credential_identity_noop_syscall`, `detcore/src/syscall_classification.rs:974`), so a uid drop never reaches the kernel: after `setuid(1000)` `capget` still reports the full sets, where Linux reports empty ones (F11). This is the gap the model closes.
- The rest of the capability surface stays with the kernel or the backend. `PR_CAPBSET_READ` is forwarded under the comment "The capability bounding set is fixed by the container launch policy" (`detcore/src/syscalls/misc.rs:373`, in `handle_prctl` at `detcore/src/syscalls/misc.rs:367`); `PR_SET_KEEPCAPS` and `PR_GET_KEEPCAPS` are forwarded as per-thread state (`detcore/src/syscalls/misc.rs:102`); and the config flag `backend_virtualizes_capability_prctls` (`detcore-model/src/config.rs:185`) routes `PR_CAPBSET_DROP` and `PR_CAP_AMBIENT` to the backend (`detcore/src/syscalls/misc.rs:395`). How the record learns the keep-capabilities flag that F11 depends on, and whether the bounding and ambient sets join the record, is an open question recorded in the `prctl` tracking body.
- The KVM test `run_kvm_setpriv_capability_wrapper_is_deterministic` (`hermit-cli/tests/cli.rs:3724`) runs `setpriv --bounding-set=-sys_time /bin/date` under `--strict --verify`, and `setpriv` reads and writes capabilities through this syscall family; it is the only indirect user, runs on the KVM backend only and returns early without `/dev/kvm`. No test calls `capget` directly.
- Reduced fidelity (owner decision 3): on `--no-namespace` (`--core-only`) and on a backend without a user namespace, the forwarded call reports the launching user's own sets, usually empty, while `getuid` reports 0. Once the model lands `capget` reports the record in every mode, but the kernel still enforces the launching user's capabilities there; no fidelity is required.

Why `model` and not the alternatives:

- `passthrough` keeps reporting full capabilities after a privilege drop that the no-op setters hid from the kernel, so a daemon that checks its capabilities are gone sees the drop fail.
- `emulate` with fixed sets would ignore `capset` as well.
- `refuse` would break `setpriv`, `capsh` and libcap users.
- The accepted cost of `model` is the one decision 4 names: the reported sets are not the ones the kernel enforces. After a modelled drop, `capget` reports no `CAP_DAC_OVERRIDE`, but the kernel still lets the guest open files it does not own.

Differences from Linux under the target:

- **Thread scope (F7).** The record is per process, so a `capset` in one thread changes what `capget` reports for every thread of the process; on Linux only the calling thread changes.
- **`execve` (F13).** Decision 4 says `execve` keeps the record, so the sets read after an `execve` are the ones from before it. Linux recomputes them at every `execve`: a root caller's permitted set is refilled from the bounding set, so a capability dropped only with `capset` comes back. The `capset` body's open question on forwarding accepted requests also asks whether this recomputation should apply to the record.
- **`/proc/<pid>/status` is not covered.** Its `CapInh:`, `CapPrm:` and `CapEff:` lines show the kernel's sets, so they disagree with the record after a change. That stays an open item for `/proc` virtualization.

## Recommended tests

- T1 (covers F1, F5, F6): under `hermit run --verify` on ptrace, call raw `syscall(SYS_capget, ...)` with version 3 for pid 0 and for `gettid()`, print the three 64-bit sets, and assert they are identical, that effective and permitted equal the full mask computed from `/proc/sys/kernel/cap_last_cap`, that inheritable is 0, and that the same call with NULL `data` returns 0.
- T2 (covers F2, F3): assert version 2 returns the same sets as version 3, and version 1 returns exactly their low 32 bits with the second data element untouched (canary).
- T3 (covers F4): assert version `0x12345678` returns `EINVAL` and leaves `header->version == 0x20080522`, and with NULL `data` returns 0 with the same rewrite.
- T4 (covers F7): have a second thread drop `CAP_CHOWN` from its effective set with `capset`; from the main thread assert `capget` on that thread's id shows the drop, and that the main thread's own sets show it too (the record is per process; on Linux only the calling thread changes, a documented difference). Have a `fork` child drop `CAP_CHOWN` and assert `capget` on the child's pid shows the drop while the parent's sets do not; assert a reaped child's pid gives `ESRCH`.
- T5 (covers F8, F9): assert `pid = -1` gives `EINVAL`, a header pointer of `(void *)1` gives `EFAULT`, and a `data` pointer of `(void *)1` with a valid header gives `EFAULT`. To pin the check order (owner decision 6), `pid = -1` with a `data` pointer of `(void *)1` gives `EINVAL`, and an unknown version with a `data` pointer of `(void *)1` gives `EINVAL`.
- T6 (covers F1, F6, F7): before any capability or uid change, assert the `CapInh:`, `CapPrm:` and `CapEff:` lines of `/proc/self/status` and of `/proc/<tid>/status` for a second thread match the `capget` results. After T4's drop, print those lines without asserting them: they show the kernel's sets, which the record does not cover.
- T7 (covers F11): as root under `--strict` on ptrace, `setresuid(-1, 1000, -1)` makes `capget` report an empty effective set and an unchanged permitted set; `seteuid(0)` copies permitted back into effective; `setfsuid(1000)` removes exactly the eight filesystem capabilities from effective and `setfsuid(0)` restores them; `setresuid(1000, 1000, 1000)` empties permitted and effective, and a following `setuid(0)` fails with `EPERM`. Repeat the full drop after `prctl(PR_SET_KEEPCAPS, 1)` and assert permitted is kept, subject to the open question in the `prctl` body.
- T8 (covers F12): `capset` dropping `CAP_CHOWN` from effective, then from permitted, reads back through `capget` each time; raising `CAP_CHOWN` back into permitted, or putting a capability into effective that permitted lacks, fails with `EPERM` and leaves the sets unchanged.
- T9 (covers F13): after `capset` drops `CAP_CHOWN` from effective and permitted, `execve` a helper that prints its `capget` sets. Linux shows `CAP_CHOWN` again for a root caller; assert the helper still reports it absent, because decision 4 keeps the record across `execve` (a documented difference). Then, after `setresuid(1000, 1000, 1000)`, `execve` the helper again and assert it reports empty permitted and effective sets, which matches Linux. Both results identical across runs.
- U1 (covers F10; host-config): an LSM refusal from `security_capget` depends on the host's security policy, which the CI host does not set up. Today Hermit forwards the call unchanged (`detcore/src/syscall_classification.rs:716`, `detcore/src/lib.rs:2843`). Under the record target the kernel never sees the call, so no LSM can refuse it (a documented difference).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. capget is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1937: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept linux_determinism conditional (repeatable per host kernel and launch path; the full mask width follows /proc/sys/kernel/cap_last_cap); target_policy investigate -> passthrough (capget must report the sets the kernel enforces; PR_CAPBSET_READ is already forwarded at detcore/src/syscalls/misc.rs:373 and backend-owned capability state is deferred to at detcore/src/syscalls/misc.rs:395); tracking_status todo with an open question for the owner: on launch paths without a user namespace capget reports the launching user, contradicting the virtual root; 9 flags; 6 tests; no coverage token recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host cap_last_cap sets the full-mask width; host LSM policy); F7 narrowed and new F10, LSM refusal via security_capget (host-config U1); the open sub-question on pinning the mask to a fixed capability count is resolved by decision 2
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy passthrough -> model (capget reports the capability sets of the per-process credential record, starting full as in the container, updated by capset and by the set*id capability fixups, not enforced); Hermit design rewritten against today's forwarding with a reduced-fidelity line for --no-namespace and namespace-less backends; new F11 (uid-change fixups), F12 (capset read-back and EPERM rules), F13 (execve recompute, kept by the record as a documented difference) and T7-T9; T4 now asserts the per-process scope, T6 asserts /proc Cap lines only before any change, U1 notes no LSM sees the call under the target; open question on launch paths without a user namespace removed (decision 3); cross_host_variant stays true (host cap_last_cap width)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (record handler keeps the kernel's check order: header version, pid EINVAL/ESRCH, then data EFAULT; T5 now pins the order); target_policy stays model; no new F/T ids; no open question removed


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `capget`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+capget&body=Discussion+for+the+%60capget%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcapget.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60capget%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: capget" (https://github.com/rrnewton/hermit/issues/1937), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

capget is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.

### Legacy strategy draft (2026-08-07)

Serve capget from a virtual credential/capability record keyed by logical process identity. Define supported ABI versions and deterministic error precedence; never read ambient host capabilities as the guest answer.

### Legacy required tests (2026-08-07)

- Cover current-process and target-process queries, supported and unsupported ABI versions, and invalid pointers.
- Check exact effective, permitted, and inheritable bitsets plus errno across repeated runs.
- Pair with capset tests to prove modeled transitions are visible deterministically.
- Cover namespace and backend differences with a negative ambient-host-capability fixture.

### Legacy acceptance text (2026-08-07)

Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.
