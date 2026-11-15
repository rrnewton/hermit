---
schema: syscall-pages/v1
syscall: process_madvise
nr_x86_64: 440
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/380
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+process_madvise&body=Discussion+for+the+%60process_madvise%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprocess_madvise.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60process_madvise%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`process_madvise(pidfd, iovec, vlen, advice, flags)` applies `madvise`-style advice to address ranges of the process a pidfd names. Hermit never consults the kernel: it returns `EINVAL` for nonzero `flags`, `EBADF` for a negative `pidfd`, and `EPERM` for everything else, as if the caller had no permission over any target.

## Linux semantics

Added in Linux 5.10. The kernel resolves the pidfd, requires `PTRACE_MODE_READ` access to the target and `CAP_SYS_NICE` for a target other than the caller, and applies `advice` to each range in the `iovec` array. It returns the number of bytes advised; if a later range fails after earlier ranges succeeded, it returns the partial count. For another process only non-destructive advice is accepted (`MADV_COLD`, `MADV_PAGEOUT`, `MADV_WILLNEED`, and `MADV_COLLAPSE` from Linux 6.1). Recent kernels also accept any `madvise` advice when the target is the caller itself, either through a pidfd for itself or through the negative `PIDFD_SELF` sentinels.

### Flags and options

- F1: `MADV_COLD`: deactivate the target's pages so they are reclaimed first.
- F2: `MADV_PAGEOUT`: reclaim the target's pages now.
- F3: `MADV_WILLNEED`: prefetch the target's pages.
- F4: `MADV_COLLAPSE` (Linux 6.1): collapse the ranges into transparent huge pages synchronously.
- F5: Self target on recent kernels: any `madvise` advice, including destructive advice such as `MADV_DONTNEED` and `MADV_FREE`, through a pidfd for the caller or a `PIDFD_SELF` sentinel.
- F6: `flags`: must be 0; otherwise `EINVAL`.
- F7: `iovec` and `vlen`: `vlen` above `UIO_MAXIOV` (1024) returns `EINVAL`; an unreadable `iovec` returns `EFAULT`; the return value is the byte count, or a partial count when a later range fails.
- F8: Target and permission errors: a descriptor that is not a pidfd returns `EBADF`; an exited target returns `ESRCH`; missing ptrace access or `CAP_SYS_NICE` returns `EPERM`; advice not allowed for a remote target returns `EINVAL`.

## Determinism analysis

`linux_determinism: conditional`. The non-destructive remote advice does not change memory contents, but its effect on page residency, and the outcome of the permission checks, depend on host state.

### Nondeterminism sources

- `MADV_COLD`, `MADV_PAGEOUT` and `MADV_WILLNEED` change which pages are resident, which is visible through `mincore`, page-fault counts and timing, and depends on host memory pressure.
- `MADV_FREE` on a self target lets the kernel discard pages lazily, so whether the old contents survive depends on memory pressure.
- `MADV_COLLAPSE` succeeds or fails depending on whether the host can allocate huge pages.
- `EPERM` versus success depends on the host's credentials, capabilities and ptrace policy (for example the Yama `ptrace_scope` setting).

The call repeats for fixed credentials, a self or permitted target, and advice that does not depend on memory pressure.

## Hermit design

Target policy: `emulate`. Detcore computes a fixed result from the arguments alone, the one an unprivileged caller without access to any target would see, and never lets a host pidfd change another process's memory.

What the code does at hermit f4055eb9cae4:

- Dispatch: a Sysno-guarded arm calls `handle_process_madvise` with arguments 0 (`pidfd`) and 4 (`flags`) only (`detcore/src/lib.rs:2041-2046`); the pinned Reverie exposes the call only as a raw syscall (`detcore/src/lib.rs:2040`).
- Handler: nonzero `flags` returns `EINVAL` (`detcore/src/syscalls/misc.rs:461-463`); a `pidfd` that is negative as a C `int` returns `EBADF`; anything else returns `EPERM` (`detcore/src/syscalls/misc.rs:465-471`). The doc comment states the intent: "Reject cross-process memory advice without consulting host process state" (`detcore/src/syscalls/misc.rs:459`).
- Classification: determinized (`detcore/src/syscall_classification.rs:238`).

Why emulate: the legacy recommendation was NEEDS-INVESTIGATION. Forwarding would expose host memory pressure and host ptrace policy, and the call's only guest-visible benefit is performance. `EPERM` is a result Linux itself returns to an unprivileged caller, so callers already handle it.

Known gaps (where Hermit's fixed result differs from Linux for the same arguments):

- A non-negative descriptor that is closed or is not a pidfd returns `EPERM`; Linux returns `EBADF`.
- F5: a self-targeted call returns `EPERM`, where recent Linux accepts it, and the `PIDFD_SELF` sentinels are negative and so return `EBADF`.
- `advice`, `iovec` and `vlen` are never inspected, so F7's `EINVAL` and `EFAULT` and F8's invalid-advice `EINVAL` become `EPERM`.
- No test calls `process_madvise`.

## Recommended tests

- T1 (covers F6): `flags = 1` with a self pidfd returns `EINVAL` under `--strict --verify`.
- T2 (covers F8): `pidfd = -1` returns `EBADF`; a pidfd for a child, a pidfd for the caller, and a closed descriptor number each return `EPERM`.
- T3 (covers F1, F2, F3, F4): Each of `MADV_COLD`, `MADV_PAGEOUT`, `MADV_WILLNEED` and `MADV_COLLAPSE` on a child pidfd returns `EPERM`, and the child's memory contents are unchanged.
- T4 (covers F5): `MADV_DONTNEED` on a self pidfd returns `EPERM` and a page written beforehand still reads back its data.
- T5 (covers F7): `vlen = 0`, `vlen = 1025` and a NULL `iovec` each return `EPERM`, pinning today's argument-blind result.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/380: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch sysno-guard.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (host-free fixed result: EINVAL for flags, EBADF for a negative pidfd, EPERM otherwise, at detcore/src/syscalls/misc.rs:460-472); linux_determinism stays conditional; 8 flag behaviours and 5 tests listed; no test calls process_madvise, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `process_madvise`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+process_madvise&body=Discussion+for+the+%60process_madvise%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprocess_madvise.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60process_madvise%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: process_madvise" (https://github.com/rrnewton/hermit/issues/380), legacy variant A1.

The legacy Determinism analysis said the pidfd identity "must be virtualized". At f4055eb9cae4 the pidfd is never resolved, so no identity reaches the host.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=process_madvise` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `process_madvise` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
process_madvise(5, [{iov_base=0x407000, iov_len=4096}], 1, MADV_NORMAL, 0) = 4096
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
