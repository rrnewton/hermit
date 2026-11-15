---
schema: syscall-pages/v1
syscall: sched_rr_get_interval
nr_x86_64: 148
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/416
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_rr_get_interval&body=Discussion+for+the+%60sched_rr_get_interval%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_rr_get_interval.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_rr_get_interval%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

Writes a thread's round-robin time quantum into a `struct timespec`. Hermit writes a fixed zero interval without contacting the kernel. The target keeps a fixed, host-independent interval and adds Linux's pid and pointer validation.

## Linux semantics

`sched_rr_get_interval(pid, tp)` in Linux 6.9 (kernel/sched/core.c):

- `pid < 0` gives EINVAL.
- `pid` 0 means the calling thread. Otherwise the kernel looks up any live task with that thread id, and an unknown id gives ESRCH.
- The scheduling class of the target then computes the interval, which is copied to `tp`. A NULL or unwritable `tp` gives EFAULT. That is checked after the pid lookup, so a bad pid wins over a bad pointer.
- The value depends on the target's policy:
  - SCHED_RR: `sched_rr_timeslice`, 100 ms by default. It can be changed with the `kernel.sched_rr_timeslice_ms` sysctl and is rounded to the host's HZ.
  - SCHED_FIFO: 0.
  - Fair policies (OTHER, BATCH, IDLE): 0 if the runqueue's fair load is zero. Otherwise it is the task's slice in jiffies, which depends on HZ, the CPU count scaling of the base slice, and load.
  - SCHED_DEADLINE: 0.

### Flags and options

- F1: `pid` selection. 0 is the calling thread; a positive value names any live thread; a negative value
  gives EINVAL; an id that names no live task gives ESRCH.
- F2: the `tp` output pointer. Linux writes a `struct timespec`. A NULL or unwritable pointer gives EFAULT,
  checked only after the pid has been resolved.
- F3: the interval value, which depends on the target's policy: the host-configured RR timeslice for
  SCHED_RR; 0 for SCHED_FIFO and SCHED_DEADLINE; and for fair policies a value derived from host HZ, CPU count
  and current runqueue load, or 0 on an idle runqueue.

## Determinism analysis

For the usual SCHED_OTHER caller the fair-class value depends on host HZ, on the CPU count (through the scaling of the base slice), on a runtime-tunable base slice, and on whether the runqueue carries load at that instant. The 2026-07-24 host trace below observed 2 ms. A SCHED_RR caller sees a sysctl-tunable value. The same program can therefore read different intervals on different hosts and even on consecutive calls. linux_determinism stays nondeterministic.

### Nondeterminism sources

- Host HZ, CPU count, `kernel.sched_rr_timeslice_ms` and the fair base slice setting (F3).
- Instantaneous runqueue load for fair-class threads (F3).
- The host task table, for ESRCH (F1).

## Hermit design

**What the code does today (hermit f4055eb9cae4).**

- detcore/src/lib.rs:2657 dispatches to `handle_sched_rr_get_interval` at detcore/src/syscalls/threads.rs:2195-2214. Its doc comment says the round-robin quantum belongs to the Linux scheduler, which Detcore does not use, so it reports a fixed zero interval.
- When `tp` is non-NULL it writes `{tv_sec: 0, tv_nsec: 0}` and returns 0. The write failure from `write_value` propagates through `?` (detcore/src/syscalls/threads.rs:2206-2213).
- The call is listed as determinized at detcore/src/syscall_classification.rs:338, under the determinism argument at detcore/src/syscall_classification.rs:318-325.
- Nothing reaches the kernel, so the host's quantum never leaks.

Zero is an answer Linux itself gives for a SCHED_OTHER thread on an idle runqueue. It agrees with the fixed SCHED_OTHER that `sched_getscheduler` reports (detcore/src/lib.rs:2655), so the value is plausible and host-independent.

**Gaps.**

- `pid` is never examined: `pid = -1` returns 0 where Linux gives EINVAL, and a thread id that names no task returns 0 where Linux gives ESRCH.
- A NULL `tp` returns 0 where Linux gives EFAULT. The `if let Some(tp)` at detcore/src/syscalls/threads.rs:2206 skips the write.

**p3 plan: emulate** (the Target paragraph at the end of this section replaces it under owner decision 7). Keep the fixed interval, and add Linux's checks in kernel order:

1. `pid < 0` gives EINVAL.
2. A nonzero `pid` that names no live Detcore thread gives ESRCH. Use the scheduler's own task table, as `handle_sched_setattr` does at detcore/src/syscalls/threads.rs:2423 with `thread_is_live`.
3. A NULL `tp` gives EFAULT.
4. Write the interval.

Why the p3 plan ruled out the alternatives:

- **Not passthrough.** The host value is exactly the leak described above.
- **Not refuse.** The call is harmless and a fixed answer is faithful to the virtual SCHED_OTHER policy.
- **Not model.** If Hermit later models a per-thread virtual policy (the open question in the sched_setscheduler body), the interval could follow that policy instead, for example a fixed 100 ms for a virtual SCHED_RR thread. Owner decision 7 (2026-10-04) chose that per-thread model.

Target (owner decision 7, with decision 6): the interval follows the target thread's virtual scheduling record, the per-thread policy that `sched_setscheduler` and `sched_setattr` write (kept in Detcore's thread state next to `timer_slack_ns`, `detcore/src/tool_local.rs:1795`): exactly 100 ms for `SCHED_RR`, Linux's default `RR_TIMESLICE` (sched_rr_get_interval(2), `/proc/sys/kernel/sched_rr_timeslice_ms`), and 0 for `SCHED_FIFO`, `SCHED_DEADLINE` and every fair policy, the fair answer being Linux's answer on an idle runqueue (F3). The host's sysctl, HZ and load never reach the guest. The checks follow the kernel's order (kernel `sched_rr_get_interval()`): `pid < 0` gives EINVAL, then a `pid` with no live guest thread gives ESRCH from `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`), then the interval is computed and copied out, where a NULL or unwritable `tp` gives EFAULT (F1, F2). Today the handler writes zero without reading `pid` and returns 0 for a NULL `tp` (`detcore/src/syscalls/threads.rs:2206`).

## Recommended tests

- T1 (covers F1): under `hermit run --verify` on ptrace: `sched_rr_get_interval(-1, &ts)` gives EINVAL; a
  thread id that never existed gives ESRCH; `sched_rr_get_interval(0, &ts)` and the tid of a live
  non-leader thread return 0.
- T2 (covers F2): `sched_rr_get_interval(0, NULL)` gives EFAULT; a non-NULL unmapped `tp` gives EFAULT;
  and a nonexistent pid with a NULL `tp` gives ESRCH, because the pid lookup comes first.
- T3 (covers F3): the guest prints the interval it reads. The value must equal the documented
  fixed constant (zero today) and be byte-identical across `--verify` runs. Also run it on a host where the
  native call returns a nonzero value, such as the 2 ms in the trace below, to show that no host value leaks.
- T4 (covers F1, F3): after `sched_setscheduler(0, SCHED_RR, {1})`, `sched_rr_get_interval(0, &ts)` reads
  exactly 0 s and 100000000 ns, and so does a sibling thread asking for that TID and a child forked from it;
  after switching to `SCHED_FIFO` it reads 0, and a thread that never changed policy reads 0. Output is
  identical across both runs of `hermit run --verify` on ptrace. Today Hermit reads 0 in every case.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/416: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (fixed host-independent interval per threads.rs:2195-2214, plus missing pid EINVAL/ESRCH and NULL-tp EFAULT validation); linux_determinism nondeterministic kept; 3 flag behaviours and 3 tests; left todo with an owner question on whether the constant should follow a modelled policy.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (interval follows the per-thread virtual policy: 100 ms for SCHED_RR, 0 otherwise; pid EINVAL, ESRCH, then EFAULT on the copy-out); target_policy emulate -> model; new T4; removed the open question on following a modelled policy

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_rr_get_interval`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_rr_get_interval&body=Discussion+for+the+%60sched_rr_get_interval%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_rr_get_interval.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_rr_get_interval%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sched_rr_get_interval" (https://github.com/rrnewton/hermit/issues/416), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sched_rr_get_interval` on a direct `syscall()` harness, invoked with arguments that query, do nothing, or fail, and on common programs where applicable. At that time the call had not yet been classified. At hermit f4055eb9cae4 it is determinized (detcore/src/syscall_classification.rs:338).

Observed strace line:
```
sched_rr_get_interval(0, {tv_sec=0, tv_nsec=2000000}) = 0
```

---
*Autonomous syscall-classification sweep of the calls that were then awaiting classification in `syscall_classification.rs`. Host testing + analysis only; no code change proposed here.*
