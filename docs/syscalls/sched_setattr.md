---
schema: syscall-pages/v1
syscall: sched_setattr
nr_x86_64: 314
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/417
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_setattr&body=Discussion+for+the+%60sched_setattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_setattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_setattr%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`sched_setattr(pid, attr, flags)` sets a thread's scheduling policy and its parameters (nice value, real-time priority, SCHED_DEADLINE runtime/deadline/period, utilization clamps) from an extensible `struct sched_attr`. Detcore handles it (`detcore/src/lib.rs:2683`, `detcore/src/syscalls/threads.rs:2318`): it reproduces the kernel's argument checks in the kernel's order, then accepts every well-formed request as a no-op, because Linux scheduling attributes cannot influence Detcore's own scheduler.

Manual reference: Linux man-pages sched_setattr(2), sched(7).

## Linux semantics

The kernel processes a request in a fixed order, and the order is visible when more than one argument is wrong, because the first applicable error wins:

1. `attr` NULL, `pid < 0` or `flags != 0`: `EINVAL`, all from one test.
2. `sched_copy_attr()` reads `attr->size`. A size of 0 means the original 48-byte layout (`SCHED_ATTR_SIZE_VER0`). A size below 48 or above one page is `E2BIG`. A size above the kernel's own struct (56 bytes, `SCHED_ATTR_SIZE_VER1`) is accepted only if every byte past 56 is zero; otherwise `E2BIG`. On every `E2BIG`, the kernel writes its own struct size back into `attr->size`. A fault while reading is `EFAULT`.
3. Utilization-clamp flags with a buffer smaller than VER1: `EINVAL`. A policy with the sign bit set: `EINVAL`.
4. The target thread is looked up: a pid that names no thread is `ESRCH`. Pid 0 means the caller.
5. `__sched_setscheduler()` judges the policy (unless `SCHED_FLAG_KEEP_POLICY`), the defined `sched_flags`, the priority range and its pairing with the policy, and the SCHED_DEADLINE parameters, all with `EINVAL`.
6. Permission and admission: `EPERM` for a real-time policy, a deadline admission or a lower nice value without `CAP_SYS_NICE` or enough `RLIMIT_RTPRIO`/`RLIMIT_NICE`; `EBUSY` when deadline bandwidth admission fails; `EOPNOTSUPP` for clamps without `CONFIG_UCLAMP_TASK`.

On success the thread's policy and parameters change and are visible through `sched_getattr`, `sched_getscheduler`, `sched_getparam` and `getpriority`.

### Flags and options

- F1: `attr` NULL, `pid < 0`, or nonzero `flags` (the syscall's third argument): `EINVAL`.
- F2: `pid` 0 (the caller) or the tid of a live thread; a pid that names no thread is `ESRCH`. The lookup happens after the size and sign checks and before the policy checks.
- F3: `attr->size` 0 (treated as VER0), 48 (VER0) or 56 (VER1): accepted.
- F4: `attr->size` below 48 or above 4096: `E2BIG`, and 56 is written back into `attr->size`.
- F5: `attr->size` between 57 and 4096: accepted when every byte past 56 is zero, otherwise `E2BIG` with the size written back. A fault while scanning is `EFAULT`.
- F6: `attr` that cannot be read (for example unmapped): `EFAULT`.
- F7: policy `SCHED_OTHER` (0), `SCHED_BATCH` (3) or `SCHED_IDLE` (5): must have `sched_priority` 0, otherwise `EINVAL`. `sched_nice` selects the nice value; out-of-range values are clamped, not refused.
- F8: policy `SCHED_FIFO` (1) or `SCHED_RR` (2): `sched_priority` must be 1-99, otherwise `EINVAL`. A nonzero priority needs privilege (`EPERM` otherwise).
- F9: policy `SCHED_DEADLINE` (6): `sched_deadline` nonzero, `sched_runtime` at least 1024 ns, neither deadline nor period with the top bit set, and runtime <= deadline <= period (period 0 means period = deadline); otherwise `EINVAL`. Admission can fail with `EBUSY` or `EPERM`, and the period must lie inside the `sched_deadline_period_{min,max}_us` sysctls.
- F10: policy `SCHED_EXT` (7): accepted only on kernels built with `CONFIG_SCHED_CLASS_EXT`.
- F11: an undefined policy value (4, or anything above 7), or a policy with the sign bit set: `EINVAL`.
- F12: `SCHED_FLAG_KEEP_POLICY` (0x08): keep the current policy and ignore `sched_policy`.
- F13: `SCHED_FLAG_KEEP_PARAMS` (0x10): keep the current parameters.
- F14: `SCHED_FLAG_RESET_ON_FORK` (0x01): children revert to a default policy.
- F15: `SCHED_FLAG_RECLAIM` (0x02) and `SCHED_FLAG_DL_OVERRUN` (0x04): deadline bandwidth reclaiming and overrun signalling.
- F16: `SCHED_FLAG_UTIL_CLAMP_MIN` (0x20) and `SCHED_FLAG_UTIL_CLAMP_MAX` (0x40): set `sched_util_min`/`sched_util_max`. With a buffer smaller than VER1: `EINVAL`. Without `CONFIG_UCLAMP_TASK`: `EOPNOTSUPP`.
- F17: any undefined `sched_flags` bit: `EINVAL`.
- F18: privilege-dependent outcomes: `EPERM` for a real-time priority, a deadline admission or a negative nice value without `CAP_SYS_NICE` or rlimit headroom.
- F19: clamp values: with `SCHED_FLAG_UTIL_CLAMP_MIN` or `_MAX`, a `sched_util_min` or `sched_util_max` above 1024 other than -1 (reset to the default), or `sched_util_min > sched_util_max` when neither is -1: `EINVAL`. Checked after the policy, priority and permission checks (kernel `uclamp_validate()`, called from `__sched_setscheduler()`).
- F20: effect: the policy, `sched_priority`, nice (fair policies only; Linux ignores `sched_nice` for real-time and deadline policies), deadline parameters, the reset-on-fork flag and the clamps are what `sched_getattr`, `sched_getscheduler`, `sched_getparam` and `getpriority` report afterwards. `SCHED_FLAG_KEEP_PARAMS` skips the parameter update (kernel `__sched_setscheduler()` does not call `__setscheduler_params()`), so only the reset-on-fork flag and the clamps change. From Linux 6.12, a nonzero `sched_runtime` on a fair policy sets a custom time slice that `sched_getattr` reports; earlier kernels ignore it.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The argument checks (F1, F3-F7, F11, F17) are pure functions of the arguments. The outcome of a well-formed request depends on host facts: the caller's capabilities and rlimits (F8, F9, F18), the kernel configuration (F10, F16), runtime sysctls (F9), the deadline bandwidth already admitted on the host (F9), and whether the target pid is live (F2). The effect itself, changing how the host schedules the thread, alters timing for the rest of the run.

### Nondeterminism sources

- Caller privileges and rlimits (`CAP_SYS_NICE`, `RLIMIT_RTPRIO`, `RLIMIT_NICE`)
- Kernel build options (`CONFIG_SCHED_CLASS_EXT`, `CONFIG_UCLAMP_TASK`) and runtime sysctls for deadline periods
- Host-wide deadline bandwidth already admitted
- The host process table, when the target pid is resolved against it
- The scheduling effect on the host, which changes the timing of every later event

## Hermit design

Target policy: `model` (owner decision 7, 2026-10-04). The p3 plan in the next paragraphs was `emulate`; the Target paragraph at the end of this section replaces it. Detcore schedules guest threads itself, so the host policy of a guest thread has no effect worth preserving, and applying it would only add host timing noise. `handle_sched_setattr` (`detcore/src/syscalls/threads.rs:2318-2436`) therefore applies no change, but it keeps the kernel's answer for every argument whose answer does not depend on the host:

- F1: `flags != 0` or `pid < 0` is `EINVAL` (`detcore/src/syscalls/threads.rs:2327-2329`); a NULL `attr` is `EINVAL` (`:2330`).
- F3, F4, F6: the size is read with a fault mapped to `EFAULT` (`:2340-2343`), size 0 becomes VER0 and anything outside 48-4096 is refused (`sched_attr_effective_size`, `:288-298`). Every `E2BIG` writes 56 back into `attr->size` (`refuse_too_big`, `:2350-2353`).
- F5: an oversized buffer has its tail scanned (`:2368-2383`, `scan_tail_is_zeroed` at `:148`). A nonzero byte is `E2BIG` with the size written back, and a fault is `EFAULT`.
- F16 size rule and F11 sign bit: checked before the pid lookup (`validate_sched_attr_before_lookup`, `:320-333`).
- F2: the pid is resolved with `thread_is_live` against Detcore's own task table (`:2423-2425`), never against the host process table, so `ESRCH` depends only on guest state.
- F7-F9, F11-F13, F17: policy, flag, priority and deadline checks run after the lookup (`validate_sched_attr_after_lookup`, `:340-387`), with the policy set at `:235-245`, the flag mask at `:215-228` and the deadline rules at `:259-277`. `SCHED_FLAG_KEEP_POLICY` substitutes the virtual current policy, `SCHED_OTHER` (`:107-112`, `:358-359`).
- Success returns 0 with no effect (`:2430-2435`).

The handler's doc comment (`detcore/src/syscalls/threads.rs:2292-2312`) states the rule for the host-dependent cases: they are accepted as no-ops, because a deterministic sandbox must not vary with the host. That covers `EPERM` (F8, F18), `EOPNOTSUPP` for clamps (F16), the deadline period sysctls and admission (F9), and `SCHED_EXT` (F10), which is accepted unconditionally.

Why the p3 plan chose `emulate` and not the alternatives:

- `passthrough` lets the guest change host scheduling, and its answer depends on host privileges and configuration.
- `refuse` breaks `chrt`, thread pools that lower their own priority, and the sibling `sched_setscheduler`/`sched_setparam` handlers, which already accept well-formed requests as no-ops.
- `model` would keep a per-thread virtual policy that `sched_getattr` and `sched_getscheduler` read back. That is the open question below, shared with the `sched_setscheduler` body; today the read side reports a fixed `SCHED_OTHER`, so a guest that sets `SCHED_BATCH` and reads it back sees `SCHED_OTHER`, where Linux would report `SCHED_BATCH`.

Known gaps against Linux, all host-independent choices of the handler:

- `sched_nice` is not checked (F7). Linux clamps it rather than refusing, so the only difference would be the read-back, which the fixed answer of `sched_getattr` already hides.
- `sched_util_min`/`sched_util_max` values are not validated (F16). Linux refuses values above 1024 with `EINVAL` when clamping is configured; Hermit accepts them.
- The code tag `TODO-HUMAN-REVIEW(PR-841)` at `detcore/src/syscalls/threads.rs:2258` (https://github.com/rrnewton/hermit/pull/841) asks for human review of the no-op policy.

Target (owner decision 7, with decision 6): keep every check above in its present kernel order, and on success write the target thread's virtual scheduling record instead of discarding the request. The record is per thread, kept in Detcore's thread state next to `timer_slack_ns` (`detcore/src/tool_local.rs:1795`) and shared with `sched_setscheduler`, `sched_setparam` and the nice value `setpriority` writes. It stores the policy, `sched_priority`, nice, the deadline runtime, deadline and period, the reset-on-fork flag and the clamps, following `__setscheduler_params()`: nice only for fair policies, clamped to -20..19 as `sched_copy_attr()` does (F7, F20); `SCHED_FLAG_KEEP_POLICY` substitutes the recorded policy where `:358-359` substitutes `SCHED_OTHER` today (F12); `SCHED_FLAG_KEEP_PARAMS` leaves policy and parameters unchanged (F13). A fair thread's `sched_runtime` is ignored and reads back as 0, the behaviour before Linux 6.12, so the answer does not depend on the host kernel (F20). Clamp values are checked as Linux does on a kernel with `CONFIG_UCLAMP_TASK`, in `uclamp_validate()`'s place after the permission step (F19); the `EOPNOTSUPP` of a kernel without it is not reproduced, under the rule at `:2292-2312`. A fork or clone child copies the record (as the timer slack is copied at `detcore/src/lib.rs:1555`); with the reset-on-fork flag, a real-time or deadline child becomes `SCHED_OTHER` with priority 0, a negative nice becomes 0, the clamps reset and the child's flag is clear, and a fork from a `SCHED_DEADLINE` thread without the flag fails with `EAGAIN` (kernel `sched_fork()`, sched(7)) (F14). `execve` keeps the record. A real-time or deadline record sets the thread's virtual timer slack to 0, as `__setscheduler_params()` does; the `sched_setscheduler` body gives the rule. Nothing reaches the host or Detcore's scheduler. The privilege-dependent answers (F8, F9 admission, F10, F16 `EOPNOTSUPP`, F18) stay accepted under the rule at `detcore/src/syscalls/threads.rs:2292-2312`. Today success returns 0 and stores nothing (`:2430-2435`).

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F11, F16, F17): run the argument bracket native and under Hermit on ptrace and require identical transcripts: NULL `attr`, negative pid, nonzero `flags`, every boundary size (0, 1, 47, 48, 56, 57, 4096, 4097), zero and nonzero tails, policies 0-8 except 7, the sign bit, an undefined flag, clamps on a VER0 buffer, an absent pid combined with each later error, and an unmapped `attr`.
  Existing candidate: `sched_setattr_argument_handling_matches_the_host_kernel` at `hermit-cli/tests/sched_setattr_abi.rs:77`, fixture `tests/c/sched_setattr_abi.c:151-343`. It runs a ptrace arm and also a SaBRe arm when the SaBRe artifacts are built.
- T2 (covers F7, F12, F13, F14, F15): well-formed requests return 0 and leave the run deterministic under `--strict --verify` on ptrace: `SCHED_OTHER`, `SCHED_BATCH` and `SCHED_IDLE` with a 56-byte struct, and `SCHED_OTHER` combined with `SCHED_FLAG_KEEP_POLICY`, `SCHED_FLAG_KEEP_PARAMS`, `SCHED_FLAG_RESET_ON_FORK`, `SCHED_FLAG_RECLAIM` and `SCHED_FLAG_DL_OVERRUN`.
  Existing candidates: `scheduler_policy_setters_verify` at `hermit-cli/tests/sched_setattr_noop.rs:30` (ptrace only, `--strict --verify` over `tests/c/sched_setattr_{other,batch,idle}.c`), and the cells `c-programs/sched-setattr-batch`, `c-programs/sched-setattr-idle` and `c-programs/sched-setattr-other` (`tests/e2e/manifests/c-programs.yaml:7618`, `:7676`, `:7734`). The flag combinations are probed only natively-against-Hermit by T1's fixture (`tests/c/sched_setattr_abi.c:246-258`).
- T3 (covers F8, F9, F18): unprivileged `SCHED_FIFO`/`SCHED_RR` at priority 1-99 and a valid `SCHED_DEADLINE` request (runtime 100000, deadline 200000, period 200000) return 0 under Hermit on ptrace, identically across runs, while the invalid priority and deadline cases return `EINVAL`. Natively an unprivileged caller would get `EPERM`; the test asserts only Hermit's fixed answer.
  Existing partial coverage: `scheduler_policy_queries_are_deterministic` at `hermit-cli/tests/scheduler_policy_queries.rs:30` and cell `c-programs/scheduler-policy-queries` (`tests/e2e/manifests/c-programs.yaml:7864`), whose fixture sends the deadline request at `tests/c/scheduler_policy_queries.c:80-90`.
- T4 (covers F10): policy `SCHED_EXT` (7) with priority 0 returns 0 under Hermit on ptrace on every host, including hosts whose kernel lacks `CONFIG_SCHED_CLASS_EXT`.
  Related existing coverage: the unit test `sched_ext_is_a_valid_policy_and_sched_iso_is_not` at `detcore/src/syscalls/threads.rs:3130`, which checks the policy set without running a guest.
- T5 (covers F2): a `sched_setattr` aimed at the tid of a second live guest thread (a non-leader) returns 0, and one aimed at that tid after the thread has exited returns `ESRCH`.
- T6 (covers F16, F19): with a 56-byte struct and `SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_UTIL_CLAMP_MAX`: min 0 and max 1024 return 0; min 1025, max 2000, and min 600 with max 500 give `EINVAL`; min -1 with max 500 returns 0. A request with both a bad clamp and an absent pid gives `ESRCH`, and one with both a bad clamp and priority 5 under `SCHED_OTHER` gives `EINVAL` for the priority, showing the clamp check comes last. Today Hermit returns 0 for every bad clamp.
- T7 (covers F7, F12, F13, F14, F20): read-back under `hermit run --verify` on ptrace. After `SCHED_BATCH` with nice 7, `sched_getattr` reports policy 3 and nice 7 and `getpriority` reports 7; nice 40 reads back as 19. After `SCHED_FIFO` priority 10 with nice 5, it reports policy 1, priority 10 and the earlier nice. After a `SCHED_DEADLINE` request it reports the runtime, deadline and period given, and a `fork` from that thread gives `EAGAIN`. `SCHED_FLAG_KEEP_POLICY` with priority 20 on the `SCHED_FIFO` thread keeps policy 1 and reports 20; `SCHED_FLAG_KEEP_PARAMS` with `SCHED_OTHER` keeps policy 1. After `SCHED_FIFO | SCHED_FLAG_RESET_ON_FORK`, a forked child reports `SCHED_OTHER`, priority 0, no flag. A `SCHED_OTHER` request with `sched_runtime` 2000000 reads back runtime 0. Today Hermit reports `SCHED_OTHER` with zero fields after every call.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/417: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (kernel-order argument checks, pid resolved against Detcore's task table, well-formed requests a no-op, threads.rs:2318); linux_determinism stays conditional; 18 flag behaviours and 5 tests listed; open question for the owner on emulate versus a modelled per-thread policy with read-back; no coverage token recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (per-thread scheduling record with read-back, KEEP_POLICY and KEEP_PARAMS against the record, fork inheritance per sched_fork including the deadline EAGAIN, pre-6.12 fair sched_runtime; clamp EINVAL checked after the permission step); target_policy emulate -> model; new F19, F20, T6, T7; removed both open questions (emulate or model; clamp values above 1024)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_setattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_setattr&body=Discussion+for+the+%60sched_setattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_setattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_setattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sched_setattr" (https://github.com/rrnewton/hermit/issues/417), legacy variant A1.

### 2026-07-24 host strace evidence

This section is history. At hermit f4055eb9cae4 the call is dispatched by its own typed arm (`detcore/src/lib.rs:2683`), not by the old classification pass-through it describes.

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sched_setattr` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `sched_setattr` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
sched_setattr(0, {size=48, sched_policy=SCHED_OTHER, sched_flags=0, sched_nice=0, sched_priority=0, sched_runtime=0, sched_deadline=0, sched_period=0}, 0) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
