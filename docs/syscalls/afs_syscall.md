---
schema: syscall-pages/v1
syscall: afs_syscall
nr_x86_64: 183
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/334
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+afs_syscall&body=Discussion+for+the+%60afs_syscall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fafs_syscall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60afs_syscall%60%2C+or+in+what+the+page+says%3F%0A
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

`afs_syscall` (x86_64 number 183) is a reserved slot for the Andrew File System. Mainline Linux has never implemented it: the slot is wired to `sys_ni_syscall`, which returns `ENOSYS`. Detcore answers it itself with a fixed `ENOSYS` and never forwards it to the host kernel (`detcore/src/lib.rs:2090` at hermit f4055eb9cae4).

Manual reference: Linux man-pages afs_syscall(2) (listed in unimplemented(2)).

## Linux semantics

The number is kept only so the x86_64 syscall table keeps its layout. Its table entry has no entry point, so the kernel dispatches it to `sys_ni_syscall`, which ignores all six argument registers, reads and writes no user memory, changes no kernel state, and returns `-ENOSYS`. The generated x86_64 tables of the 6.13 and 7.1 kernels installed on the evidence host (`arch/x86/include/generated/asm/syscalls_64.h`) both read `__SYSCALL(183, sys_ni_syscall)`. The host strace under Additional information shows the same result with every argument set to `-1`.

Only a kernel outside mainline could give the slot a meaning (historically, out-of-tree AFS modules patched the syscall table). A host seccomp filter can also substitute its own errno for an unknown syscall before `sys_ni_syscall` runs.

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

`linux_determinism: deterministic`. On every mainline x86_64 kernel the call returns `ENOSYS` whatever its arguments, with no input from kernel state, time, scheduling or other processes, so it repeats exactly.

### Nondeterminism sources

- None on a mainline kernel.
- Host-dependent only: a non-mainline kernel that patches slot 183, or a host seccomp policy that answers unknown syscalls with another errno. Neither varies between two runs on the same host, but either makes the native result depend on which host runs the program.

## Hermit design

Target policy: `refuse`, a fixed `ENOSYS` that Detcore produces without consulting the host. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` puts `afs_syscall` in the Determinized class, in the group tagged `TODO-HUMAN-REVIEW(#715)` (https://github.com/rrnewton/hermit/issues/715) for syscalls the kernel leaves at `sys_ni_syscall` (`detcore/src/syscall_classification.rs:270`-`287`).
- `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:856`) lists the same 13 syscalls, including `afs_syscall`.
- In `handle_syscall_event`, the match on `classify_syscall` (`detcore/src/lib.rs:2028`) reaches the arm at `detcore/src/lib.rs:2090`-`2094`, which returns `Err(Error::Errno(Errno::ENOSYS))`. The guest's syscall is never executed on the host. The arm does not depend on `panic_on_unsupported_syscalls`, so the result is the same with fail-closed on (the `hermit run` default) or off (`--allow-unsupported-syscalls`).
- The syscall is intercepted in both subscriptions: run mode subscribes every syscall (`detcore/src/lib.rs:1094`), and the `passthru_opt` subscription that `hermit record` and `hermit replay` use adds every Determinized syscall (`detcore/src/lib.rs:1300`-`1302`).
- It is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), the set other execution paths must answer with the same fixed errno.
- The unit test `unimplemented_enosys_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2209`) checks that the classification and the helper agree on exactly these 13 syscalls. It does not launch Hermit, so it says nothing about the guest-visible result.

Why `refuse` and not the legacy `passthrough` recommendation: on every mainline kernel the two give the guest the same `ENOSYS`, but forwarding would let a patched kernel or a host seccomp policy change the answer. A fixed refusal is identical to mainline Linux and is deterministic by construction. `model` and `emulate` do not apply: there is no behaviour to model. The legacy recommendation was written when the code still forwarded the syscall to the host; it no longer does.

## Recommended tests

- T1 (covers none): under `hermit run --verify` on ptrace, a C program calls `syscall(183)` once with all six arguments zero and once with all six `-1`, and prints the return value and `errno` each time. Both calls must return `-1`/`ENOSYS`, both runs must print the same, and the output must equal a native run on a mainline kernel. One program can sweep all 13 slots of `is_unimplemented_enosys_syscall` at once.
- T2 (covers none): run the T1 program with fail-closed on (the default) and with `--allow-unsupported-syscalls`. In both, the call returns `ENOSYS` and the run continues to a zero exit; fail-closed must not stop the run, because this is a deterministic refusal, not an unsupported syscall.
- T3 (covers none): `hermit record` the T1 program, then `hermit replay` it. Replay must print the same `ENOSYS` results.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/334#issuecomment-5872937566
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/334: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_unimplemented_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; afs_syscall records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/334 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> refuse (Detcore returns a fixed ENOSYS at detcore/src/lib.rs:2090 without reaching the host, identical to mainline sys_ni_syscall); linux_determinism stays deterministic; no flags; 3 tests proposed, none run against this list, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `afs_syscall`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+afs_syscall&body=Discussion+for+the+%60afs_syscall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fafs_syscall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60afs_syscall%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: afs_syscall" (https://github.com/rrnewton/hermit/issues/334), legacy variant A2. It was ranked 55 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/334.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=afs_syscall -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(183, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
afs_syscall(0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff) = -1 ENOSYS (Function not implemented)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/334#issuecomment-5872937566
