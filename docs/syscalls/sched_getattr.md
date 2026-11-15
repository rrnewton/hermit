---
schema: syscall-pages/v1
syscall: sched_getattr
nr_x86_64: 315
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/413
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_getattr&body=Discussion+for+the+%60sched_getattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_getattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_getattr%60%2C+or+in+what+the+page+says%3F%0A
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

`sched_getattr(pid, attr, size, flags)` reads a thread's scheduling policy and parameters into an extensible `struct sched_attr`. Detcore handles it (`detcore/src/lib.rs:2680`, `detcore/src/syscalls/threads.rs:2223`): after checking `flags`, `size` and `attr`, it writes a fixed 48-byte record that says `SCHED_OTHER` with every parameter zero, for any thread, and never asks the host.

Manual reference: Linux man-pages sched_getattr(2), sched(7).

## Linux semantics

The kernel first rejects, all with `EINVAL` from one test: `attr` NULL, `pid < 0`, `size` larger than one page, `size` smaller than 48 (`SCHED_ATTR_SIZE_VER0`), and nonzero `flags`. It then resolves `pid` (0 means the caller), returning `ESRCH` when no such thread exists; a security module can refuse with `EPERM`.

It fills a kernel `struct sched_attr` from the target thread: the policy; `SCHED_FLAG_RESET_ON_FORK` if set; the deadline runtime, deadline and period for `SCHED_DEADLINE`; the real-time priority for `SCHED_FIFO`/`SCHED_RR`; the nice value for the other policies; on recent kernels (6.12 and later) the custom time slice of a fair-class thread in `sched_runtime`; and, when the kernel has `CONFIG_UCLAMP_TASK`, the utilization clamps in `sched_util_min`/`sched_util_max`.

Finally it copies `min(size, 56)` bytes to user space, storing that copied length in the `size` field, so a caller with a 48-byte buffer sees `size=48` and one with a larger buffer sees `size=56`. A failed copy is `EFAULT`.

### Flags and options

- F1: nonzero `flags`: `EINVAL`.
- F2: `attr` NULL: `EINVAL`.
- F3: `pid < 0`: `EINVAL`.
- F4: `size` below 48 or above 4096: `EINVAL`.
- F5: `size` 48 to 55: the first `size` bytes are copied and `attr->size` is set to `size`. `size` 56 or more: 56 bytes are copied, including `sched_util_min`/`sched_util_max`, and `attr->size` is 56.
- F6: `pid` 0 (the caller) or the tid of another live thread; a pid that names no thread is `ESRCH`.
- F7: reported policy, priority and nice value: whatever the thread currently has, as set by `sched_setattr`, `sched_setscheduler`, `setpriority` or inheritance from the parent.
- F8: reported `sched_flags`: `SCHED_FLAG_RESET_ON_FORK` when the thread has it.
- F9: reported `SCHED_DEADLINE` parameters (runtime, deadline, period) for a deadline thread.
- F10: reported `sched_runtime` for a fair-class thread on recent kernels: its time slice, which defaults to a value derived from the host CPU count.
- F11: reported `sched_util_min`/`sched_util_max`: the thread's clamps when `CONFIG_UCLAMP_TASK` is enabled.
- F12: `attr` that cannot be written: `EFAULT`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The argument checks (F1-F4) are pure functions of the arguments. The answer for a valid call is host state: the policy and nice value inherited from whatever launched the program, the default slice derived from the host's CPU count (the 2026-07-24 strace below shows `sched_runtime=2800000` for a `SCHED_OTHER` thread), the kernel's clamp configuration, and whether the pid is live in the host process table. The call repeats when the thread's scheduling state is unchanged and the host is the same.

### Nondeterminism sources

- Policy and nice value inherited from the launching environment, for example a parent run under `chrt` or `nice`
- The default fair-class slice, which scales with the host CPU count, and the kernel version that decides whether it is reported
- Kernel build options (`CONFIG_UCLAMP_TASK`) and the host's default clamp values
- The host process table, when the target pid is resolved against it

## Hermit design

Target policy: `model` (owner decision 7, 2026-10-04). The p3 plan in the next paragraphs was `emulate` with one fixed record; the Target paragraph at the end of this section replaces it. Detcore's scheduler replaces Linux scheduling, so a guest thread's host attributes are inert, and the handler reports one fixed record instead. `handle_sched_getattr` (`detcore/src/syscalls/threads.rs:2223-2255`):

- returns `EINVAL` for nonzero `flags` (F1, `:2229-2231`), for `size` smaller than `sizeof(libc::sched_attr)`, which is 48 bytes (F4 lower bound, `:2233-2236`), and for a NULL `attr` (F2, `:2237`);
- builds a zeroed `sched_attr` with `size` 48 and policy `SCHED_OTHER` (`:2240-2242`), so priority, nice, flags, deadline parameters and slice all read 0 (F7-F10);
- writes those 48 bytes to `attr` (`:2249`); a failed write propagates the memory layer's errno, `EFAULT` for an unmapped buffer (F12);
- returns 0 and logs the pid, which it otherwise ignores (`:2250-2254`).

The fixed record matches the rest of Detcore's virtual scheduling state: `sched_setattr` accepts well-formed requests as no-ops (see the `sched_setattr` body), the virtual current policy is `SCHED_OTHER` (`detcore/src/syscalls/threads.rs:107-112`), and `getpriority` reports nice 0 for every thread (`getpriority_result`, `detcore/src/syscalls/misc.rs:150-156`).

Why the p3 plan chose `emulate` and not the alternatives:

- `passthrough` reports the host's slice, inherited policy and clamps, all of which vary with the host and the launcher.
- `refuse` breaks `chrt -p` and other scheduling queries; the handler's doc comment says it exists to re-enable `chrt` under `--strict` (`detcore/src/syscalls/threads.rs:2220`). The compat cell labelled `chrt` (`tests/e2e/manifests/compat.yaml:185`) runs `chrt -p $$`.
- `model`, a per-thread virtual policy that this call reads back, is the open question recorded in the `sched_setattr` body. It would change this handler and that one together.

Known gaps against Linux, each a guest-visible difference:

- **pid is ignored (F3, F6).** A negative pid succeeds instead of `EINVAL`, and a pid that names no thread succeeds instead of `ESRCH`. The `sched_setattr` handler already resolves pids against Detcore's task table with `thread_is_live` (`detcore/src/syscalls/threads.rs:2423-2425`); this handler could do the same.
- **size above one page is accepted (F4 upper bound).** Linux returns `EINVAL` for `size > 4096`.
- **The copy length and the size field are fixed (F5).** The handler always writes 48 bytes and stores `size=48`. Linux copies `min(size, 56)` bytes and stores that length, so a caller with a 56-byte buffer sees `size=56` and zeroed clamp fields natively, but `size=48` and untouched clamp fields under Hermit.
- The code tag `TODO-HUMAN-REVIEW(#791)` at `detcore/src/syscalls/threads.rs:2222` (https://github.com/rrnewton/hermit/pull/791) asks for human review of this handler.

Target (owner decision 7, with decision 6): report the target thread's virtual scheduling record, the per-thread state that `sched_setattr`, `sched_setscheduler`, `sched_setparam` and `setpriority` write (kept in Detcore's thread state next to `timer_slack_ns`, `detcore/src/tool_local.rs:1795`), and close the three gaps above, in the kernel's order (sched_getattr(2); kernel `sys_sched_getattr()`): first `EINVAL` for a NULL `attr`, a negative `pid`, a `size` above 4096 or below 48, or nonzero `flags` (F1-F4); then `ESRCH` when `pid` names no live guest thread, from `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`) (F6); then fill the record as `get_params()` does: the policy, `SCHED_FLAG_RESET_ON_FORK` when set (F8), the deadline runtime, deadline and period for `SCHED_DEADLINE` (F9), `sched_priority` for `SCHED_FIFO` and `SCHED_RR`, and nice for the other policies (F7), plus the recorded clamps, which start at 0 and 1024 (F11); last, copy `min(size, 56)` bytes with `attr->size` set to that length, where a failed copy is `EFAULT` (F5, F12; kernel `sched_attr_copy_to_user()`). A fair thread's `sched_runtime` reads 0, the behaviour before Linux 6.12, so the slice no longer reflects the host CPU count (F10). A thread that never set anything reads `SCHED_OTHER` with zero parameters and clamps 0 and 1024, whatever the launcher's policy. Today the handler ignores `pid` and writes a fixed 48-byte record (`detcore/src/syscalls/threads.rs:2240-2249`).

## Recommended tests

- T1 (covers F1, F2, F3, F4): with raw `syscall(SYS_sched_getattr, ...)`, check `EINVAL` for nonzero `flags`, a NULL `attr`, `pid` -1, `size` 47 and `size` 4097. Compare against native, and expect the pid and large-size cases to differ until the gaps above are fixed.
  No test file at this SHA names `sched_getattr`, so T1-T4 have no existing candidate.
- T2 (covers F5, F7, F8, F9, F10, F11): with poisoned 48-byte and 64-byte buffers, call for pid 0 and check the record under `hermit run --verify` on ptrace: policy `SCHED_OTHER`, every parameter 0, the bytes past the copied length untouched, and the `size` field. Assert Linux's rule (48 for the small buffer, 56 for the large one), so the test records the F5 gap until it is fixed. Repeat after `sched_setattr` sets `SCHED_BATCH` with `SCHED_FLAG_RESET_ON_FORK`, and after it sets a valid `SCHED_DEADLINE` request: under the target the answer reports what was set (T6).
- T3 (covers F6): call for the tid of a second live guest thread, which must succeed, and for that tid after the thread has exited, which Linux answers with `ESRCH`.
- T4 (covers F12): call with `attr` pointing at an unmapped page and expect `EFAULT`.
- T5 (covers F7): run `chrt -p $$` under `--strict --verify` on ptrace and require identical output across runs.
  Existing candidate: the compat cell labelled `chrt` at `tests/e2e/manifests/compat.yaml:185`. This body did not confirm which `chrt` versions call `sched_getattr` rather than `sched_getscheduler` and `sched_getparam`.
- T6 (covers F5, F7, F8, F9, F10, F11): read-back under `hermit run --verify` on ptrace with a 56-byte buffer. A fresh thread reads `SCHED_OTHER`, zero parameters and clamps 0 and 1024. After `sched_setattr` sets `SCHED_BATCH` with nice 7 and `SCHED_FLAG_RESET_ON_FORK`, it reads policy 3, nice 7 and the flag; a forked child reads `SCHED_OTHER`, nice 7, no flag. After `SCHED_FIFO` priority 10 it reads priority 10 and nice 0; after `SCHED_DEADLINE` (100000, 200000, 200000) it reads those three values; after clamps 100 and 900 it reads 100 and 900. A second thread reading the first thread's TID sees the same record. A `SCHED_OTHER` request with `sched_runtime` 2000000 reads back runtime 0. Today Hermit reads `SCHED_OTHER` with zero fields every time.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/413: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (fixed 48-byte SCHED_OTHER record with zero parameters, threads.rs:2223); linux_determinism stays conditional; 12 flag behaviours and 5 tests listed; gaps recorded (pid ignored, size > 4096 accepted, copy length and size field fixed at 48); open question on emulate versus a modelled per-thread policy, shared with sched_setattr; no coverage token recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (report the per-thread scheduling record as get_params does, fair sched_runtime 0 as before Linux 6.12; Linux order EINVAL incl. size > 4096, then ESRCH, then a min(size, 56) copy); target_policy emulate -> model; new T6; T2's stays-SCHED_OTHER clause replaced by T6; removed both open questions (emulate or model; fix the three gaps)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_getattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_getattr&body=Discussion+for+the+%60sched_getattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_getattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_getattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sched_getattr" (https://github.com/rrnewton/hermit/issues/413), legacy variant A1.

### 2026-07-24 host strace evidence

This section is history. At hermit f4055eb9cae4 the call is dispatched by its own typed arm (`detcore/src/lib.rs:2680`), not by the old classification pass-through it describes.

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sched_getattr` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `sched_getattr` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
sched_getattr(0, {size=48, sched_policy=SCHED_OTHER, sched_flags=0, sched_nice=0, sched_priority=0, sched_runtime=2800000, sched_deadline=0, sched_period=0}, 48, 0) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
