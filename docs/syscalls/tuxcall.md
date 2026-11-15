---
schema: syscall-pages/v1
syscall: tuxcall
nr_x86_64: 184
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/501
  - https://github.com/rrnewton/hermit/issues/523
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+tuxcall&body=Discussion+for+the+%60tuxcall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftuxcall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60tuxcall%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_unimplemented_enosys_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`tuxcall` (x86_64 number 184) is a reserved slot for TUX, an in-kernel web server that was never merged into mainline Linux; the slot is wired to `sys_ni_syscall`, which returns `ENOSYS`. Detcore answers it itself with a fixed `ENOSYS` and never forwards it to the host kernel (`detcore/src/lib.rs:2090` at hermit f4055eb9cae4).

Manual reference: [tuxcall(2)](https://man7.org/linux/man-pages/man2/tuxcall.2.html) (listed in unimplemented(2)).

## Linux semantics

TUX was distributed as an out-of-tree kernel patch and was never a supported general userspace API. Mainline keeps the number only so the x86_64 syscall table keeps its layout. The table entry has no entry point, so the kernel dispatches it to `sys_ni_syscall`, which ignores all six argument registers, reads and writes no user memory, changes no kernel state, and returns `-ENOSYS`. The generated x86_64 tables of the 6.13 and 7.1 kernels installed on the evidence host (`arch/x86/include/generated/asm/syscalls_64.h`) both read `__SYSCALL(184, sys_ni_syscall)`. Two independent host probes under Additional information, one with zero arguments and one with leftover register values, both returned `ENOSYS`.

Only a kernel carrying the TUX patch could give the slot a meaning. A host seccomp filter can also substitute its own errno for an unknown syscall before `sys_ni_syscall` runs.

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

`linux_determinism: deterministic`, which both legacy issues agreed on. On every mainline x86_64 kernel the call returns `ENOSYS` whatever its arguments, with no input from kernel state, time, scheduling or other processes, so it repeats exactly.

### Nondeterminism sources

- None on a mainline kernel. Only a kernel or ABI implementation change could alter the result; no supported kernel provides a TUX service.
- Host-dependent only: a kernel carrying the TUX patch, or a host seccomp policy that answers unknown syscalls with another errno. Neither varies between two runs on the same host, but either makes the native result depend on which host runs the program.

## Hermit design

Target policy: `refuse`, a fixed `ENOSYS` that Detcore produces without consulting the host. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` puts `tuxcall` in the Determinized class, in the group tagged `TODO-HUMAN-REVIEW(#715)` (https://github.com/rrnewton/hermit/issues/715) for syscalls the kernel leaves at `sys_ni_syscall` (`detcore/src/syscall_classification.rs:270`-`287`).
- `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:856`) lists the same 13 syscalls, including `tuxcall`.
- In `handle_syscall_event`, the match on `classify_syscall` (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2090`-`2094`, which returns `Err(Error::Errno(Errno::ENOSYS))`. The guest's syscall is never executed on the host. The arm does not depend on `panic_on_unsupported_syscalls`, so the result is the same with fail-closed on (the `hermit run` default) or off (`--allow-unsupported-syscalls`).
- The syscall is intercepted in both subscriptions: run mode subscribes every syscall (`detcore/src/lib.rs:1094`), and the `passthru_opt` subscription that `hermit record` and `hermit replay` use adds every Determinized syscall (`detcore/src/lib.rs:1300`-`1302`).
- It is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), the set other execution paths must answer with the same fixed errno.
- The unit test `unimplemented_enosys_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2209`) checks that the classification and the helper agree on exactly these 13 syscalls. It does not launch Hermit, so it says nothing about the guest-visible result.

Why `refuse` and not the `passthrough` that both legacy issues recommended: on every mainline kernel the two give the guest the same `ENOSYS`, but forwarding would let a patched kernel or a host seccomp policy change the answer. A fixed refusal is identical to mainline Linux and is deterministic by construction. `model` and `emulate` do not apply: there is no behaviour to model. Both legacy issues described the code at the time (hermit `dd60278f` in https://github.com/rrnewton/hermit/issues/523), when the syscall was still forwarded to the host; that description is superseded by the code above.

## Recommended tests

- T1 (covers none): under `hermit run --verify` on ptrace, a C program calls `syscall(184)` once with all six arguments zero and once with all six `-1`, and prints the return value and `errno` each time. Both calls must return `-1`/`ENOSYS`, both runs must print the same, and the output must equal a native run on a mainline kernel. One program can sweep all 13 slots of `is_unimplemented_enosys_syscall` at once.
- T2 (covers none): run the T1 program with fail-closed on (the default) and with `--allow-unsupported-syscalls`. In both, the call returns `ENOSYS` and the run continues to a zero exit; fail-closed must not stop the run, because this is a deterministic refusal, not an unsupported syscall.
- T3 (covers none): `hermit record` the T1 program, then `hermit replay` it. Replay must print the same `ENOSYS` results.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/501: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_unimplemented_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/523 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged duplicate https://github.com/rrnewton/hermit/issues/523 section by section (no contradiction in results; both issues' "temporary pass-through" code description is superseded); target_policy passthrough -> refuse (Detcore returns a fixed ENOSYS at detcore/src/lib.rs:2090 without reaching the host, identical to mainline sys_ni_syscall); linux_determinism stays deterministic; no flags; 3 tests proposed, none run against this list, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `tuxcall`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+tuxcall&body=Discussion+for+the+%60tuxcall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftuxcall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60tuxcall%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: tuxcall" (https://github.com/rrnewton/hermit/issues/501), legacy variant A1. Its comment history stays on that issue.

Duplicate https://github.com/rrnewton/hermit/issues/523 was filed the same day as "Syscall classification: tuxcall" (legacy variant A1) and is retired into this issue; its comment history (0 comments when retired) stays on that issue. Its text was merged above: the TUX description, the man7 manual link, the "only a kernel or ABI change could alter the result" point, and the note that TUX was never a supported userspace API. The two issues did not disagree about Linux behaviour or classification; both described the then-current code as the UNCLASSIFIED temporary pass-through block, which the Hermit design section replaces.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/501)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=tuxcall` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. At the time, Hermit routed `tuxcall` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`; at hermit f4055eb9cae4 it no longer does.

Observed strace line:
```
tuxcall(0, 0, 0)                = -1 ENOSYS (Function not implemented)
```

*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/523)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=tuxcall`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` placed `tuxcall` in the UNCLASSIFIED temporary-passthrough block. The arguments in the line below are leftover register values, not chosen inputs; the result is the same `ENOSYS` as the zero-argument probe above.

Observed strace line:
```text
tuxcall(0x4031fe, 0x7ffd9fe7ed90, 0) = -1 ENOSYS (Function not implemented)
```

*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
