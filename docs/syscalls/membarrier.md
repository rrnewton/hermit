---
schema: syscall-pages/v1
syscall: membarrier
nr_x86_64: 324
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+membarrier&body=Discussion+for+the+%60membarrier%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmembarrier.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60membarrier%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`membarrier(2)` lets a thread force a memory barrier on other running threads, so that programs can use cheap one-sided fences on their fast path (for example user-space RCU, or the per-thread state reclamation in some language runtimes). Detcore answers the call itself and never asks the kernel. `MEMBARRIER_CMD_QUERY` returns a fixed mask of 31, and every other command returns 0 without doing anything. This is sound only because Detcore runs one guest thread at a time.

## Linux semantics

The call is `membarrier(cmd, flags, cpu_id)`. `cmd` selects one operation. On success, every targeted thread that is running has passed through a full memory barrier before the call returns, and threads that are not running will pass one before they next run user code. The "private expedited" commands work only after the process has registered for them; an unregistered call gives `EPERM`. The set of commands a host supports depends on the kernel version, the architecture and the boot configuration, and `MEMBARRIER_CMD_QUERY` reports it.

### Flags and options

- F1: `MEMBARRIER_CMD_QUERY` (0): returns the mask of supported commands. The mask depends on kernel version and architecture, and leaves out `MEMBARRIER_CMD_GLOBAL` when the host boots with `nohz_full`.
- F2: `MEMBARRIER_CMD_GLOBAL` (1, formerly `MEMBARRIER_CMD_SHARED`): a barrier on every thread of every process on the system. Slow, because it waits for an RCU grace period. `EINVAL` under `nohz_full`.
- F3: `MEMBARRIER_CMD_GLOBAL_EXPEDITED` (2) and `MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED` (4): a fast barrier on the threads of every process that registered. The caller itself does not need to register.
- F4: `MEMBARRIER_CMD_PRIVATE_EXPEDITED` (8) and `MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED` (16): a fast barrier on the threads of the calling process. `EPERM` until the process registers.
- F5: `MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE` (32) and its register command (64): as F4, and each target CPU also executes a core-serializing instruction. `EINVAL` on architectures without support, and `EPERM` until registered.
- F6: `MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ` (128) and its register command (256), Linux 5.10: restarts any restartable-sequence critical section running on the caller's threads. `EPERM` until registered.
- F7: `MEMBARRIER_CMD_GET_REGISTRATIONS` (512, Linux 6.3): returns the mask of commands the process has registered for.
- F8: `flags` and `cpu_id`: `flags` must be 0, except `MEMBARRIER_CMD_FLAG_CPU` with the RSEQ command, which limits it to the CPU named by `cpu_id`. Any other `flags` value, an unknown `cmd`, or a `cmd` with more than one command bit gives `EINVAL`.

## Determinism analysis

The return values depend on the host and on the process's own registrations. The `QUERY` mask and whether `GLOBAL` is allowed come from the kernel version, the architecture and `nohz_full`. `EPERM` or 0 for the private commands depends on the program's earlier registrations, which repeat. The barrier itself has no result that a program can observe directly; it only changes which memory writes another thread is guaranteed to see. The RSEQ command aborts whichever critical sections happen to be running, which depends on timing. `linux_determinism` is `conditional`: the results repeat on one host for one program, but the `QUERY` mask and the `GLOBAL` result vary between hosts.

## Hermit design

`target_policy` is `emulate`: Detcore answers every command without calling the kernel. Code at hermit f4055eb9cae4:

- Dispatch: `detcore/src/lib.rs:2793` runs `handle_membarrier` (`detcore/src/syscalls/misc.rs:605-649`). It never calls `record_or_replay` or injects the syscall.
- Rationale, from the doc comment at `misc.rs:609-614`: Detcore runs all guest threads one at a time in a single total order, so any barrier is already satisfied.
- F1: `misc.rs:634-640` answers `QUERY` with `SUPPORTED`, defined at `misc.rs:627-631` as `GLOBAL | GLOBAL_EXPEDITED | REGISTER_GLOBAL_EXPEDITED | PRIVATE_EXPEDITED | REGISTER_PRIVATE_EXPEDITED`, which is 31. This hides the host's mask, so a program sees the same mask on every host.
- F2, F3, F4: `misc.rs:641-647` returns 0 for every other `cmd`. `GLOBAL` succeeds even where a `nohz_full` host would give `EINVAL`, which is consistent with the mask. Where this differs from Linux:
  - `PRIVATE_EXPEDITED` before registration returns 0, where Linux gives `EPERM`.
  - Registration is not recorded anywhere.
- F5, F6, F7: these commands are absent from the mask, so a program that checks `QUERY` will not use them. A program that calls them anyway gets 0:
  - `SYNC_CORE` reports success but executes no serializing instruction.
  - `RSEQ` reports success but restarts nothing.
  - `GET_REGISTRATIONS` returns an empty mask even after registrations.
  Linux would give `EINVAL` for a command that its own mask leaves out.
- F8: `misc.rs:633` reads only `cmd`; `flags` and `cpu_id` are never checked. Non-zero `flags`, an unknown `cmd` and a `cmd` with several bits all return 0 instead of `EINVAL`.
- Why the no-op is sound depends on serialized threads, which `hermit run --no-sequentialize-threads` turns off (`hermit-cli/src/bin/hermit/run.rs:464-466`). In that mode guest threads run in parallel on the host, and this handler still issues no barrier.

Target (owner decision 6): `handle_membarrier` stays an emulation but makes Linux's checks in the kernel's order, as a kernel whose `QUERY` mask is the 31 Hermit reports would. `SYSCALL_DEFINE3(membarrier)` in `kernel/sched/membarrier.c` checks `flags` first (`EINVAL` unless it is 0, or `MEMBARRIER_CMD_FLAG_CPU` with `PRIVATE_EXPEDITED_RSEQ`), then switches on `cmd`: `QUERY` returns the mask, the five supported commands run, and any other value, including a multi-bit `cmd` and the `SYNC_CORE`, `RSEQ` and `GET_REGISTRATIONS` commands that the mask leaves out, gives `EINVAL` (F5 to F8). `PRIVATE_EXPEDITED` before `REGISTER_PRIVATE_EXPEDITED` gives `EPERM` (`membarrier_private_expedited()` tests the address space's registration state), so Detcore must track that registration per address space: Linux keeps it in the `mm`, so threads share it, a `fork` child inherits it (`dup_mm()` in `kernel/fork.c` copies the `mm`, and `mm_init()` does not reset it) and `execve` clears it (`membarrier_exec_mmap()`). `GLOBAL` keeps returning 0, because the reported mask includes it. Today `misc.rs:633-648` reads only `cmd` and returns 0 for everything except `QUERY`; the change is the `flags` check, a `cmd` match whose default arm returns `EINVAL`, and the registration state, all still without calling the kernel.

## Recommended tests

- T1 (covers F1, F2): `QUERY` returns 31 and `GLOBAL` returns 0, identically across runs and hosts under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/membarrier_query.c` (cell `c-programs/membarrier-query` at `tests/e2e/manifests/c-programs.yaml:16555`, verify ci on ptrace, kvm and liteinst, with `bitwise_parity` asserted).
- T2 (covers F3, F4): `REGISTER_GLOBAL_EXPEDITED` then `GLOBAL_EXPEDITED`, and `REGISTER_PRIVATE_EXPEDITED` then `PRIVATE_EXPEDITED`, each return 0. `PRIVATE_EXPEDITED` before registration returns `EPERM`, as on Linux (owner decision 6); Hermit at f4055eb9cae4 returns 0. A `fork` child of a registered parent gets 0 without registering again, and the same program after `execve` gets `EPERM` until it registers. Also run a two-thread exchange that uses a compiler barrier on the fast side and `membarrier` on the slow side; it must finish with identical output.
- T3 (covers F5, F6, F7): call the `SYNC_CORE` and `RSEQ` register and barrier commands and `GET_REGISTRATIONS`. The results must match across runs and must agree with the `QUERY` mask: a command missing from the mask should give `EINVAL`, not 0.
- T4 (covers F8): `GLOBAL` with `flags` 1, `MEMBARRIER_CMD_FLAG_CPU` with a command that is not RSEQ, an unknown `cmd` (`1 << 20`), and `GLOBAL | PRIVATE_EXPEDITED` at once. Linux gives `EINVAL` for all four.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (Detcore answers QUERY with 31 and every other command with 0, never calling the kernel); linux_determinism stays conditional; 8 flag behaviours and 4 tests listed; open questions for the owner on argument validation (unregistered PRIVATE_EXPEDITED, unknown cmd and non-zero flags return 0, not EPERM or EINVAL) and on the no-op under --no-sequentialize-threads; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied: target is Linux's checks in kernel order for a mask-31 kernel (flags EINVAL, then EINVAL for unknown, multi-bit and unreported commands, EPERM for PRIVATE_EXPEDITED before registration, registration tracked per address space, inherited by fork, cleared by execve); today detcore/src/syscalls/misc.rs:633-648 returns 0 for every non-QUERY cmd, so a change is needed; target_policy stays emulate; T2 now asserts EPERM; argument-validation open question removed
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/membarrier_query.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `membarrier`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+membarrier&body=Discussion+for+the+%60membarrier%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmembarrier.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60membarrier%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `membarrier`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`hermit-cli/tests/rr_suite.rs:297` runs rr's own `membarrier` program as `rr_membarrier`. It is listed as ignored in `hermit-cli/tests/fail_closed_allowed_ignores.tsv:94` ("ptrace-heavy rr program; requires PMU branch counters and working mount namespaces"), so it is not a coverage path today.

### Open questions

- `hermit run --no-sequentialize-threads` (`hermit-cli/src/bin/hermit/run.rs:464-466`) lets guest threads run in parallel, but `membarrier` remains a no-op. In that mode, should the call go to the kernel, or should the run refuse it? The same question will apply to any future parallel-execution mode.
