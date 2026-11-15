---
schema: syscall-pages/v1
syscall: vhangup
nr_x86_64: 153
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/510
  - https://github.com/rrnewton/hermit/issues/531
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+vhangup&body=Discussion+for+the+%60vhangup%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fvhangup.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60vhangup%60%2C+or+in+what+the+page+says%3F%0A
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

`vhangup()` simulates a hangup on the caller's controlling terminal, so that other processes lose access to it. Hermit refuses it: Detcore returns a fixed `EPERM` without forwarding the call (`detcore/src/lib.rs:2164`), which is the errno any process without `CAP_SYS_TTY_CONFIG` in the initial user namespace receives.

Manual reference: [vhangup(2)](https://man7.org/linux/man-pages/man2/vhangup.2.html).

## Linux semantics

`vhangup` takes no arguments. It is used by getty and login programs before they hand a terminal to a new session.

- The kernel checks `CAP_SYS_TTY_CONFIG` with `capable()`, that is, in the initial user namespace. A process that is root only inside a user namespace still gets `EPERM`.
- With the capability, the kernel hangs up the caller's controlling terminal: open descriptors on it stop working (reads see end of file, writes fail), and the terminal's session is signalled (`SIGHUP`, `SIGCONT`). The call returns 0, also when the caller has no controlling terminal.
- Without the capability it returns `EPERM` and changes nothing. The legacy host probes (Linux 6.17, the measurement host) and a 2026-10-03 probe on Linux 7.1.3 (unprivileged, no effective capabilities) all returned `EPERM`.

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

`linux_determinism: conditional`. For a caller without `CAP_SYS_TTY_CONFIG` the result is a fixed `EPERM` with no side effect. For a privileged caller the effect depends on which processes, inside or outside the guest, hold the terminal open, on which session owns it, and on when the resulting signals are delivered, so the observable outcome depends on host terminal state and host scheduling.

The legacy twin https://github.com/rrnewton/hermit/issues/531 classified it as nondeterministic; this body keeps `conditional` from https://github.com/rrnewton/hermit/issues/510, because the nondeterminism exists only on the privileged path, which a Hermit guest never reaches.

### Nondeterminism sources

- Whether the caller holds `CAP_SYS_TTY_CONFIG` in the initial user namespace
- Controlling-terminal ownership, the session and process groups attached to it, and descriptors held by processes outside the guest
- Signal delivery timing for the `SIGHUP`/`SIGCONT` the hangup sends

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the dispatch arm at `detcore/src/lib.rs:2164-2168` returns `Err(Error::Errno(Errno::EPERM))` for every syscall in `is_privileged_admin_refused_syscall` (`detcore/src/syscall_classification.rs:902`), which lists `vhangup` among 16 privileged administration calls. The arm runs before the typed match and before any `record_or_replay` call: it does not read guest memory and never forwards the call, so record, replay and both `--verify` runs compute the same `EPERM`. The refusal applies in every mode, not only when Hermit fails closed. The rationale comments are at `detcore/src/lib.rs:2154-2163` and `detcore/src/syscall_classification.rs:889-901`, tagged `TODO-HUMAN-REVIEW(#722)` (https://github.com/rrnewton/hermit/issues/722).

Why `refuse`:

- The guest's controlling terminal can be the user's real terminal. A forwarded, privileged `vhangup` would hang up a terminal shared with processes outside the guest, a host side effect Detcore cannot model or replay.
- Because the kernel checks the capability in the initial user namespace, an unprivileged Linux process gets exactly this `EPERM`, also when it is root inside a user namespace. The refusal is therefore what Linux returns for the identity Hermit presents (virtual uid 0, `detcore/src/syscall_classification.rs:657-668`), not a Hermit-specific errno.
- `emulate` (pretending the hangup happened) would need a model of terminal sessions that Detcore does not have, and would report success for an operation the guest's credentials could not perform.

The code already realizes the policy. The unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks only that `vhangup` is classified Determinized and is in the refusal set; it does not execute the syscall, and as a unit test it cannot serve as a coverage path.

## Recommended tests

- T1 (covers none): under `hermit run --verify` on ptrace, a guest calls `syscall(SYS_vhangup)`, asserts -1 with `errno == EPERM`, and then writes a marker to stdout to show its descriptors still work. Both runs must print identical output. An unprivileged native run also returns `EPERM`, so this test alone cannot tell the refusal from forwarding; it pins the guest-visible result.
- T2 (covers none): record and then replay the same guest on ptrace; replay must reproduce the `EPERM` and the marker output.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/510: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/531 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2164-2168 and never forwards; capable() in the initial user namespace makes EPERM the native unprivileged result too); linux_determinism stays conditional; merged duplicate https://github.com/rrnewton/hermit/issues/531; no flags, 2 tests listed; no coverage token, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `vhangup`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+vhangup&body=Discussion+for+the+%60vhangup%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fvhangup.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60vhangup%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: vhangup" (https://github.com/rrnewton/hermit/issues/510), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=vhangup` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `vhangup` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
vhangup()                       = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Merged duplicate https://github.com/rrnewton/hermit/issues/531

Filed 2026-07-24 as "Syscall classification: vhangup" (legacy variant A1); its comment history (0 comments) stays on that issue. Merged 2026-10-03 by syscall-tracking-p3:

- Its classification NONDETERMINISTIC lost to CONDITIONAL from https://github.com/rrnewton/hermit/issues/510, for the reason under Determinism analysis.
- Its recommended action NEEDS-HANDLER, and NEEDS-INVESTIGATION from https://github.com/rrnewton/hermit/issues/510, are both superseded by the refusal arm Detcore already has.
- Its host evidence matches the evidence above: on `the measurement host` (Linux 6.17, x86_64, glibc), traced with `strace -qq -f -e trace=vhangup`, at `detcore/src/syscall_classification.rs` revision `dd60278f` (then in the legacy pass-through block):

```text
vhangup()                       = -1 EPERM (Operation not permitted)
```

### 2026-10-03 host probe (syscall-tracking-p3)

Linux 7.1.3, uid 212630 with an empty effective capability set, raw `syscall(153)` through Python ctypes: `vhangup() = -1 EPERM`.
