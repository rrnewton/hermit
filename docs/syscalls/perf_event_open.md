---
schema: syscall-pages/v1
syscall: perf_event_open
nr_x86_64: 298
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/316
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+perf_event_open&body=Discussion+for+the+%60perf_event_open%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fperf_event_open.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60perf_event_open%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_perf_event_enosys_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 6
---

## Summary

`perf_event_open(attr, pid, cpu, group_fd, flags)` opens a hardware or software performance-monitoring event described by `struct perf_event_attr` and returns a file descriptor through which the counter is read, sampled or controlled. Hermit refuses it: Detcore returns a fixed `ENOSYS` for every guest call without reading the arguments or consulting the host (`detcore/src/lib.rs:2182`), which is the result a kernel built without performance-event support gives.

Manual reference: Linux man-pages perf_event_open(2); `include/uapi/linux/perf_event.h`.

## Linux semantics

On a kernel built with `CONFIG_PERF_EVENTS`, the call copies in `attr`, validates it against the target (`pid`, `cpu`), the group (`group_fd`) and `flags`, checks permissions, and on success returns a new file descriptor. That descriptor is the whole interface to the event: `read(2)` returns counter values in the layout `attr.read_format` selects, `ioctl(2)` enables, disables, resets or reconfigures it, `mmap(2)` maps a ring buffer of samples, and `poll(2)` or `fcntl(F_SETOWN/F_SETSIG)` deliver overflow notifications. On a kernel built without `CONFIG_PERF_EVENTS` the syscall is not implemented and returns `ENOSYS` for every argument shape.

Permission depends on host policy: `/proc/sys/kernel/perf_event_paranoid`, `CAP_PERFMON` (or `CAP_SYS_ADMIN` on older kernels), Linux security modules and seccomp profiles, several of which return `EACCES` or `EPERM`. Availability of each event type depends on the host CPU, the hypervisor (many virtual machines expose no hardware PMU) and the kernel's configured PMUs.

Verified on the host (2026-07-24, see Additional information): a NULL `attr` pointer fails with `EFAULT`.

### Flags and options

- F1: `attr` pointer: an unreadable pointer, including NULL, gives `EFAULT`.
- F2: `attr.size` ABI versioning: 0 is taken as `PERF_ATTR_SIZE_VER0` (64 bytes). A size below that, or a larger size whose bytes past the kernel's structure are not all zero, gives `E2BIG`, and the kernel writes the size it supports back into `attr.size` in guest memory.
- F3: `attr.type = PERF_TYPE_HARDWARE`: generalized hardware events (`PERF_COUNT_HW_CPU_CYCLES`, `PERF_COUNT_HW_INSTRUCTIONS`, cache and branch events and others) counted by the CPU's PMU. An event the hardware does not support gives `ENOENT` or `EOPNOTSUPP`.
- F4: `attr.type = PERF_TYPE_SOFTWARE`: kernel software counters (`PERF_COUNT_SW_CPU_CLOCK`, `PERF_COUNT_SW_TASK_CLOCK`, page faults, context switches, CPU migrations, alignment and emulation faults, `PERF_COUNT_SW_DUMMY`, `PERF_COUNT_SW_BPF_OUTPUT`, `PERF_COUNT_SW_CGROUP_SWITCHES`).
- F5: `attr.type = PERF_TYPE_TRACEPOINT`: `attr.config` is a tracepoint id read from tracefs; access to tracepoint data is gated by `perf_event_paranoid` and capabilities.
- F6: `attr.type = PERF_TYPE_HW_CACHE`: cache events, with `attr.config` encoding cache level, operation and result.
- F7: `attr.type = PERF_TYPE_RAW` and dynamic PMU types (the number in `/sys/bus/event_source/devices/<pmu>/type`): model-specific raw events, for example the retired-conditional-branch event that `tests/util/pmu_skid.c` and Hermit's own scheduler use.
- F8: `attr.type = PERF_TYPE_BREAKPOINT`: a hardware breakpoint or watchpoint at `attr.bp_addr` with `attr.bp_type` and `attr.bp_len`. Exhausting the debug registers gives `ENOSPC`.
- F9: invalid event description: an unknown `attr.type` gives `ENOENT`; an invalid `attr.config`, reserved bits set, or an attribute combination the PMU does not support gives `EINVAL` or `EOPNOTSUPP`.
- F10: sampling and overflow attributes: `sample_period` or `sample_freq` with `freq`, `sample_type`, `wakeup_events` or `wakeup_watermark` with `watermark`, `precise_ip` (skid constraint; unsupported levels give `EOPNOTSUPP`), `sample_max_stack` (too large gives `EOVERFLOW`), `sigtrap` with `sig_data`.
- F11: counting-control attributes: `disabled`, `inherit`, `inherit_stat`, `pinned`, `exclusive` (a busy PMU gives `EBUSY`), `enable_on_exec`, `remove_on_exec`, the `exclude_*` bits (user, kernel, hv, idle, host, guest, callchain), `read_format`, and the record bits (`mmap`, `comm`, `task` and others) that add side-band records to the ring buffer.
- F12: the returned descriptor's follow-on interfaces: `read` of counter values, `ioctl` (`PERF_EVENT_IOC_ENABLE`, `_DISABLE`, `_RESET`, `_REFRESH`, `_PERIOD`, `_SET_OUTPUT`, `_SET_FILTER`, `_ID`, `_SET_BPF`, `_PAUSE_OUTPUT`, `_MODIFY_ATTRIBUTES`), `mmap` of the sample ring buffer, `poll`, and `fcntl(F_SETOWN/F_SETSIG)` overflow signals.
- F13: target selection by `pid` and `cpu`: `pid == 0` is the calling thread and `pid > 0` another thread, which needs ptrace-attach permission (`EACCES` or `EPERM`) and must exist (`ESRCH`); `cpu == -1` follows the thread on any CPU and `cpu >= 0` counts only on that CPU (an invalid or offline CPU gives `EINVAL` or `ENODEV`); `pid == -1` with `cpu >= 0` counts every process on that CPU and needs privilege; `pid == -1` with `cpu == -1` gives `EINVAL`.
- F14: `group_fd`: -1 makes the new event a group leader; the descriptor of an existing leader adds the event to that group so the members are scheduled together; a descriptor that is not open gives `EBADF`, and one that is not a valid leader for this event gives `EINVAL`.
- F15: `PERF_FLAG_FD_CLOEXEC`: the new descriptor has close-on-exec set.
- F16: `PERF_FLAG_FD_NO_GROUP` and `PERF_FLAG_FD_OUTPUT`: create the event without joining `group_fd`'s group, or route its output to the group leader's ring buffer. The man page documents `PERF_FLAG_FD_OUTPUT` as broken since Linux 2.6.35 in favour of `PERF_EVENT_IOC_SET_OUTPUT`.
- F17: `PERF_FLAG_PID_CGROUP`: `pid` is a descriptor of a cgroup directory and the event counts that cgroup; valid only in per-CPU mode (`cpu >= 0`).
- F18: any other bit in `flags` gives `EINVAL`.
- F19: host policy and resources: `perf_event_paranoid`, `CAP_PERFMON`/`CAP_SYS_ADMIN`, LSM and seccomp decide `EACCES`/`EPERM`; a kernel without `CONFIG_PERF_EVENTS` gives `ENOSYS`; the descriptor limit gives `EMFILE`; `EINTR` can interrupt the call.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. A successful call exists to report real measurements. Hardware counts (cycles, instructions, cache misses) and software counts (clock time, page faults, context switches, migrations) vary from run to run, and so do sample contents, the timing of overflow signals and ring-buffer wakeups. Whether the call succeeds at all depends on host state the guest does not control: the CPU and hypervisor, the kernel configuration, `perf_event_paranoid`, capabilities, seccomp, and how many hardware counters and debug registers other users already hold. Only the pure argument-validation failures (F1, F2, F9, F18) repeat, and even their order relative to permission checks depends on the host kernel version.

### Nondeterminism sources

- Counter values and samples read from the returned descriptor, which measure real hardware and kernel activity
- Overflow signal and wakeup timing, which follow host execution rather than guest logical time
- Host policy and hardware that decide success or the error code: PMU presence, virtualization, `perf_event_paranoid`, capabilities, seccomp, counter and debug-register contention
- A guest event on the same thread competes with Hermit's own use of the PMU. Hermit uses a hardware counter for its deterministic preemption, and reserves the overflow signal `reverie::PERF_EVENT_SIGNAL`, which Detcore strips from guest signal masks (`detcore/src/syscalls/signal.rs:126`).

## Hermit design

Target policy: `refuse`, with a fixed `ENOSYS`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` lists `perf_event_open` as Determinized (`detcore/src/syscall_classification.rs:422`), and the predicate `is_perf_event_enosys_syscall` matches exactly this syscall (`detcore/src/syscall_classification.rs:1174`). Its doc comment (`detcore/src/syscall_classification.rs:1168`) states the reason: the result would depend on host PMU hardware, kernel configuration, `perf_event_paranoid` and capabilities, and Detcore does not model counter state, overflow delivery or perf-event descriptors.
- The dispatch arm in `handle_syscall_event` returns `Err(Errno::ENOSYS)` (`detcore/src/lib.rs:2182`). It neither reads `attr` nor forwards the call, so no descriptor is created and the guest's descriptor numbering is unchanged. The arm is not conditioned on the fail-closed policy, unlike the neighbouring `rseq` arm (`detcore/src/lib.rs:2031`), which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1330`), which other backends consult to enforce the same refusal (`detcore/src/lib.rs:217`).
- Unit tests pin membership only: `perf_event_open_is_deterministically_unavailable` (`detcore/src/syscall_classification.rs:2632`) and the refusal-aggregate test (`detcore/src/syscall_classification.rs:2720`). They do not exercise the arm's result; the e2e programs under Recommended tests do.

Consequence for every flag: F1 to F19 all return `ENOSYS` under Hermit. This matches a real Linux configuration (a kernel without `CONFIG_PERF_EVENTS`) rather than the host's, so a guest that probes for perf support takes its "unsupported" path. It differs from the host for well-formed requests that would succeed, and also for malformed ones: a NULL `attr` gets `ENOSYS`, not the `EFAULT` the host gives (F1), and `attr.size` is never written back (F2). `ENOSYS` is the right error rather than `EACCES`/`EPERM`, because tools such as perf treat those as a fixable permission problem and suggest changing `perf_event_paranoid`.

Why `refuse` and not the alternatives:

- `passthrough` would give the guest real counter values, real overflow signals and a real descriptor whose reads break determinism, and a guest event on the same thread would contend with Hermit's own preemption counter.
- `model` would need Detcore to own a perf-event descriptor type and synthesize counter values, `read_format` layouts, ioctls, ring buffers and overflow signals from logical time. A subset is conceivable (see Future work), but no current guest workload is known to need it.
- `emulate` with a fake descriptor would hand back an object whose later `read`, `ioctl` and `mmap` calls Detcore cannot service.

The code carries `TODO-HUMAN-REVIEW(PR-876)` markers on the arm (`detcore/src/lib.rs:2178`) and on the classification entry (`detcore/src/syscall_classification.rs:420`). They ask for a human review of this boundary; they do not report a known defect.

`tests/util/pmu_skid.c`, one of the files the facts count as naming this syscall, is a host diagnostic. Under Hermit it never reaches `perf_event_open`: its child's `PTRACE_TRACEME` is refused first and the program exits with status 3 (`tests/util/pmu_skid.c:400`, expected by `tests/e2e/manifests/util-c.yaml:31`). It is not evidence for this syscall.

Every test under Recommended tests runs under `hermit run --verify` on ptrace and asserts `ENOSYS` with no descriptor created: the next `open` returns the lowest free descriptor.

## Recommended tests

- T1 (covers F4): software events `PERF_COUNT_SW_CPU_CLOCK` and `PERF_COUNT_SW_TASK_CLOCK` with `pid = 0`, `cpu = -1`, `group_fd = -1`, `flags = 0` return `ENOSYS`.
  Existing candidates: `tests/c/perf_event_open_enosys.c` (cell `c-programs/perf-event-open-enosys`, `tests/e2e/manifests/c-programs.yaml:4844`, verify ci on ptrace at line 4863) and `tests/c/perf_event_software_enosys.c` (cell `c-programs/perf-event-software-enosys`, `tests/e2e/manifests/c-programs.yaml:4911`, ptrace at line 4930).
- T2 (covers F3, F11): a hardware `PERF_COUNT_HW_INSTRUCTIONS` event with `disabled`, `exclude_kernel` and `exclude_hv` returns `ENOSYS`.
  Existing candidate: `tests/c/perf_event_hardware_enosys.c` (cell `c-programs/perf-event-hardware-enosys`, `tests/e2e/manifests/c-programs.yaml:4777`, ptrace at line 4796). It sets only three F11 bits; a full T2 also covers `inherit`, `pinned`, `exclusive`, `enable_on_exec` and `read_format`.
- T3 (covers F8): a `PERF_TYPE_BREAKPOINT` write watchpoint on a local variable returns `ENOSYS`.
  Existing candidate: `tests/c/perf_event_watchpoint_enosys.c` (cell `c-programs/perf-event-watchpoint-enosys`, `tests/e2e/manifests/c-programs.yaml:4978`, ptrace at line 4997).
- T4 (covers F1, F2): `attr = NULL`, `attr` pointing at an unmapped page, `attr.size = 1`, and `attr.size = 4096` with nonzero trailing bytes all return `ENOSYS`, not `EFAULT` or `E2BIG`, and `attr.size` in guest memory is unchanged afterwards.
- T5 (covers F5, F6, F7, F9): tracepoint, hardware-cache, raw (`PERF_TYPE_RAW` with the RCB event) and a dynamic PMU type read from sysfs, plus an unknown type `0xffff` and a reserved attribute bit, all return `ENOSYS`.
- T6 (covers F10, F12): a sampling event (`sample_period`, `sample_type`, `wakeup_events = 1`, `precise_ip = 2`, `sigtrap`) returns `ENOSYS`. Because no descriptor exists, F12 is unreachable: assert that a `read`, `ioctl(PERF_EVENT_IOC_ENABLE)` and `mmap` on the descriptor number the host would have returned give `EBADF`, and that no `reverie::PERF_EVENT_SIGNAL` or `SIGIO` reaches the guest.
- T7 (covers F13, F14): target and group combinations return `ENOSYS`: `pid` of a child process, a nonexistent `pid`, `pid = -1` with `cpu = 0`, `pid = -1` with `cpu = -1`, `cpu = 100000`, `group_fd` of a regular file, and `group_fd = 999`.
- T8 (covers F15, F16, F17, F18): each of `PERF_FLAG_FD_CLOEXEC`, `PERF_FLAG_FD_NO_GROUP`, `PERF_FLAG_FD_OUTPUT`, `PERF_FLAG_PID_CGROUP` (with a cgroup directory descriptor and `cpu = 0`) and an undefined bit `1 << 10` returns `ENOSYS`.
- T9 (covers F19): the guest result is the same `ENOSYS` with and without the compatibility opt-out from the fail-closed policy, and on a host where a native run of the same program succeeds (record the native result next to Hermit's in the test output).
- T10 (covers F3, F4): record a guest that calls `perf_event_open`, then replay it; both phases see `ENOSYS`. The four e2e cells above have record/replay disabled for ptrace (for example `tests/e2e/manifests/c-programs.yaml:4888`), so this needs a replay cell or a cargo test.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/316#issuecomment-5872897160
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/316: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_perf_event_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; perf_event_open records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/316 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every call without reading arguments, detcore/src/lib.rs:2182, matching a kernel without CONFIG_PERF_EVENTS); linux_determinism stays nondeterministic; 19 flag behaviours and 10 tests listed; three existing ptrace verify cells (software, hardware, watchpoint) are T1-T3 candidates but no run has been checked against this list, so no coverage token, ptrace_verification stays untested and tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/perf_event_hardware_enosys.c, T3 covered by tests/c/perf_event_watchpoint_enosys.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `perf_event_open`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+perf_event_open&body=Discussion+for+the+%60perf_event_open%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fperf_event_open.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60perf_event_open%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: perf_event_open" (https://github.com/rrnewton/hermit/issues/316), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/316.

### Future work

- Not a blocker for `refuse`: a later `model` could serve a subset of software events (`PERF_COUNT_SW_TASK_CLOCK` and `PERF_COUNT_SW_CPU_CLOCK` from logical time, `PERF_COUNT_SW_DUMMY`) with a Detcore-owned descriptor, if a guest workload is found that needs them. Hardware, raw, breakpoint and tracepoint events would stay refused.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=perf_event_open`, and by confirming hermit currently routes `perf_event_open` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Note (2026-10-03): the routing described in this 2026-07-24 entry is stale. At hermit f4055eb9cae4 the call is Determinized and refused with `ENOSYS` (see Hermit design).

Observed strace line:
```
perf_event_open(NULL, 0, -1, -1, 0) = -1 EFAULT (Bad address)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/316#issuecomment-5872897160
