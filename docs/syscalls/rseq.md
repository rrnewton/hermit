---
schema: syscall-pages/v1
syscall: rseq
nr_x86_64: 334
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rseq&body=Discussion+for+the+%60rseq%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frseq.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rseq%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "sysno-guard"
  refused: true
  refused_only_when_fail_closed: true
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`rseq(rseq, rseq_len, flags, sig)` registers a per-thread restartable-sequences area that the kernel keeps updated with the current CPU and uses to abort critical sections on preemption, migration or signal delivery. Under the default fail-closed policy Hermit returns `ENOSYS` without consulting the kernel, so no area is ever registered; only the explicit `--allow-unsupported-syscalls` opt-out forwards the call.

## Linux semantics

A successful registration (flags 0) ties `struct rseq` to the calling thread. From then on, on every return to user space after the thread was preempted, migrated or interrupted by a signal, the kernel writes `cpu_id_start` and `cpu_id` (and `node_id` and `mm_cid` when the registered length covers them). If the thread was inside a critical section described by `rseq_cs`, the kernel checks that the 32-bit `sig` value precedes the abort handler and moves the instruction pointer to `abort_ip`; a signature mismatch kills the thread with `SIGSEGV`. glibc 2.35 and later registers an area for every thread at startup and exports `__rseq_offset` and `__rseq_size`; it treats `ENOSYS` as "not available" and sets `__rseq_size` to 0.

### Flags and options

- F1: Register (`flags == 0`): `rseq_len` must be the original 32-byte size or a supported extended size, and `rseq` must be suitably aligned; returns 0 and starts the CPU-id updates described above.
- F2: Re-registration: the same arguments as the current registration return `EBUSY`; different arguments return `EINVAL`.
- F3: `RSEQ_FLAG_UNREGISTER` (1): unregisters when `rseq` and `rseq_len` match the registration (`EINVAL` otherwise) and `sig` matches (`EPERM` otherwise).
- F4: Invalid arguments: unknown flag bits, a misaligned pointer or an unsupported length return `EINVAL`; an unmapped area returns `EFAULT`.
- F5: Critical-section abort: with `rseq_cs` set, preemption, migration or signal delivery inside `[start_ip, start_ip + post_commit_offset)` moves the thread to `abort_ip`; a wrong signature delivers `SIGSEGV`.
- F6: `rseq_cs` flags `RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT`, `RSEQ_CS_FLAG_NO_RESTART_ON_SIGNAL` and `RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE`: deprecated; recent kernels reject them and the thread gets `SIGSEGV`.
- F7: Lifetime: a `fork` child keeps the registration, a new thread (`CLONE_VM`) starts unregistered, and `execve` clears it.
- F8: Extended fields `node_id` and `mm_cid`: written only when `rseq_len` covers them (the supported size is reported through `AT_RSEQ_FEATURE_SIZE`).

## Determinism analysis

`linux_determinism: nondeterministic`. The return value depends only on the arguments and the current registration, but a successful registration makes the kernel write host state into guest memory asynchronously.

### Nondeterminism sources

- `cpu_id`, `cpu_id_start`, `node_id` and `mm_cid` report the host CPU, NUMA node and concurrency id the thread last ran on, which depends on the host scheduler and the CPU affinity.
- Whether a critical section completes or aborts to `abort_ip` depends on when the host preempts the thread, migrates it, or delivers a signal.
- glibc's own startup registration means any glibc program reads these values (for example through `sched_getcpu`) without making a syscall.

Pinning the guest to one CPU fixes the CPU ids but not the preemption-dependent aborts, so the call does not repeat under any host configuration Hermit controls.

## Hermit design

Target policy: `refuse`. Hermit cannot make the kernel's asynchronous writes and aborts repeat, and `ENOSYS` is an outcome that glibc and other runtimes handle by falling back to ordinary syscalls.

What the code does at hermit f4055eb9cae4:

- Dispatch: a Sysno-guarded arm returns `ENOSYS` when `panic_on_unsupported_syscalls` is set and forwards the call to the kernel otherwise (`detcore/src/lib.rs:2031-2037`; dispatch is by Sysno because Rseq is not typed in the pinned Reverie revision, `detcore/src/lib.rs:2029-2030`). The flag is the fail-closed policy that ordinary `hermit run` enables; `--allow-unsupported-syscalls` is the opt-out (`detcore-model/src/config.rs:447-451`).
- Classification: listed with the determinized rows (`detcore/src/syscall_classification.rs:183`) and as a strict-only deterministic refusal, which "preserve native behavior only under the explicit compatibility opt-out, and fail closed otherwise" (`detcore/src/syscall_classification.rs:1311-1317`). Backends that run guest syscalls outside the dispatcher consult the same predicate; the unit test `deterministic_refusal_aggregate_includes_fixed_error_families` pins rseq in both sets (`detcore/src/syscall_classification.rs:2709-2719`, `detcore/src/syscall_classification.rs:2766-2775`).
- The call is subscribed even under record/replay's pass-through optimization (`detcore/src/lib.rs:1217`), and charged the synchronizing-syscall virtual-time cost (`detcore/src/syscall_time.rs:65-76`).

The refusal happens before any argument check, so F2, F3 and F4 all report `ENOSYS` under the default policy rather than Linux's `EBUSY`, `EPERM` or `EINVAL`. That is intended: the area is never registered, so none of F5 through F8 can occur. Under the opt-out the call reaches the kernel and all the sources above apply; that mode is outside the determinism guarantee.

Why not emulate: an emulation would have to write a fixed `cpu_id` into the area and never abort a critical section. That is reachable for the CPU ids but would silently change F5 semantics for code that relies on aborts for correctness, so a clean `ENOSYS` is the safer contract.

Known gaps: no guest test exercises the refusal; the only coverage is the classification unit test above.

## Recommended tests

- T1 (covers F1, F4, F8): Under default `hermit run --strict`, raw `syscall(SYS_rseq, area, 32, 0, sig)` with a valid area, with an extended length, with an unknown flag bit and with a misaligned pointer each return `ENOSYS`; the area's `cpu_id` keeps its initial sentinel value. Run under `--verify`.
- T2 (covers F2, F3): Register twice and then unregister with `RSEQ_FLAG_UNREGISTER`, once with a wrong `sig`; every call returns `ENOSYS`.
- T3 (covers F5, F6, F7): A glibc program that starts threads, forks and execs, reads `__rseq_size` (expect 0), and calls `sched_getcpu`; it completes under `--strict --verify` with identical output and no `SIGSEGV`.
- T4 (covers none): With `--allow-unsupported-syscalls`, the same raw call reaches the kernel (it returns `EBUSY` or `EINVAL` because glibc already registered), documenting that the opt-out leaves the guarantee.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch sysno-guard, refused only when fail-closed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (fail-closed ENOSYS before the kernel at detcore/src/lib.rs:2031-2037; forwarded only under --allow-unsupported-syscalls); linux_determinism conditional -> nondeterministic (kernel writes host CPU ids and aborts critical sections on host preemption); 8 flag behaviours and 4 tests listed; no guest test exists, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rseq`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rseq&body=Discussion+for+the+%60rseq%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frseq.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rseq%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `rseq`: the 2026-07-24 classification campaign filed issues only for syscalls in that campaign's pass-through block, and Detcore already handled this one.

`tests/standalone/strict_timer_create.sh:10` records that CPython got further under `--strict` "after the rseq + syscall-cascade fixes", which is the history behind the fail-closed arm.
