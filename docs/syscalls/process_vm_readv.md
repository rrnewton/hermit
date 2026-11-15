---
schema: syscall-pages/v1
syscall: process_vm_readv
nr_x86_64: 310
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/382
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+process_vm_readv&body=Discussion+for+the+%60process_vm_readv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprocess_vm_readv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60process_vm_readv%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_process_isolation_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`process_vm_readv(pid, local_iov, liovcnt, remote_iov, riovcnt, flags)` copies bytes from the address space of process `pid` (gathered from `remote_iov`) into the caller's buffers (scattered over `local_iov`) without going through the target. Detcore refuses every call with a fixed `EPERM` before the kernel sees it (`detcore/src/lib.rs:2172-2176`), whatever the target, including the caller itself, and without any argument check.

Manual reference: Linux man-pages process_vm_readv(2).

## Linux semantics

Added in Linux 3.2. The kernel (`mm/process_vm_access.c`) checks in this order:

1. `flags` must be 0, else `EINVAL`.
2. The local iovec array is imported: more than `IOV_MAX` (1024) entries, or a total length that overflows `ssize_t`, gives `EINVAL`; an unreadable array gives `EFAULT`.
3. If the local total length is 0, the call returns 0 at once.
4. The remote iovec array is read (`EINVAL` above `IOV_MAX`, `EFAULT` if unreadable). If every remote length is 0, the call returns 0, still before any PID lookup.
5. `pid` is looked up in the caller's PID namespace; any thread ID works. No such task gives `ESRCH`.
6. Access needs `PTRACE_MODE_ATTACH_REALCREDS` against the target, the same check as a ptrace attach: matching real credentials and a dumpable target, or `CAP_SYS_PTRACE`, and whatever the LSMs add (Yama `ptrace_scope`). A denial gives `EPERM`. A target in the caller's own thread group is always allowed.
7. Bytes are copied remote vector by remote vector, in order, filling local vectors in order; the two lists need not have matching shapes. A remote range that is not mapped or not readable stops the copy. If any byte was copied, the call returns the short count; otherwise it returns `EFAULT`. Man-pages documents partial transfers as happening at the granularity of iovec elements.

The copy is not synchronized with the target: if the target runs concurrently, the bytes read are whatever its memory held at the instant each page was copied. The verified host run under Additional information read 16 bytes of the caller's own memory.

### Flags and options

- F1: `flags != 0`: `EINVAL`, checked before anything else.
- F2: iovec array limits: `liovcnt` or `riovcnt` above `IOV_MAX`, or a local total length that overflows `ssize_t`: `EINVAL`.
- F3: zero-length transfer: a local total of 0, or a remote list whose lengths are all 0 (including `riovcnt == 0`), returns 0 before the PID lookup or the permission check.
- F4: `pid` names the caller's own process or another thread of it: permitted without privilege; copies the bytes.
- F5: `pid` names another process the caller may ptrace-attach to: copies the bytes.
- F6: `pid` names a process the caller may not attach to (different credentials, non-dumpable target, Yama `ptrace_scope` refusal): `EPERM`.
- F7: `pid` names no task in the caller's PID namespace: `ESRCH`.
- F8: unreadable local or remote iovec array, or unwritable local buffer: `EFAULT`.
- F9: remote range not mapped or not readable: `EFAULT` if nothing was copied, otherwise a short count of the bytes copied before the failure.
- F10: scatter and gather: the local and remote lists may have different counts and lengths; bytes flow in list order, and the transfer ends at the shorter total.
- F11: resource exhaustion while pinning pages or allocating the iovec copies: `ENOMEM`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. A read of the caller's own memory (F4) by a single-threaded caller returns deterministic bytes. Everything else depends on things outside the program: the bytes read from another process (F5) depend on where that process is in its execution when the copy happens, which is a scheduling race; the PID values and whether they name a live task (F7) depend on PID allocation; and whether access is allowed (F6) depends on host credentials, dumpability and Yama or other LSM policy. Even a self-read is racy if another thread of the caller writes the source concurrently.

### Nondeterminism sources

- Scheduling: the target's progress at copy time decides the bytes read
- PID allocation and process lifetime: whether `pid` names a live task, and which one
- Host policy: credentials, dumpability, `CAP_SYS_PTRACE`, Yama `ptrace_scope` and other LSMs
- The target's memory map at copy time, which decides where a partial transfer stops

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `EPERM`, the errno Detcore returns today (`detcore/src/lib.rs:2172-2176`), returned before any argument is examined. Linux's `process_vm_rw` checks in this order: `flags != 0` gives `EINVAL` (F1); importing the local and remote iovec arrays gives `EINVAL` or `EFAULT` (F2, F8); a zero total length returns 0 (F3); the PID lookup gives `ESRCH` (F7); the `PTRACE_MODE_ATTACH_REALCREDS` access check in `mm_access` gives `EPERM` (F6); then the copy runs (F9-F11). Under decision 6 the fixed `EPERM` comes before all of these, so a valid call and a garbage-argument call get the same `EPERM`, as T3 and T4 assert. Owner decision 9 makes `refuse` the long-term target; Detcore will not model cross-process copies. The supportable fallback is a program's ordinary path for moving data between processes: MPI shared-memory transports, for example, fall back to copying through a shared-memory segment when single-copy cross-memory attach is unavailable (recalled, not re-checked in an MPI source for this body). One part of this target is not settled by the decisions: a self-targeted read (F4) is allowed unprivileged on Linux, which is decision 6's stated exception, yet decision 9 refuses the whole family. This body keeps `EPERM` for every target, as the code does, and records the conflict as an open question. Today's code realizes the target as stated, so no code change is needed unless the owner resolves that question differently.

- `process_vm_readv` is classified `Determinized` (`detcore/src/syscall_classification.rs:529`) and belongs to `is_process_isolation_refused_syscall` (`detcore/src/syscall_classification.rs:1137-1145`). The dispatch arm at `detcore/src/lib.rs:2172-2176` returns `Err(Error::Errno(Errno::EPERM))` without reading any argument or contacting the kernel. The arm has no mode or fail-closed condition, so `run`, `--verify`, `--strict` and record/replay all see the same `EPERM`, and nothing is copied into the guest's buffers.
- The set is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1328`) and not of the fail-closed-only set, matching the code facts.
- The doc comment at `detcore/src/syscall_classification.rs:1130-1136` gives the reason: the call addresses another process through PIDs and lifetime and permission state that Detcore does not model, and a fixed `EPERM` does not depend on host capabilities or Yama/LSM policy.

Why `refuse` today:

- `passthrough` would hand the guest's PID argument to the host kernel without translation into host task IDs, let the guest read processes outside the container if host policy allowed it, and make the result depend on host Yama policy.
- `EPERM` is a value Linux itself returns for a denied target (F6), so programs that probe the call already handle it.

Where the refusal differs from Linux:

- Every argument class F1-F11 gets `EPERM`. Linux would return `EINVAL` for F1 and F2, 0 for F3, data for F4, F5 and F10, `ESRCH` for F7, and `EFAULT` for F8 and F9.
- The largest gap is F4: a read of the caller's own memory always succeeds natively. Detcore itself, in its own process, issues a self-targeted `process_vm_readv` with no remote vectors to ask the running kernel whether iovec ranges are importable (`detcore/src/iovecs.rs:43-56`); a guest doing the same thing gets `EPERM`.

Before owner decision 9, a `model` policy was considered plausible: Detcore sequentializes guest threads, so a copy between guest processes would happen at a scheduler-chosen point, and the remaining work would be guest-to-host PID translation, refusing targets outside the container, and a permission answer computed from guest state. Decision 9 settles the family as `refuse`.

## Recommended tests

- T1 (covers F4): a self-targeted one-byte read under `hermit run --strict --verify` on ptrace returns -1 with `errno == EPERM` and leaves the destination byte unchanged, identically in both runs.
  Existing candidates: `tests/c/process_vm_readv_refusal_probe.c` (cell `c-programs/process-vm-readv-refusal-probe`, `tests/e2e/manifests/c-programs.yaml:6078`, verify `ci: ptrace: true`), also run by `process_isolation_refusals_verify` in `hermit-cli/tests/process_isolation_refusals.rs:34` with `--backend=ptrace` (`hermit-cli/tests/process_isolation_refusals.rs:69`).
- T2 (covers F5, F6, F10): a parent forks a child that holds a known buffer; the parent's read of the child's buffer, with mismatched local and remote iovec shapes, returns `EPERM` and copies nothing. Repeat after the child calls `prctl(PR_SET_DUMPABLE, 0)`: still `EPERM`.
- T3 (covers F1, F2, F3): `flags = 1`, `liovcnt = IOV_MAX + 1`, a zero-length local vector, and `riovcnt = 0` each return `EPERM` (where Linux returns `EINVAL`, `EINVAL`, 0 and 0). The test pins the refusal as unconditional, so a later change to argument handling is a visible policy change.
- T4 (covers F7, F8, F9): a PID that names no task, a local iovec pointer of `(void *)-1`, and a remote range at an unmapped address each return `EPERM`, never `ESRCH` or `EFAULT`.
- T5 (covers F11): a request with `IOV_MAX` local and remote vectors over a large buffer returns `EPERM` without allocating or pinning anything; the guest's RSS before and after is unchanged.
- T6 (covers F4, F5): record and replay a guest that calls `process_vm_readv` on itself and on a child and prints each errno; replay prints the same `EPERM` lines.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/382: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_process_isolation_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (investigate -> refuse; Detcore returns a fixed EPERM for every call at detcore/src/lib.rs:2172-2176, including self-targeted reads that Linux always allows); linux_determinism stays conditional; 11 flag behaviours and 6 tests listed; the existing probe covers only a self-read and no ptrace result was inspected, so tracking_status stays todo and ptrace_verification untested; open question for the owner on modelling guest-internal targets
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed EPERM before any argument check, already realized at detcore/src/lib.rs:2172-2176; kernel check order recorded); target_policy stays refuse; no new F/T ids; removed the open question on modelling targets inside the container; added an open question on the self-target exception and ENOSYS versus EPERM
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/process_vm_readv_refusal_probe.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `process_vm_readv`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+process_vm_readv&body=Discussion+for+the+%60process_vm_readv%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprocess_vm_readv.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60process_vm_readv%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: process_vm_readv" (https://github.com/rrnewton/hermit/issues/382), legacy variant A1. Companion: process_vm_writev (https://github.com/rrnewton/hermit/issues/383), which shares the dispatch arm and the refusal set.

The refusal was added by the change the code tags `TODO-HUMAN-REVIEW(PR-844)` (`detcore/src/lib.rs:2170`); that tag still asks for a human review of the policy.

### Open questions

- Decision 6 exempts calls that Linux allows unprivileged, and a self-targeted `process_vm_readv` (F4) is one: Linux never answers it with `EPERM`. Under the standing least-common-denominator policy a refusal must look like a real kernel configuration, and the configuration that refuses every caller is a kernel built without `CONFIG_CROSS_MEMORY_ATTACH`, which gives `ENOSYS`. Should the family (this call and `process_vm_writev`) switch from `EPERM` to `ENOSYS`, so that no case needs an exception? Today's `EPERM` stays the target until the owner decides; the change would be one errno in the arm at `detcore/src/lib.rs:2175` and the expected errno in T1-T6.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=process_vm_readv` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `process_vm_readv` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
process_vm_readv(3069897, [{iov_base="\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", iov_len=16}], 1, [{iov_base=0x407000, iov_len=16}], 1, 0) = 16
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

Note added 2026-10-03: the 2026-07-24 sentence about routing through a pass-through block is stale. At hermit f4055eb9cae4 the call is `Determinized` and refused with `EPERM` (see Hermit design).
