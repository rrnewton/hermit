---
schema: syscall-pages/v1
syscall: arch_prctl
nr_x86_64: 158
linux_determinism: conditional
target_policy: emulate
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+arch_prctl&body=Discussion+for+the+%60arch_prctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Farch_prctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60arch_prctl%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 5
---

## Summary

`arch_prctl` sets and reads x86_64 thread state that depends on the CPU: the FS and GS base registers, whether the CPUID instruction faults, the extended-state (XSAVE) permissions, the vDSO placement, linear address masking (LAM) and shadow stacks. Hermit passes the FS and GS commands to the kernel. It answers the CPU-feature commands with fixed values from a conservative virtual CPU, so that the host's hardware does not show through, and rejects the rest with EINVAL.

## Linux semantics

`arch_prctl(code, addr)` in Linux 6.9 tries `do_arch_prctl_64` first and falls back to the commands shared with 32-bit (arch/x86/kernel/process_64.c). Commands that read state write through `addr`, and an unwritable address gives EFAULT. Several commands depend on the CPU or the kernel build: CPUID faulting, XSAVE features, LAM, user shadow stacks, CONFIG_CHECKPOINT_RESTORE. Unknown commands give EINVAL. Command values are from arch/x86/include/uapi/asm/prctl.h.

### Flags and options

- F1: ARCH_SET_FS (0x1002) and ARCH_GET_FS (0x1003). SET loads the FS base, which is the thread-local storage
  pointer; a value at or above TASK_SIZE_MAX gives EPERM. GET writes the base to `addr`.
- F2: ARCH_SET_GS (0x1001) and ARCH_GET_GS (0x1004). The same for the GS base.
- F3: ARCH_GET_CPUID (0x1011). Returns 1 if the CPUID instruction runs normally and 0 if it faults
  (SIGSEGV). Without CPUID-faulting hardware the answer is always 1.
- F4: ARCH_SET_CPUID (0x1012). `addr` 0 makes CPUID fault and nonzero restores it. Linux never returns
  EPERM here. The setting is inherited across fork and
  restored to "runs normally" at execve.
- F5: ARCH_GET_XCOMP_SUPP (0x1021). Writes the supported XSAVE feature mask. It always includes the
  legacy x87 and SSE bits, so it is never 0.
- F6: ARCH_GET_XCOMP_PERM (0x1022) and ARCH_GET_XCOMP_GUEST_PERM (0x1024). These write the process's
  permitted XSAVE features, for the process itself and for KVM guests it runs.
- F7: ARCH_REQ_XCOMP_PERM (0x1023) and ARCH_REQ_XCOMP_GUEST_PERM (0x1025). These request permission for a
  dynamically enabled feature. Only AMX tile data (18) can be requested. Another feature that exists gives
  EOPNOTSUPP, a feature the CPU lacks gives EOPNOTSUPP, and an index past the last XSAVE feature gives EINVAL.
- F8: ARCH_MAP_VDSO_X32, ARCH_MAP_VDSO_32 and ARCH_MAP_VDSO_64 (0x2001 to 0x2003). These map a vDSO image
  at `addr`. They need CONFIG_CHECKPOINT_RESTORE and are used by CRIU.
- F9: the LAM commands:
  - ARCH_GET_UNTAG_MASK (0x4001) and ARCH_GET_MAX_TAG_BITS (0x4003) write a mask or a bit count. The bit
    count is 0 without LAM.
  - ARCH_ENABLE_TAGGED_ADDR (0x4002) gives ENODEV without LAM hardware.
  - ARCH_FORCE_TAGGED_SVA (0x4004).
  - All of them need CONFIG_ADDRESS_MASKING.
- F10: ARCH_SHSTK_STATUS (0x5005). Writes the enabled shadow-stack feature bits to `addr`. Without
  CONFIG_X86_USER_SHADOW_STACK it gives EINVAL.
- F11: ARCH_SHSTK_ENABLE (0x5001) and ARCH_SHSTK_DISABLE (0x5002), with feature bits ARCH_SHSTK_SHSTK
  (bit 0) and ARCH_SHSTK_WRSS (bit 1). Only one bit is allowed per call; more than one gives EINVAL. A locked
  feature gives EPERM. Without user shadow-stack hardware the result is EOPNOTSUPP.
  - Disabling SHSTK when it is not enabled returns 0.
  - Disabling WRSS when SHSTK is not enabled gives EPERM.
  - A value with neither bit set gives EINVAL.
- F12: ARCH_SHSTK_LOCK (0x5003) and ARCH_SHSTK_UNLOCK (0x5004). LOCK always returns 0. UNLOCK only works
  through ptrace on another task and gives EINVAL when a thread calls it on itself.
- F13: any other command value gives EINVAL.
- F14: ARCH_SET_CPUID gives ENODEV on a CPU without CPUID faulting.

## Determinism analysis

The FS and GS commands (F1, F2) are pure functions of the guest's own earlier calls: the base is whatever the guest loaded, normally the TLS block that libc set up at startup. They repeat whenever the guest's address layout repeats. Every other group depends on the machine:

- CPUID faulting (F3, F4) depends on whether the CPU supports it. The same binary sees GET = 1 and SET = ENODEV on one host, and a working toggle on another.
- The XSAVE masks (F5, F6) and AMX permission (F7) are the CPU's feature set.
- LAM (F9) and shadow stacks (F10 to F12) depend on both the CPU and the kernel build. The vDSO commands (F8) depend on the kernel build.

On a fixed host with a fixed kernel every command repeats. Across hosts they differ, so linux_determinism is conditional.

### Nondeterminism sources

- CPUID-faulting hardware support (F3, F4).
- The host CPU's XSAVE and AMX feature set (F5, F6, F7).
- Kernel build options: CONFIG_CHECKPOINT_RESTORE, CONFIG_ADDRESS_MASKING, CONFIG_X86_USER_SHADOW_STACK (F8, F9, F10, F11, F12).
- LAM and user shadow-stack hardware (F9, F11).

### Cross-host variation

- CPUID-faulting support on the host CPU: under the default `hermit run` on ptrace, a host with it gets Hermit's fixed answers (GET_CPUID 0, SET_CPUID(0) 0, SET_CPUID(1) EPERM), while a host without it gets the kernel's own answers (GET_CPUID 1, SET_CPUID(0) ENODEV) (F3, F4, F14). The XSAVE, AMX, LAM, vDSO and shadow-stack commands get fixed answers on every host and do not vary.

## Hermit design

**What the code does today (hermit f4055eb9cae4).** detcore/src/lib.rs:2517 dispatches to `handle_arch_prctl` at detcore/src/syscalls/misc.rs:281-360. Its doc comment says it keeps thread-local bases and hides host CPU feature controls. The call is listed as determinized at detcore/src/syscall_classification.rs:59. Command by command:

- **F1, F2.** All four commands are sent to the kernel with `guest.inject` (detcore/src/syscalls/misc.rs:293-296). The kernel does the validation and keeps the state, which belongs only to the guest thread.
- **F3, F4.** The answer depends on the CPUID mode that is configured (detcore/src/syscalls/misc.rs:289-291):
  - When Reverie traps CPUID through hardware faulting, GET returns 0 and SET(0) returns 0, but SET(nonzero) gives EPERM (detcore/src/syscalls/misc.rs:308-317). Hermit answers the faults from a fixed table and keeps CPUID faulting on.
  - In explicit host-CPUID mode (CPUID virtualization off), GET returns 1, SET(0) gives EPERM and SET(nonzero) returns 0 (detcore/src/syscalls/misc.rs:318-327). The comment there says Reverie cannot faithfully deliver a CPUID fault that the guest asks for.
  - A backend that installs its own CPUID table gets the same answers as host-CPUID mode (detcore/src/syscalls/misc.rs:298-306).
  - On a ptrace host without CPUID faulting, both commands go to the kernel and the host's true state is reported (detcore/src/syscalls/misc.rs:329-332).
- **F5, F6.** All three GET commands write 0 through `write_arch_prctl_u64` (detcore/src/syscalls/misc.rs:334-338). That helper gives EFAULT for a NULL address (detcore/src/syscalls/misc.rs:270-279).
- **F7.** Both REQ commands give EINVAL (detcore/src/syscalls/misc.rs:339-341).
- **F10.** SHSTK_STATUS writes 0 (detcore/src/syscalls/misc.rs:345-347).
- **F11.** SHSTK_DISABLE with a nonzero value inside `ARCH_SHSTK_VALID_MASK` (0b11, detcore/src/syscalls/misc.rs:50) returns 0 (detcore/src/syscalls/misc.rs:348-352). Every other DISABLE value, and ENABLE, give EINVAL (detcore/src/syscalls/misc.rs:353-356).
- **F12.** LOCK and UNLOCK give EINVAL (detcore/src/syscalls/misc.rs:353-356).
- **F8, F9, F13.** Every other command, including the vDSO and LAM commands, gives EINVAL (detcore/src/syscalls/misc.rs:358).

**Chosen policy: emulate.** The CPU-feature commands get fixed answers from a virtual CPU with no optional extended state, no shadow stack and no LAM, so a program sees the same features on every host. The FS and GS commands keep going to the kernel, because their state is the guest's own. This is what the code does. It is emulation rather than modelling because Hermit keeps no per-thread state that a SET could change and a GET read back: for example, a SET_CPUID(0) under faulting returns 0 without changing anything Hermit records.

Where Hermit's answers differ from anything Linux can return. Each is deterministic, but a program checking for Linux behaviour could take a different branch:

- **SET_CPUID.** It gives EPERM, which Linux never returns for this command. Linux gives 0 or ENODEV.
- **CPUID state under faulting.** GET reports 0 ("CPUID faults"), yet the guest's own CPUID still works, because Hermit emulates it. A guest that disables CPUID itself, such as a nested record tool, will not get the SIGSEGV it expects.
- **XCOMP masks.** XCOMP_SUPP and XCOMP_PERM report 0, but Linux always includes the x87 and SSE bits (0x3).
- **REQ_XCOMP.** Linux gives EOPNOTSUPP for a feature index other than AMX tile data, which is the case the existing test passes (17, AMX tile config). Hermit gives EINVAL. Linux uses EINVAL only for an index past the last XSAVE feature.
- **Shadow stacks.** SHSTK_LOCK gives EINVAL where Linux returns 0. SHSTK_DISABLE with both bits set returns 0 where Linux gives EINVAL. Disabling WRSS alone returns 0 where Linux gives EPERM or EOPNOTSUPP. ENABLE gives EINVAL where Linux, on hardware without shadow stacks, gives EOPNOTSUPP.
- **Host-dependent branch.** On a ptrace host without CPUID faulting, F3 and F4 still report the host's state. That differs across hosts, though it is fixed on any one host.

Whether these answers should be changed to ones Linux could give (for example, XCOMP masks of 0x3 that match the virtual CPUID table, EOPNOTSUPP instead of EINVAL) is an owner decision. It is recorded under Open questions.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F10, F11, F12, F13): the existing C test, run under `--verify` on ptrace.
  Existing candidate: tests/c/arch_prctl_determinism.c (manifest `c-programs/arch-prctl-determinism`,
  tests/e2e/manifests/c-programs.yaml:325, verify ci true with backends ptrace and kvm). It checks:
  - the GET_FS/SET_FS round trip and a GS change and restore;
  - the CPUID state: SET(1) gives EPERM and SET(0) returns 0 when the state is 0; SET(1) returns 0 or ENODEV
    when it is 1;
  - the three XCOMP GET commands write 0, and a NULL address gives EFAULT;
  - REQ_XCOMP with 17 gives EINVAL;
  - SHSTK_STATUS writes 0, and a NULL address gives EFAULT;
  - DISABLE(SHSTK) returns 0, DISABLE(0) and DISABLE(1<<63) give EINVAL, and ENABLE, LOCK and UNLOCK give EINVAL;
  - command 0x7fffffff gives EINVAL.
  The test also has a guest `--host-cpuid` mode (GET 1, SET(0) EPERM, SET(1) 0), but the manifest cell does
  not pass that argument.
- T2 (covers F8, F9): ARCH_MAP_VDSO_64, ARCH_GET_UNTAG_MASK, ARCH_GET_MAX_TAG_BITS, ARCH_ENABLE_TAGGED_ADDR and
  ARCH_FORCE_TAGGED_SVA each return the documented virtual answer (EINVAL today), byte-identical under
  `--verify`, and no host mask is written to `addr`. No existing test exercises these commands.
- T3 (covers F11): DISABLE(WRSS) alone and DISABLE(SHSTK | WRSS) return the virtual answer chosen by the
  owner (0 today for both). T1 does not exercise these two values.
- T4 (covers F3, F4): run T1's CPUID checks on a host without CPUID faulting and on one with it, and record
  which branch each host takes. The answers must be identical across `--verify` runs on the same host. Note that
  `cpuid_faulting_supported` (detcore/tests/misc/mod.rs:89-102) only probes the host natively in a forked child,
  not under Hermit, so it is not a test of this handler.
- U1 (covers F14; host-config): ENODEV from SET_CPUID appears only on a CPU without CPUID faulting, which the CI host may not be. On such a host Hermit forwards the call to the kernel unchanged (detcore/src/syscalls/misc.rs:329-332).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (FS/GS injected, CPU-feature commands answered from a fixed virtual CPU per misc.rs:281-360); linux_determinism conditional confirmed; 13 flag behaviours and 4 tests; left todo with an owner question on the answers Linux cannot produce (SET_CPUID EPERM, XCOMP masks 0, EINVAL vs EOPNOTSUPP, SHSTK_LOCK).
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (CPUID-faulting support decides whether GET/SET_CPUID get Hermit's fixed answers or the kernel's); F4 narrowed and new F14, ENODEV from SET_CPUID without CPUID faulting (host-config U1); the open question on hosts without CPUID faulting is resolved by decision 2 (no virtualization for now)
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/arch_prctl_determinism.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `arch_prctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+arch_prctl&body=Discussion+for+the+%60arch_prctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Farch_prctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60arch_prctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `arch_prctl`: the 2026-07-24 classification campaign filed issues only for syscalls that were then awaiting classification, and Detcore already handled this one.

### Open questions

- **Answers Linux cannot produce.** Should the virtual CPU's answers be changed to values Linux itself could return? Each change is guest-visible and would change tests/c/arch_prctl_determinism.c:
  - SET_CPUID gives EPERM today; Linux gives 0 or ENODEV.
  - XCOMP_SUPP and XCOMP_PERM report 0 today; Linux always includes 0x3, and the values could match the virtual CPUID table.
  - REQ_XCOMP gives EINVAL today; Linux gives EOPNOTSUPP.
  - SHSTK_LOCK gives EINVAL today; Linux returns 0.
  - SHSTK_DISABLE with both bits returns 0 today; Linux gives EINVAL.
  - Disabling WRSS alone returns 0 today; Linux gives EPERM or EOPNOTSUPP.
  - The current answers are deterministic.
