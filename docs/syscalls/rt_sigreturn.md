---
schema: syscall-pages/v1
syscall: rt_sigreturn
nr_x86_64: 15
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/408
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigreturn&body=Discussion+for+the+%60rt_sigreturn%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigreturn.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigreturn%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`rt_sigreturn()` returns from a signal handler: the kernel reads the signal frame it wrote at delivery from the user stack and restores the interrupted context from it. Hermit forwards it to the host kernel on ptrace; its result is exactly as repeatable as the signal delivery and handler that produced the frame.

## Linux semantics

The call takes no arguments. It is reached through the `SA_RESTORER` trampoline that libc installs with every `rt_sigaction`, when the handler returns; programs do not normally call it directly, and doing so without a valid frame on the stack kills the process. The kernel reads an `rt_sigframe` (a `ucontext_t` plus `siginfo`) at the current stack pointer and restores registers, the signal mask, the extended FPU state and the alternate signal stack from it. The handler may have edited the frame, so the restored context is whatever is in that memory, not necessarily what was saved. The "return value" is the restored `rax`, which lets an interrupted syscall's result or a handler-chosen value pass through unchanged.

### Flags and options

- F1: general registers, `rip` and `rsp` are restored from `uc_mcontext`, including edits the handler made; `eflags` is restored through a mask of user-modifiable bits, and segment state follows F5
- F2: the signal mask is set from `uc_sigmask`; `SIGKILL` and `SIGSTOP` are removed, and pending signals unblocked by the new mask are delivered on return
- F3: extended FPU state (`fpstate`): restored from the xsave area after checking its magic numbers and size; a NULL `fpstate` pointer resets the FPU to its initial state; a malformed area resets it and raises `SIGSEGV` on some kernels
- F4: the alternate signal stack is restored from `uc_stack` as if by `sigaltstack`; an invalid value is ignored rather than failing the return
- F5: `uc_flags`: `UC_SIGCONTEXT_SS` and `UC_STRICT_RESTORE_SS` decide whether the saved `ss` is restored or fixed up to a flat user selector
- F6: an unreadable frame or invalid saved state makes the kernel force `SIGSEGV` on the task instead of returning
- F7: return value: the restored `rax`, never an errno of its own; an interrupted syscall's `-EINTR`, a restart, or a handler-written value all reach the program through it

## Determinism analysis

The call reads only the frame on the guest's own stack and the task's register and signal state; nothing in it depends on time or on another process. Its result therefore repeats whenever the frame repeats. The frame is written by the kernel at delivery: under Hermit, signal delivery is scheduler-serialized (`detcore/src/syscalls/signal.rs:674`), the stack addresses repeat because the guest runs with `ADDR_NO_RANDOMIZE` (stated at `tests/c/personality_domain.c:12`), and the handler is guest code run under the same scheduler. The one host-dependent part is the xsave area inside the frame, whose layout and size depend on the CPU features the host enables; it is fixed on a given host. That host dependency is why `linux_determinism` is conditional rather than deterministic.

### Nondeterminism sources

- Host CPU features that change the size and contents of the xsave area (F3), and the kernel's choice of frame layout; both are fixed on a given host and kernel.
- Anything that makes delivery itself differ (which instruction was interrupted, which syscall was restarted). Those are properties of signal delivery and of the scheduler, tracked with the signal family, not with this call.

### Cross-host variation

- Host CPU's enabled xsave features (XCR0, for example AVX-512 or AMX state) and the kernel's frame layout: they decide the size and contents of the `fpstate` area that the call checks and restores, so which bytes of a handler-edited frame are restored differs between hosts (F3). Virtual CPUID does not change the kernel's xsave format, and the call is forwarded unchanged (`detcore/src/lib.rs:2843`).
- Host kernel version: whether a malformed `fpstate` area only resets the FPU or also raises `SIGSEGV` (F3).

## Hermit design

Target policy: `passthrough`. The code already does this. `rt_sigreturn` is classified `PassThrough` at `detcore/src/syscall_classification.rs:804`, the last entry of the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The comment directly above it, at `detcore/src/syscall_classification.rs:802`, records the backend split: "Ptrace executes rt_sigreturn directly; DBT has dedicated injected-sigreturn handling, while KVM deterministically reports its current lack of signal support". The test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins it as PassThrough at `detcore/src/syscall_classification.rs:2084`, and it is in the census at `detcore/src/syscall_classification.rs:1741`. The blanket arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it.

Why passthrough, not `model` or `emulate`:

- The state it restores (registers, FPU state, signal mask) lives in the traced task, and on ptrace the kernel already wrote the frame it reads. Emulating it would mean parsing the kernel's frame format and setting every register through ptrace, reproducing the kernel's own validation for no change in result.
- Its inputs are all guest-controlled memory and task state that Hermit's serialized signal delivery already makes repeatable. There is no host input to virtualize beyond the CPU's xsave format, which Hermit does not control on ptrace.
- Refusing it is not an option: every handler that returns uses it.

Code facts show `subscribed_record_replay: false`: `rt_sigreturn` is not in the `--passthru-opt` list near `detcore/src/lib.rs:1214`. `hermit record` uses its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places outside the exported tree; this body does not describe how record treats it.

The open gap is test evidence. `tests_naming` is 0: no test names `SYS_rt_sigreturn`. Every fixture that installs a handler reaches it through the libc restorer, so evidence comes from tests that check what the return restored.

## Recommended tests

- T1 (covers F1, F7): under `hermit run --verify` on ptrace, install an `SA_SIGINFO` handler that edits `uc_mcontext` (sets a callee-saved register and `rax`, and advances `rip` past a known instruction) and assert the interrupted code sees the edited values after the return.
  Existing partial coverage: `tests/c/waitid_signal_interrupt.c:86` and `tests/c/waitid_signal_interrupt.c:87` write `REG_RAX` and `REG_RIP` in the handler, but the case is driven only on DBT by `hermit-cli/tests/waitid_signal_interrupt.rs:869` (`dbt_exact_child_waits_honor_a_changed_signal_context`), so it is not ptrace evidence.
- T2 (covers F2): block `SIGUSR2` in `sa_mask`, have the `SIGUSR1` handler raise `SIGUSR2` and clear it from `uc_sigmask`, and assert `SIGUSR2` is delivered right after the return; also assert a handler that adds `SIGKILL` to `uc_sigmask` leaves it unblocked.
  Existing partial coverage: `tests/c/signal_delivery_sequence.c:442` and `tests/c/signal_delivery_sequence.c:457` raise `SIGUSR1` blocked and unblocked and count deliveries, which needs each handler return to restore the mask (cell `c-programs/signal-delivery-sequence`, `tests/e2e/manifests/c-programs.yaml:17172`, ptrace verify in CI).
- T3 (covers F3): load known values into an `xmm` register and `ymm` upper half where available, take a signal whose handler clobbers them, and assert the values are back after the return; in a second case set `uc_mcontext.fpregs` to NULL and assert the FPU control word reads its initial value.
- T4 (covers F4, F5): install an alternate stack, take an `SA_ONSTACK` signal whose handler changes `uc_stack`, and assert `sigaltstack(NULL, &cur)` after the return reports the changed stack; print `uc_flags` from the handler and assert the expected `UC_SIGCONTEXT_SS` / `UC_STRICT_RESTORE_SS` bits.
  Existing partial coverage: `tests/c/signal_determinism.c:754` (`test_altstack_preservation`, two deliveries on an alternate stack); the CI cell `c-programs/signal-determinism` (`tests/e2e/manifests/c-programs.yaml:8132`) passes only the `itimer-delivery` guest argument on ptrace, so this check is not run in CI.
- T5 (covers F6): in a forked child, call raw `syscall(SYS_rt_sigreturn)` with a stack pointer at unmapped memory (through a small assembly stub) and assert the parent sees the child killed by `SIGSEGV`. Note that `--verify` cannot compare a signal-terminated guest, so the assertion must be made by the parent.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/408: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional (the frame's xsave layout is host-CPU dependent), target_policy investigate -> passthrough (ptrace executes it directly per detcore/src/syscall_classification.rs:802; forwarded at detcore/src/lib.rs:2843); listed 7 frame behaviours and 5 tests; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host-enabled xsave features decide the fpstate layout the call restores; kernel version decides the malformed-fpstate outcome); no U items; no open questions resolved


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rt_sigreturn`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigreturn&body=Discussion+for+the+%60rt_sigreturn%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigreturn.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigreturn%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: rt_sigreturn" (https://github.com/rrnewton/hermit/issues/408), legacy variant A1.

### Open questions

- None that block the policy. T5 needs a fixture that kills its own child; the owner may prefer to record F6 as covered by kernel behaviour rather than add a crash fixture.

### Legacy text (2026-07-24)

The legacy body said: "Invoked by the SA_RESTORER trampoline; must not be called directly (would corrupt the stack). Verified via natural occurrence under strace (see evidence) rather than direct invocation." Its legacy recommended action was **NEEDS-INVESTIGATION**.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=rt_sigreturn` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `rt_sigreturn` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
rt_sigreturn({mask=[]})         = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
