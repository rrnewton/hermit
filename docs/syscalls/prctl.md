---
schema: syscall-pages/v1
syscall: prctl
nr_x86_64: 157
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/325
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+prctl&body=Discussion+for+the+%60prctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60prctl%60%2C+or+in+what+the+page+says%3F%0A
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

`prctl` is a process-control multiplexer that selects one operation by its first argument. Hermit handles it option by option:

- It keeps the timer slack in virtual per-thread state.
- It passes through the five SET/GET pairs whose state belongs only to the guest's own task: name, dumpability, keep-capabilities, parent-death signal, and transparent-huge-page disable.
- It records and replays PR_CAPBSET_READ.
- It refuses every other option with ENOSYS.

## Linux semantics

`prctl(option, arg2, arg3, arg4, arg5)` returns 0, a small integer, or a value written through a pointer argument, depending on `option`. Linux returns EINVAL for an option number it does not know, for an option that belongs to another architecture, and for most options when an unused argument is nonzero. Some options read or change state that the host owns: the capability bounding set, seccomp, the timestamp counter, the memory map, Yama ptrace scope, perf events. Others only touch the calling task's own attributes. The option numbers below are those in the host's `/usr/include/linux/prctl.h`.

### Flags and options

- F1: PR_SET_NAME (15) and PR_GET_NAME (16). SET copies up to 16 bytes (TASK_COMM_LEN) from arg2 into the
  thread's `comm` and truncates the rest. GET writes 16 bytes to arg2. A bad address gives EFAULT. execve
  resets `comm` to the program's basename.
- F2: PR_SET_DUMPABLE (4) and PR_GET_DUMPABLE (3). SET takes 0 or 1; any other value, including
  2 (SUID_DUMP_ROOT), gives EINVAL. GET returns the current value. execve recomputes it from the credentials
  and, for a setuid or unreadable binary, from the `fs.suid_dumpable` sysctl.
- F3: PR_SET_KEEPCAPS (8) and PR_GET_KEEPCAPS (7). SET takes 0 or 1 (else EINVAL), and gives EPERM if
  SECBIT_KEEP_CAPS_LOCKED is set. execve clears the flag.
- F4: PR_SET_PDEATHSIG (1) and PR_GET_PDEATHSIG (2). SET takes a valid signal number or 0 (else EINVAL).
  GET writes an int to arg2. fork clears the value in the child.
- F5: PR_SET_THP_DISABLE (41) and PR_GET_THP_DISABLE (42). Nonzero arg3 to arg5 give EINVAL. The flag is
  inherited across fork and kept across execve.
- F6: PR_SET_TIMERSLACK (29) and PR_GET_TIMERSLACK (30). SET with 0 restores the thread's default slack;
  any other value sets the slack in nanoseconds. A real-time or deadline thread ignores the request. GET
  returns the current slack. A new task copies the creator's current slack into both its current and its
  default slack. The value is also visible as `/proc/<tid>/timerslack_ns`.
- F7: PR_CAPBSET_READ (23). Returns 1 or 0 for capability arg2. A number above the running kernel's
  CAP_LAST_CAP gives EINVAL.
- F8: PR_CAPBSET_DROP (24), and PR_CAP_AMBIENT (47) with its sub-operations IS_SET, RAISE, LOWER and
  CLEAR_ALL. These change the capability sets. Dropping needs CAP_SETPCAP; raising an ambient capability needs
  it to be permitted and inheritable.
- F9: PR_SET_NO_NEW_PRIVS (38) and PR_GET_NO_NEW_PRIVS (39). SET takes only arg2 = 1, with the other
  arguments zero, and cannot be undone.
- F10: PR_SET_SECCOMP (22) and PR_GET_SECCOMP (21). These install strict or filter mode, or report the mode.
- F11: PR_SET_CHILD_SUBREAPER (36) and PR_GET_CHILD_SUBREAPER (37). GET writes an int to arg2.
- F12: PR_MCE_KILL (33), with sub-operations CLEAR and SET (EARLY, LATE, DEFAULT), and PR_MCE_KILL_GET (34).
- F13: PR_SET_TSC (26) and PR_GET_TSC (25). These choose whether RDTSC works or raises SIGSEGV for the
  thread. GET writes an int to arg2.
- F14: address-space and debugger state:
  - PR_GET_TID_ADDRESS (40) writes the thread's clear_child_tid pointer.
  - PR_SET_MM (35) rewrites mm fields and needs CAP_SYS_RESOURCE.
  - PR_SET_VMA (0x53564d41) names anonymous mappings.
  - PR_GET_AUXV (0x41555856) copies the saved auxiliary vector.
  - PR_SET_PTRACER (0x59616d61) sets the Yama ptracer.
- F15: PR_SET_SECUREBITS (28) and PR_GET_SECUREBITS (27). SET needs CAP_SETPCAP.
- F16: the other options that do something on x86_64:
  - PR_TASK_PERF_EVENTS_DISABLE and ENABLE (31, 32)
  - PR_SET_SPECULATION_CTRL and GET (53, 52)
  - PR_SET_IO_FLUSHER and GET (57, 58)
  - PR_SCHED_CORE (62)
  - PR_SET_MEMORY_MERGE and GET (67, 68)
  - PR_SET_MDWE and GET (65, 66)
  - PR_SET_SYSCALL_USER_DISPATCH (59)
  - PR_SET_TIMING and GET (14, 13)
  - PR_TIMER_CREATE_RESTORE_IDS (77) and PR_FUTEX_HASH (78), on the newest kernels.
  Each reads or changes perf, speculation, memory-reclaim, core-scheduling, KSM, W^X, syscall-interception or
  futex state.
- F17: options that Linux rejects with EINVAL on x86_64, and unknown option numbers:
  - Options for other architectures: PR_SET/GET_UNALIGN (6, 5), FPEMU (10, 9), FPEXC (12, 11), ENDIAN (20, 19),
    FP_MODE (45, 46), SVE (50, 51), SME (63, 64), the PAC keys (54, 60, 61), TAGGED_ADDR_CTRL (55, 56), the
    RISC-V options (69 to 71) and PPC_DEXCR (72, 73).
  - The removed MPX options (43, 44).
  - The generic shadow-stack options (74 to 76), which x86_64 controls through `arch_prctl` instead.
  - Any number Linux does not define.

## Determinism analysis

Most of the per-task options (F1 to F4, F9, F11) are pure functions of the guest's own earlier calls and arguments. Their state starts from execve or fork, so they repeat as long as the program and its launch path are the same. The exceptions make the call conditional:

- **Inherited from the launcher.** The initial THP_DISABLE value (F5), the initial dumpability after a setuid or unreadable exec (F2, through `fs.suid_dumpable`), and the physical timer slack (F6) are inherited from whoever launched Hermit.
- **Host kernel and host credentials.** PR_CAPBSET_READ (F7) depends on the bounding set the launcher had and on the host kernel's CAP_LAST_CAP. A capability added in a newer kernel reads 0 or 1 on one host and gives EINVAL on another. The capability, securebits and seccomp options (F8, F10, F15) depend on host privilege.
- **Effects outside the guest.** PR_SET_TSC (F13) would change how RDTSC behaves under Hermit's own virtualization. PR_SET_SECCOMP (F10) changes how later syscalls are dispatched. The F14 options read or rewrite kernel-chosen addresses. The F16 options touch host-wide perf, speculation, memory-reclaim and scheduling state.

### Nondeterminism sources

- Launcher-inherited initial state: THP_DISABLE, physical timer slack, dumpability after a privileged exec (F2, F5, F6).
- Host kernel version and the launcher's capability sets: CAP_LAST_CAP and the bounding set (F7, F8, F15).
- Host privilege: CAP_SETPCAP, CAP_SYS_RESOURCE, securebits locks (F3, F8, F14, F15).
- Kernel-chosen addresses (F14) and host-wide scheduler, perf, speculation and memory-management state (F16).
- Options whose effect would change Hermit's own control of the guest: seccomp, TSC, syscall user dispatch (F10, F13, F16).

### Cross-host variation

- Host kernel version (`/proc/sys/kernel/cap_last_cap`): PR_CAPBSET_READ for a capability added in a newer kernel reads 0 or 1 on one host and gives EINVAL on another (F7). In `run` the host kernel answers (`detcore/src/syscalls/misc.rs:374`).

## Hermit design

**What the code does today (hermit f4055eb9cae4).** detcore/src/lib.rs:2522 dispatches to `handle_prctl` at detcore/src/syscalls/misc.rs:362-406. Its doc comment says it keeps deterministic thread controls, reports the container's fixed capability bounding set, and rejects options that expose process or host state Hermit does not model. Option by option:

- **Timer slack (F6), modelled.**
  - detcore/src/syscalls/misc.rs:376-392 never passes PR_SET_TIMERSLACK to the tracee. A request of 0 restores `default_timer_slack_ns`; anything else is stored in `thread_state_mut().timer_slack_ns`.
  - detcore/src/syscalls/misc.rs:393 answers PR_GET_TIMERSLACK from the same state.
  - The default is the kernel's 50 000 ns, not the launcher's physical value (detcore/src/tool_local.rs:1706-1711).
  - A new task copies the creator's current slack into both fields, as Linux does (detcore/src/lib.rs:1553-1556).
  - Linux's real-time and deadline branch is unreachable, because the virtual scheduling policy is always SCHED_OTHER (detcore/src/syscalls/misc.rs:384-385).
- **Guest-owned task state (F1 to F5), passed through.**
  - `is_supported_prctl_option` (detcore/src/syscalls/misc.rs:81-128) admits NAME, THP_DISABLE, DUMPABLE, KEEPCAPS and PDEATHSIG, each SET/GET pair with a comment arguing why passthrough is deterministic.
  - They reach the kernel through `passthrough` (detcore/src/syscalls/misc.rs:400-402), which calls `record_or_replay_preserving_tool_errors` (detcore/src/lib.rs:367-374). Linux therefore does the validation and keeps the state.
- **PR_CAPBSET_READ (F7), recorded.** detcore/src/syscalls/misc.rs:373-374 sends it through `record_or_replay`. In run mode the host kernel answers, and record/replay reproduces that answer. The comment says the bounding set is fixed by the container launch policy, but the answer for a capability above the host's CAP_LAST_CAP still comes from the host kernel.
- **Capability changes (F8).** These pass through only when `backend_virtualizes_capability_prctls` is set (detcore/src/syscalls/misc.rs:394-399 and 130-134). That config field (detcore-model/src/config.rs:183-188) defaults to false (asserted at detcore-model/src/config.rs:1481), so on ptrace both options get ENOSYS.
- **Everything else (F9 to F17) gets ENOSYS** (detcore/src/syscalls/misc.rs:403).

**Chosen policy: model.** The design above is the right shape, and the timer slack already shows the modelled form: Hermit owns the state the guest reads back. Keep it:

- Keep the virtual timer slack.
- Keep passing through the options whose state belongs only to the guest's task.
- Answer host-dependent reads from Hermit-owned state, not from the host.
- Keep refusing options with effects Hermit does not model.

Remaining gaps:

- **F7.** Record and replay makes PR_CAPBSET_READ repeat within one recording, but `run` still varies with the host kernel's CAP_LAST_CAP. The modelled form answers from a fixed virtual bounding set with a pinned last capability, and gives EINVAL above it. This is an owner question because it changes guest-visible results.
- **F5.** The initial THP_DISABLE value is inherited from the launcher, because the passthrough starts from the host task's flag. A modelled launch would clear it.
- **F17.** Linux returns EINVAL for options that belong to other architectures and for unknown numbers. Hermit returns ENOSYS from the catch-all arm (detcore/src/syscalls/misc.rs:403). A program that probes for a newer option and handles only EINVAL takes the wrong branch.
- **Target (owner decision 6, 2026-10-04): F17 gets Linux's EINVAL.** `prctl` is an emulated call, so it owes Linux's checks in kernel order, and the first check `sys_prctl` (kernel/sys.c) makes after the LSM hook is the option switch, whose `default:` arm returns EINVAL. Detcore keeps a fixed list of the option numbers Linux implements on x86_64 (F1 to F16) and answers every other number with EINVAL before looking at any argument; the list is Hermit's own, not the host kernel's, so a newer host option (PR_FUTEX_HASH, 78) does not change the answer. Options Linux implements but Hermit refuses (F9 to F16, and F8 on ptrace) keep the fixed ENOSYS, which decision 6 allows for refused options. The code change is to split the `_ => Err(Errno::ENOSYS.into())` arm at detcore/src/syscalls/misc.rs:403 into "known to Linux on x86_64: ENOSYS" and "anything else: EINVAL". A program that falls back on EINVAL, such as a probe for a newer option, then takes Linux's branch.
- **F9 to F16.** These are refused with ENOSYS even though Linux implements them. Several have their own refusal tests (see Recommended tests), which shows the refusal is intended for now. PR_SET_NO_NEW_PRIVS is the most commonly called. It is refused by design (detcore/src/syscalls/misc.rs:967 asserts it is not supported), and modelling it would mean a virtual flag that tightens later execve behaviour.

## Recommended tests

- T1 (covers F1, F2, F3, F4): name round trip, dumpability 1 to 0 to 1 with EINVAL for 2, keep-caps round
  trip, and parent-death signal set, get and clear, all under `--verify` on ptrace. Existing candidates:
  tests/c/prctl_identity.c (manifest `c-programs/prctl-identity`), tests/c/prctl_dumpable.c
  (`c-programs/prctl-dumpable`), tests/c/prctl_pdeathsig.c (`c-programs/prctl-pdeathsig`), and
  `prctl_keepcaps_round_trips_deterministically` (detcore/tests/misc/mod.rs:289).
- T2 (covers F5): THP_DISABLE set and clear round trip, plus EINVAL for nonzero arg3. Also launch the guest
  from a parent that has THP_DISABLE set, and pin the initial value the guest observes. Existing candidate for
  the round trip: tests/c/thp_disable.c (`c-programs/thp-disable`).
- T3 (covers F6): default 50 000 ns, set and get, reset with 0, inheritance by a child thread and a
  child process, and agreement with `/proc/self/timerslack_ns`. Existing candidates: tests/c/prctl_option_policy.c
  (`c-programs/prctl-option-policy`), `timer_slack_prctl_and_procfs_share_virtual_state`
  (detcore/tests/misc/mod.rs:305) and `timer_slack_virtual_state_is_isolated_from_physical_tracee`
  (detcore/tests/misc/mod.rs:744).
- T4 (covers F7): read every capability from 0 to 63 and print each result or errno. The output must be
  identical across `--verify` runs, and once the virtual bounding set exists, identical across hosts with
  different kernels. No existing test names PR_CAPBSET_READ.
- T5 (covers F8, F15): on ptrace, PR_CAPBSET_DROP, each PR_CAP_AMBIENT sub-operation, and the
  PR_SET/GET_SECUREBITS pair give the documented refusal (ENOSYS today). No existing test names them; the unit
  test `backend_virtualized_prctl_support_is_capability_scoped` (detcore/src/syscalls/misc.rs:973) checks
  only the option list.
- T6 (covers F9, F10, F11, F12): the refusal tests. Existing candidates: tests/c/no_new_privs_refusal.c
  (`c-programs/no-new-privs-refusal`), tests/c/seccomp_refusal.c (`c-programs/seccomp-refusal`, PR_GET_SECCOMP
  only), tests/c/child_subreaper_refusal.c (`c-programs/child-subreaper-refusal`) and tests/c/mce_kill_policy.c
  (`c-programs/mce-kill-refusal`).
- T7 (covers F13, F14, F16): each option in these groups returns the documented refusal under `--verify`, and
  RDTSC keeps working after a refused PR_SET_TSC. No existing test names PR_SET_TSC, PR_GET_TID_ADDRESS,
  PR_SET_MM or PR_SET_PTRACER.
- T8 (covers F17): an option for another architecture (PR_GET_UNALIGN), a removed option (43) and an
  undefined number (0x7fffffff) give EINVAL, as on Linux (owner decision 6); today Hermit returns ENOSYS, so
  this test fails until the catch-all arm is split. A refused option Linux implements (PR_SET_NO_NEW_PRIVS)
  still gives ENOSYS in the same program.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/325#issuecomment-5872919039
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/325: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; prctl records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/325 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (virtual timer slack, passthrough of guest-owned task state, ENOSYS refusal of the rest, per misc.rs:362-406); linux_determinism conditional kept; 17 flag behaviours and 8 tests; left todo with owner questions on the unknown-option errno and a virtual capability bounding set.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host cap_last_cap decides which capabilities PR_CAPBSET_READ knows); no U items; resolved the open question on a fixed virtual bounding set (decision 2: tag, no virtualization)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): not a credential-record call, target_policy stays model; no reduced-fidelity line (the body does not discuss --no-namespace or in-process modes); no question removed; new open question on how PR_SET_KEEPCAPS, PR_CAPBSET_READ, PR_CAPBSET_DROP and PR_CAP_AMBIENT relate to the decision 4 capability record
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (option numbers Linux does not implement on x86_64 get EINVAL from a Hermit-owned list, refused options Linux implements keep ENOSYS; T8 now asserts EINVAL); target_policy stays model; no new F/T ids; removed the unknown-options question


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `prctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+prctl&body=Discussion+for+the+%60prctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fprctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60prctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: prctl" (https://github.com/rrnewton/hermit/issues/325), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/325.

### Open questions

- **Capability options and the credential record (owner decision 4).** Decision 4 puts the capability sets in a per-process Detcore record that `capset` and the set*id calls update and `capget` reports; it does not name `prctl`. Three groups of options touch the same state; this body keeps describing today's handling of them until the owner answers:
  - PR_SET_KEEPCAPS and PR_GET_KEEPCAPS (F3) are passed through as the kernel's per-thread flag (detcore/src/syscalls/misc.rs:102-110). The record's uid-change fixups need that flag to decide whether a full drop to nonzero uids keeps the permitted set (the `capget` and `setuid` bodies). Should Detcore track the flag in the record, cleared at execve as Linux clears it, or read it from the kernel when a set*id call is modelled?
  - PR_CAPBSET_READ (F7) is still answered by the kernel (detcore/src/syscalls/misc.rs:373-374), while the record now holds the other sets and `capset`'s inheritable rule reads the bounding set. Should the bounding set join the record?
  - PR_CAPBSET_DROP and PR_CAP_AMBIENT (F8) return ENOSYS on ptrace (detcore/src/syscalls/misc.rs:394-399, 403) and are left to the backend when `backend_virtualizes_capability_prctls` is set (detcore-model/src/config.rs:183-188). Under the record, a full uid drop clears the ambient set and `capset` reduces it. Should these options act on the record, or stay refused, with the ambient set fixed empty and the bounding set fixed at launch?

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=prctl`. At that time the call had not yet been classified, and the harness ran end to end under `hermit run` with exit status 0. At hermit f4055eb9cae4 it is determinized with a typed handler (detcore/src/syscalls/misc.rs:367).

Observed strace line:
```
prctl(PR_CAPBSET_READ, CAP_MAC_OVERRIDE) = 1
```

---
*Autonomous syscall-classification sweep, batch 3, of the calls that were then awaiting classification in `syscall_classification.rs`. Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/325#issuecomment-5872919039
