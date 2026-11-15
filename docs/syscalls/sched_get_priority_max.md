---
schema: syscall-pages/v1
syscall: sched_get_priority_max
nr_x86_64: 146
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/411
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_get_priority_max&body=Discussion+for+the+%60sched_get_priority_max%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_get_priority_max.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_get_priority_max%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 1
---

## Summary

`sched_get_priority_max(policy)` returns the maximum static priority that `sched_setscheduler` and `sched_setparam` accept for a scheduling policy: 99 for `SCHED_FIFO` and `SCHED_RR`, 0 for the other policies, `EINVAL` for an unknown policy. Detcore passes it to the kernel unchanged (`detcore/src/lib.rs:2843`).

## Linux semantics

The kernel answers from a fixed switch on `policy`. It reads no task, no memory and no host state. The only thing that varies is which policy numbers the running kernel knows: `SCHED_EXT` (7) is known from Linux 6.12 (from upstream history, not probed on an older kernel here).

Host results measured for this issue on Linux 7.1.3: policies 0, 3, 5, 6 and 7 return 0; policies 1 and 2 return 99; -1, 4, 8, `0x40000000` and `0x40000001` return `EINVAL`.

### Flags and options

- F1: `SCHED_OTHER` (0), `SCHED_BATCH` (3) and `SCHED_IDLE` (5): return 0
- F2: `SCHED_FIFO` (1) and `SCHED_RR` (2): return 99
- F3: `SCHED_DEADLINE` (6): returns 0; deadline tasks carry no static priority
- F4: `SCHED_EXT` (7): returns 0 on a kernel that knows the policy (6.12 and later)
- F5: any other value, including the reserved 4 (`SCHED_ISO`), negative values and values above 7: `EINVAL`
- F6: a valid policy with `SCHED_RESET_ON_FORK` (`0x40000000`) OR'd in: `EINVAL`; only `sched_setscheduler` accepts that bit
- F7: `SCHED_EXT` (7) on a kernel older than 6.12, which does not know the policy: `EINVAL`

## Determinism analysis

`linux_determinism: deterministic`. The result is a constant for each policy on a given kernel, and the call has no side effect. It changes only if the kernel version changes between runs, and then only for `SCHED_EXT` (F4, F7).

### Nondeterminism sources

- None at run time. The host kernel version decides between F4 and F7.

### Cross-host variation

- Host kernel version: whether policy 7 (`SCHED_EXT`) returns 0 (F4) or `EINVAL` (F7); kernels before 6.12 do not know the policy, and Hermit forwards the kernel's answer unchanged.

## Hermit design

Target policy: `passthrough`. This matches the code at hermit f4055eb9cae4.

- `classify_syscall` returns `PassThrough` for `sched_get_priority_max` in the "ISSUE-REVIEWED PASS-THROUGH SYSCALLS" block (`detcore/src/syscall_classification.rs:824`). The block's comment at `detcore/src/syscall_classification.rs:808` says these remain conditional on Hermit's fixed-container assumptions, and `detcore/src/syscall_classification.rs:811` still carries `TODO-HUMAN-REVIEW(PR-643)`. The unit test `representative_policies_stay_in_their_reviewed_sections` pins it (`detcore/src/syscall_classification.rs:2113`).
- Dispatch is the generic arm at `detcore/src/lib.rs:2843`, which calls `passthrough` (`detcore/src/lib.rs:367`). Under record and replay, `passthru_opt` (`detcore/src/lib.rs:3140`) leaves it unsubscribed (`detcore/src/lib.rs:1084`).

Passing through is enough because the answer is a kernel constant. The kernel's answers also agree with Detcore's own scheduling model on a current kernel:

- Detcore's `sched_setattr` validation uses `MAX_RT_PRIO = 100` with real-time priorities 1 to 99 (`detcore/src/syscalls/threads.rs:202`, enforced at `detcore/src/syscalls/threads.rs:373`).
- Detcore reports every thread as `SCHED_OTHER` (`detcore/src/lib.rs:2655`) and accepts `sched_setscheduler` without applying it (`detcore/src/lib.rs:2652`); the "Batch 3" unit-test loop pins those as determinized (`detcore/src/syscall_classification.rs:2121`).

### Gaps and open points

- `SCHED_EXT` (F4): Detcore's `is_valid_sched_policy` accepts policy 7 on every host (`detcore/src/syscalls/threads.rs:235`); its doc comment (`detcore/src/syscalls/threads.rs:230`) says the kernel accepts it only with `CONFIG_SCHED_CLASS_EXT`. On a host kernel older than 6.12, `sched_get_priority_max(7)` would return `EINVAL` (F7) while Detcore's `sched_setattr` validation accepts the same policy. Owner decision 2 (2026-10-04) keeps the pass-through rather than a fixed table, and the difference is recorded under Cross-host variation.

## Recommended tests

- T1 (covers F1, F2, F3): raw `syscall(SYS_sched_get_priority_max, p)` for p in 0, 1, 2, 3, 5 and 6 under `hermit run --verify` on ptrace; print each result and require the values above.
- T2 (covers F4): `sched_get_priority_max(7)` returns 0 on the CI host and the result is identical across runs; the harness records the host kernel release (`uname -r` outside Hermit, since the guest's `uname` reports `5.2.0`, `detcore/src/syscalls/misc.rs:541`) next to the result, so a host change is visible.
- T3 (covers F5, F6): -1, 4, 8, `INT_MAX`, `SCHED_FIFO | SCHED_RESET_ON_FORK` and `SCHED_OTHER | SCHED_RESET_ON_FORK` all return `EINVAL`.
- U1 (covers F7; host-config): the CI host runs a kernel that knows `SCHED_EXT`, so CI cannot produce the `EINVAL` that kernels before 6.12 return for policy 7. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:824`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/411: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism deterministic and target_policy passthrough kept (a kernel constant per policy, agreeing with Detcore's MAX_RT_PRIO at threads.rs:202); 6 flag behaviours and 3 tests; open question for the owner whether to answer from a fixed table so SCHED_EXT matches is_valid_sched_policy on pre-6.12 hosts, tracking_status todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host kernel version: SCHED_EXT known from 6.12); F4 narrowed to kernels that know SCHED_EXT and new F7 for the pre-6.12 EINVAL, covered by host-config U1; the open question on a fixed table versus pass-through is resolved by decision 2 (keep the pass-through, record the cross-host difference)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_get_priority_max`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_get_priority_max&body=Discussion+for+the+%60sched_get_priority_max%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_get_priority_max.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_get_priority_max%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sched_get_priority_max" (https://github.com/rrnewton/hermit/issues/411), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sched_get_priority_max` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `sched_get_priority_max` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
sched_get_priority_max(SCHED_OTHER) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
