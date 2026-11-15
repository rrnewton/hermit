---
schema: syscall-pages/v1
syscall: sethostname
nr_x86_64: 170
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/430
  - https://github.com/rrnewton/hermit/issues/446
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sethostname&body=Discussion+for+the+%60sethostname%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsethostname.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sethostname%60%2C+or+in+what+the+page+says%3F%0A
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

`sethostname(name, len)` sets the host name of the caller's UTS namespace, which `uname` and `gethostname` then report. Hermit refuses it today: Detcore returns a fixed `EPERM` without forwarding the call or reading `name` (`detcore/src/lib.rs:2164`), so the guest's host name stays the one Hermit set up for the whole run. The target (owner decision 11) is to emulate it: a per-run name that `uname` then reports, with `EINVAL` for an over-long name.

Manual reference: [sethostname(2)](https://man7.org/linux/man-pages/man2/sethostname.2.html).

## Linux semantics

- The kernel checks `CAP_SYS_ADMIN` in the user namespace that owns the caller's UTS namespace before it checks `len`. A 2026-10-03 probe on Linux 7.1.3, unprivileged, returned `EPERM` for both `len = -1` and `len = 1000`. The legacy probes on Linux 6.17 returned `EPERM` for `sethostname("", 0)`.
- With the capability, `len` must be between 0 and 64 (`__NEW_UTS_LEN`). The kernel copies `len` bytes, zero-fills the rest of the 65-byte field, and the new name is visible at once to every process sharing the UTS namespace, through `uname`, `gethostname` and `/proc/sys/kernel/hostname`.
- Root in a user namespace that owns a fresh UTS namespace (the usual container setup) passes the check; root in a user namespace whose UTS namespace belongs to another user namespace does not.

### Flags and options

- F1: `0 <= len <= 64`, privileged caller: the UTS host name becomes the first `len` bytes of `name`; `len = 0` sets an empty name.
- F2: `len < 0` or `len > 64`, privileged caller: `EINVAL`.
- F3: `name` is not readable, privileged caller: `EFAULT`.
- F4: no `CAP_SYS_ADMIN` over the UTS namespace: `EPERM`, before `len` and `name` are checked.
- F5: the change is visible to every process sharing the UTS namespace, including processes outside the caller's process tree, and later `uname`/`gethostname` calls return it.

## Determinism analysis

`linux_determinism: conditional`. The requested bytes come from the guest, so a successful call in a UTS namespace that only the guest uses is a deterministic state change. The result becomes host-dependent when the UTS namespace is shared with processes outside the guest (their own `sethostname` calls race with the guest's, and the guest's change leaks to them), and when success depends on the capabilities the host grants. Unprivileged, the result is always `EPERM`.

### Nondeterminism sources

- Whether the UTS namespace is shared with processes outside the guest, and the host name they set
- `CAP_SYS_ADMIN` over the UTS namespace, which depends on how the host or container created it
- The host name in effect before the call, which later `uname` output reflects

## Hermit design

Target policy: `emulate` (owner decision 11, 2026-10-04); the code realizes `refuse` today. At hermit f4055eb9cae4 the dispatch arm at `detcore/src/lib.rs:2164-2168` returns `Err(Error::Errno(Errno::EPERM))` for every syscall in `is_privileged_admin_refused_syscall` (`detcore/src/syscall_classification.rs:902`), which lists `sethostname` among 16 privileged administration calls. The arm runs before the typed match and before any `record_or_replay` call: it reads neither argument, never forwards the call, and record, replay and both `--verify` runs compute the same `EPERM`. The refusal applies in every mode, not only when Hermit fails closed. The rationale comments are at `detcore/src/lib.rs:2154-2163` and `detcore/src/syscall_classification.rs:889-901`, tagged `TODO-HUMAN-REVIEW(#722)` (https://github.com/rrnewton/hermit/issues/722).

How the host name reaches the guest: `handle_uname` (`detcore/src/syscalls/misc.rs:523`) forwards `uname` through `record_or_replay` and, when the run has no UTS namespace (`has_uts_namespace`, `detcore-model/src/config.rs:482-485`), overwrites `nodename` with `hermetic-container.local` (`DEFAULT_HOSTNAME`, `detcore/src/consts.rs:27`; override at `detcore/src/syscalls/misc.rs:535-540`). With a UTS namespace the kernel's value is reported. The refusal keeps either value fixed for the whole run, so `uname` never changes under the guest. The legacy twin's claim that "hermit already virtualizes the hostname" is therefore true only for runs without a UTS namespace.

Why today's refusal is not the target: the ptrace backend runs the guest inside a `CLONE_NEWUSER` namespace that maps its uid to 0 (comment at `detcore/src/syscall_classification.rs:660-663`), and `capget` passes through (`detcore/src/syscall_classification.rs:714-717`), so the guest can see `CAP_SYS_ADMIN` in its own user namespace. A uid 0 process in its own UTS namespace may set its host name on Linux, so the fixed `EPERM` is a compatibility difference (container init scripts, `hostname` in build scripts), even though it is deterministic.

Target (owner decision 11): Detcore keeps one host name for the run, shared by every guest process as a UTS namespace is (F5). It starts as the `nodename` `uname` reports today. `sethostname` replaces it and returns 0, and `handle_uname` writes the stored name into `nodename` in every mode, not only when `has_uts_namespace` is false (today's override at `detcore/src/syscalls/misc.rs:535-540`). The handler runs the kernel's checks in the order of `SYSCALL_DEFINE2(sethostname)` in `kernel/sys.c`: the capability check (F4), then `len` outside 0..64 gives `EINVAL` (F2), then `name` is copied (`EFAULT`, F3; `len = 0` copies nothing, so a NULL `name` with `len = 0` succeeds). The stored field is the `len` bytes copied verbatim and zero-filled to 65 bytes, as the kernel does. The guest holds `CAP_SYS_ADMIN` in its own user namespace, so F4 arises only once the decision-4 credential record lets a guest drop that capability; until then the check always passes. Not covered by decision 11: `/proc/sys/kernel/hostname` still reads the kernel's value, the same open item as `/proc` credential lines under decision 4. One way to build it is to forward the call when the run's UTS namespace is owned by the guest's user namespace, so the kernel and `/proc` agree; the container setup is in `hermit-cli/src`, which is not in the export at this SHA, so whether that ownership holds could not be confirmed.

The unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks only that `sethostname` is classified Determinized and is in the refusal set; it does not execute the syscall, and as a unit test it cannot serve as a coverage path.

## Recommended tests

- T1 (covers F1, F5): under `hermit run --verify` on ptrace, a guest forks a child that waits on a pipe, calls `uname`, then `sethostname("hermit-test", 11)`, asserts 0, then calls `uname` again in itself, in the waiting child and in a child forked afterwards, and asserts all three report `nodename` `hermit-test`; then `sethostname("", 0)` returns 0 and `uname` reports an empty `nodename`. Both runs must print the same output.
- T2 (covers F2, F3): `sethostname` with `len = -1` and `len = 65` returns `EINVAL`, a NULL `name` with `len = 5` returns `EFAULT`, a NULL `name` with `len = 0` returns 0, and `len = 64` with a valid 64-byte buffer returns 0 with `uname` reporting all 64 bytes. After each failing call `uname` still reports the previous name.
- T3 (covers F4): after the guest drops `CAP_SYS_ADMIN` from its effective set through `capset` (the decision-4 capability record), `sethostname` with `len = -1` returns `EPERM`, not `EINVAL`, and `uname` is unchanged.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/430: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/446 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (fixed EPERM at detcore/src/lib.rs:2164-2168, never forwarded; uname stays fixed for the run); linux_determinism stays conditional; merged duplicate https://github.com/rrnewton/hermit/issues/446; 5 flag behaviours and 2 tests listed; open question for the owner: emulate inside a guest-owned UTS namespace instead of refusing; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 11 applied (per-run host name reported by uname, kernel check order EPERM then EINVAL then EFAULT); target_policy refuse->emulate; T1 and T2 rewritten for the target; new T3 (F4); open question on emulating inside a guest-owned UTS namespace removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sethostname`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sethostname&body=Discussion+for+the+%60sethostname%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsethostname.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sethostname%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sethostname" (https://github.com/rrnewton/hermit/issues/430), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=sethostname`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `sethostname` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
sethostname("", 0)              = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### Merged duplicate https://github.com/rrnewton/hermit/issues/446

Filed 2026-07-24 as "Syscall classification: sethostname" (legacy variant A1); its comment history (0 comments) stays on that issue. Merged 2026-10-03 by syscall-tracking-p3:

- Both issues classified the call CONDITIONAL and recommended NEEDS-HANDLER; the refusal arm Detcore already has supersedes the recommendation.
- Its claim that "hermit already virtualizes the hostname seen via gethostname/uname" is corrected under Hermit design: true only when the run has no UTS namespace (`detcore/src/syscalls/misc.rs:535`).
- Its point that a guest's `sethostname` must be "virtualized/scoped so it neither reads nor mutates host-global identity" is kept; it is the reason refusal is safe today and the constraint any `emulate` design must meet.
- Its host evidence, on Linux 6.17 x86_64 with glibc, traced with `strace -f -e trace=sethostname`, matches the evidence above:

```text
sethostname("", 0)              = -1 EPERM (Operation not permitted)
```

### 2026-10-03 host probe (syscall-tracking-p3)

Linux 7.1.3, uid 212630 with an empty effective capability set, raw `syscall(170, NULL, len)` through Python ctypes: `len = -1` and `len = 1000` both returned `EPERM`, showing the capability check precedes the length check.
