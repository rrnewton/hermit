---
schema: syscall-pages/v1
syscall: process_mrelease
nr_x86_64: 448
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/381
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+process_mrelease&body=Discussion+for+the+%60process_mrelease%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprocess_mrelease.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60process_mrelease%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_optional_memory_feature_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`process_mrelease(pidfd, flags)` frees the address space of a process that is already dying, referenced by a pidfd, so a userspace out-of-memory killer gets the memory back without waiting for the victim to finish exiting. Hermit refuses it: Detcore returns `ENOSYS` for every argument without consulting the host (`detcore/src/lib.rs:2343-2347` at hermit f4055eb9cae4), which is what a kernel older than 5.15 returns.

## Linux semantics

Added in Linux 5.15 for userspace out-of-memory killers. The call only speeds up reclaim; it never changes what the target computed, because the target already has a fatal signal pending or is exiting. The kernel checks `flags` first, then resolves the pidfd to a task, then decides whether that task's memory can be reaped. Reaping runs the same code as the kernel's own OOM reaper, under the target's mmap lock taken for reading.

The 2026-07-24 trace under Additional information shows a bad pidfd failing with `EBADF` on Linux 6.17.

### Flags and options

- F1: `flags != 0`: `EINVAL`. No flag is defined, and this check runs before the pidfd is examined.
- F2: `pidfd` is not an open descriptor, or is a descriptor that is not a pidfd: `EBADF`.
- F3: the pidfd's process has already been reaped (exited and waited for), so no task remains: `ESRCH`.
- F4: the target is alive and not exiting, or shares its address space with a process that is not exiting: `EINVAL`. If the OOM reaper has already finished with that address space, the call returns 0 instead.
- F5: the target is dying and still has its address space: the memory is reaped and the call returns 0. It returns `EAGAIN` when the reap could not complete, and `EINTR` when a fatal signal reaches the caller while it waits for the target's mmap lock.
- F6: the target is dying and has already released its address space: 0.
- F7: availability: `ENOSYS` on kernels before 5.15.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The argument errors (F1, F2) repeat. Everything that involves a real target races with that target's exit.

### Nondeterminism sources

- How far the target's exit has progressed when the call runs decides between 0, `EINVAL`, `ESRCH` and `EAGAIN` (F3-F6). That depends on host scheduling of the target, of the kernel OOM reaper, and of the parent that waits for it.
- Contention on the target's mmap lock (`EAGAIN`, `EINTR`).
- A pidfd names a host process; its identity must be virtualized as it is for every other pidfd call.
- Kernel version decides whether the call exists (F7).

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` (`detcore/src/syscall_classification.rs:52`) lists `Sysno::process_mrelease` among the Determinized calls (`detcore/src/syscall_classification.rs:541`), under the comment tag `TODO-HUMAN-REVIEW(PR-839)`, which says a fixed `ENOSYS` keeps guest behaviour host-independent and lets callers use their portable fallbacks.
- `is_optional_memory_feature_syscall` (`detcore/src/syscall_classification.rs:1263`) selects it, and its doc comment names the reason for this call: it "operates on a dying process through a pidfd" and would expose host process lifecycle.
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2343-2347`) returns `Err(Error::Errno(Errno::ENOSYS))` for that predicate, before the typed match and without a condition on fail-closed mode.
- The predicate is one term of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`, term at `detcore/src/syscall_classification.rs:1333`), which is why the code facts show `refused: true`.

Every behaviour (F1-F7) therefore collapses into the F7 outcome. One visible difference from a 5.15+ kernel: `process_mrelease(-1, 1)` gets `ENOSYS` under Hermit where Linux returns `EINVAL` (F1), because Hermit presents the pre-5.15 kernel and not a kernel that checks flags.

Why `refuse` and not the alternatives:

- The call has no effect on any result a guest can compute; it only changes how fast host memory comes back. Under Hermit the guest's memory pressure is not the host's, so the call has nothing useful to do.
- `model` would need Detcore to track a target's exit progress deterministically, so that 0, `EINVAL` and `ESRCH` come out in a fixed order. That is possible in principle, since Detcore schedules every guest thread, but it buys nothing a guest needs: an OOM killer that gets `ENOSYS` still kills its victim with `kill` or `pidfd_send_signal` and simply does not speed up reclaim.
- `passthrough` exposes the host race described under Determinism analysis.

The pidfd itself comes from `pidfd_open`, which Detcore handles in its typed match (`detcore/src/lib.rs:2591`), so refusing `process_mrelease` does not prevent a guest from creating or using pidfds for other calls.

## Recommended tests

- T1 (covers F2, F7): `syscall(SYS_process_mrelease, -1, 0)` returns `-1` with `errno == ENOSYS` under `hermit run --strict --verify --panic-on-unsupported-syscalls` on ptrace, and the verify run reports determinism.
  Existing candidates: `tests/c/process_mrelease_enosys.c` (cell `c-programs/process-mrelease-enosys`, `tests/e2e/manifests/c-programs.yaml:6011`, verify with `ci` true on ptrace) and `optional_memory_features_fall_back_deterministically` at `hermit-cli/tests/optional_memory_features.rs:30`, which runs it with `--backend=ptrace` (`hermit-cli/tests/optional_memory_features.rs:68`). Both use only `(-1, 0)`.
- T2 (covers F1): `process_mrelease(-1, 1)` and `process_mrelease(valid_pidfd, 0x80000000)` return `ENOSYS` under Hermit. Natively on 5.15+ both are `EINVAL`; the test records that Hermit presents the pre-5.15 result.
- T3 (covers F3, F4, F5, F6): a guest forks a child that blocks, obtains a pidfd with `pidfd_open`, and calls `process_mrelease` three times: while the child is alive, after `kill(child, SIGKILL)` but before `waitpid`, and after `waitpid`. Every call returns `ENOSYS`, `waitpid` still reports death by `SIGKILL`, and the output is identical across the `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/381: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_optional_memory_feature_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns ENOSYS for every argument at detcore/src/lib.rs:2343-2347, presenting a pre-5.15 kernel; the call only speeds host reclaim and its result races with the target's exit); linux_determinism stays conditional; 7 flag behaviours and 3 tests listed; existing ptrace tests cover (-1, 0) only and no coverage token is recorded, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by hermit-cli/tests/optional_memory_features.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `process_mrelease`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+process_mrelease&body=Discussion+for+the+%60process_mrelease%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprocess_mrelease.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60process_mrelease%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: process_mrelease" (https://github.com/rrnewton/hermit/issues/381), legacy variant A1.

### 2026-07-24 host strace evidence

The routing described in this subsection is historical. At hermit f4055eb9cae4 the call no longer passes through; Detcore refuses it with `ENOSYS` (see Hermit design).

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=process_mrelease` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `process_mrelease` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
process_mrelease(-1, 0)         = -1 EBADF (Bad file descriptor)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
