---
schema: syscall-pages/v1
syscall: sigaltstack
nr_x86_64: 131
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1955
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sigaltstack&body=Discussion+for+the+%60sigaltstack%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsigaltstack.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sigaltstack%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`sigaltstack(ss, old_ss)` installs, removes or queries the calling thread's alternate signal stack, the memory a handler registered with `SA_ONSTACK` runs on. Hermit forwards it to the host kernel. On the ptrace backend the kernel both keeps this state and builds the signal frames that use it, and Detcore has no alternate-stack state of its own, so pass-through is the target: every input comes from the guest, and the result repeats given Hermit's serialized signal delivery and a fixed host CPU and kernel. It is common in ordinary programs: the Rust runtime queries and installs a guard stack on each thread it starts, as the recorded syscall trace at `tests/rust/bind_connect_race.rs:70` shows.

Manual reference: [sigaltstack(2)](https://man7.org/linux/man-pages/man2/sigaltstack.2.html).

## Linux semantics

The kernel (`do_sigaltstack` in `kernel/signal.c`) works on the calling thread only. If `ss` is non-NULL it is copied in first; a fault gives `EFAULT`. If `old_ss` is non-NULL the current setting is filled in: `ss_sp` and `ss_size`, and `ss_flags` set to `SS_DISABLE` when no stack is installed, `SS_ONSTACK` when the thread's stack pointer is currently inside the alternate stack, otherwise 0, with `SS_AUTODISARM` added if it was requested. If `ss` is non-NULL: a thread running on its alternate stack gets `EPERM` (this check comes before flag validation); a mode other than 0, `SS_ONSTACK` or `SS_DISABLE` (after removing `SS_AUTODISARM`) gets `EINVAL`; a request identical to the current setting returns 0 at once; `SS_DISABLE` removes the stack and ignores `ss_sp` and `ss_size`; otherwise a size below `MINSIGSTKSZ` (2048 on x86-64) gets `ENOMEM`, and on x86 so does a size below the current signal-frame size when the kernel enforces that (`sigaltstack_size_valid`, active with `CONFIG_STRICT_SIGALTSTACK_SIZE` or once the process has AMX state permission). Writing `old_ss` back can fail with `EFAULT`. With `SS_AUTODISARM`, the kernel clears the setting on entry to a handler that runs on the alternate stack and restores it on `sigreturn`.

The setting is inherited by `fork` and by `clone` without `CLONE_VM` or with `CLONE_VFORK`; a thread created with `CLONE_VM` and without `CLONE_VFORK` starts with none; `execve` clears it. When a signal whose handler has `SA_ONSTACK` is delivered and the thread is not already on the alternate stack, the kernel builds the frame at the top of the alternate stack instead of the current one.

### Flags and options

- F1: `ss == NULL`, `old_ss` non-NULL: query only; nothing changes.
- F2: install: `ss_flags == 0` with `ss_sp` and `ss_size` of at least `MINSIGSTKSZ`.
- F3: `SS_DISABLE`: remove the alternate stack; `ss_sp` and `ss_size` are ignored and later queries report `SS_DISABLE` with zero pointer and size.
- F4: `SS_ONSTACK` given as the input mode: accepted and treated like 0 (historical spelling).
- F5: `SS_AUTODISARM` combined with the mode: the setting is cleared while a handler runs on the alternate stack and restored on return.
- F6: any other mode bits: `EINVAL`.
- F7: a change requested while running on the alternate stack: `EPERM`, even for an otherwise invalid mode; a query from there succeeds and reports `SS_ONSTACK`.
- F8: `ss_size` below `MINSIGSTKSZ`: `ENOMEM`; resubmitting the exact current setting returns 0 before this check.
- F9: unreadable `ss` or unwritable `old_ss`: `EFAULT`.
- F10: the `old_ss` flags report `SS_DISABLE`, `SS_ONSTACK` or 0, plus `SS_AUTODISARM` when set.
- F11: inheritance: kept across `fork`, `vfork` and `clone` without `CLONE_VM`; absent in a new thread (`CLONE_VM` without `CLONE_VFORK`); cleared by `execve`.
- F12: delivery: a handler installed with `SA_ONSTACK` runs on the alternate stack when one is installed and the thread is not already on it; otherwise it runs on the current stack.
- F13: host-config failure: where the kernel enforces it, `ss_size` at or above `MINSIGSTKSZ` but below the host's signal-frame size gives `ENOMEM`. The frame size follows the XSAVE features the host kernel enables, so a size accepted on one host is rejected on another.

## Determinism analysis

The call has no host input beyond the CPU and kernel configuration. Its arguments are guest addresses and sizes; the stored setting is per-thread state that only the guest changes; and the `SS_ONSTACK` report (F7, F10) depends only on whether the guest is inside a handler on that stack. Under Hermit, guest addresses repeat across runs (the pass-through comment at `detcore/src/syscall_classification.rs:708` relies on "fixed guest address and PID namespaces" for this group), and signal delivery points are chosen by Hermit's scheduler, so F5, F7 and F12 repeat. What can differ is the host: the x86 signal frame grows with the XSAVE features the host kernel enables, so the frame size, whether a small alternate stack overflows, and whether F8's frame-size check rejects a size all depend on the CPU and kernel build; glibc's `SIGSTKSZ` and `sysconf(_SC_SIGSTKSZ)` follow the same host value, which is why the fixture uses a fixed 64 KiB buffer (`tests/c/sigaltstack_state.c:20`, "avoid host-dependent SIGSTKSZ"). So the call is `conditional`: repeatable on a fixed host, given Hermit's control of signal timing, with cross-host differences limited to frame size.

### Nondeterminism sources

- When signals arrive relative to the guest's code, which decides whether a query or change happens on the alternate stack (F7, F10) and which stack a handler uses (F12); Hermit's scheduler fixes it.
- Host CPU XSAVE features and kernel configuration: signal-frame size and the strict size check (F8), and the host-dependent `SIGSTKSZ` that programs often size their stacks with.

### Cross-host variation

- Host CPU XSAVE features and kernel configuration: they set the signal-frame size, and with it whether the strict size check rejects a given `ss_size` (F13) and whether a small alternate stack overflows when a handler runs on it (F12).

## Hermit design

Target policy: `passthrough` (changed from `investigate`). Today `sigaltstack` is classified `PassThrough` at `detcore/src/syscall_classification.rs:713`, in the pass-through block that the comment at `detcore/src/syscall_classification.rs:691` calls "conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions", under the `TODO-HUMAN-REVIEW(PR-889)` note at `detcore/src/syscall_classification.rs:708`. It is listed in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1748`, charged the fast virtual-time cost at `detcore/src/syscall_time.rs:45`, and forwarded by the generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843`. Unlike its neighbours in the same block, it is not in the pinned `PassThrough` list of `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`).

The 2026-08-07 strategy draft (kept under Additional information) proposed holding the alternate stack in Detcore's logical thread and building signal frames from that model. That does not fit the ptrace backend. Detcore holds no alternate-stack state: in the export, `sigaltstack` appears in `detcore/src` only at the classification and timing lines above. On ptrace the host kernel delivers each signal that Hermit lets through and builds the frame from its own copy of the setting; a Detcore copy could only mirror the kernel, and any difference would make `old_ss` disagree with where handlers actually run. Every input is guest-controlled and Hermit already serializes signal timing, so forwarding gives the same answer on every run of the same host. `emulate` or `model` would be needed only on a backend whose personality builds signal frames itself: the backend-parity notes say the KVM personality "does not yet synthesize an x86-64 signal frame" (`tests/backend-parity/README.md:119`) and list `sigaltstack_state` as a KVM gap (`tests/backend-parity/README.md:159`). That is backend work, not a change to this pass-through.

Two cross-host limits stay outside this policy: frame size, and the strict size check (F8). They are reproducible on one host; making them identical across hosts would mean controlling the XSAVE feature set the host kernel exposes, which Hermit's CPUID virtualization does not claim here.

Existing evidence:

- `tests/c/sigaltstack_state.c` queries (`tests/c/sigaltstack_state.c:28`), installs (`tests/c/sigaltstack_state.c:36`), re-queries (`tests/c/sigaltstack_state.c:41`) and disables (`tests/c/sigaltstack_state.c:49`) without delivering a signal. Its cell `c-programs/sigaltstack-state` (`tests/e2e/manifests/c-programs.yaml:18169`) runs verify in CI on ptrace and dbt and expects `sigaltstack ok=4`. This covers F1, F2, F3 and part of F10.
- `tests/c/signal_determinism.c:754` (`test_altstack_preservation`) installs a stack, delivers two signals to an `SA_ONSTACK` handler that checks its own address lies inside the alternate stack (`tests/c/signal_determinism.c:740`), then queries and restores. `alternate_signal_stack_is_preserved` (`hermit-cli/tests/signal_determinism.rs:400`) runs that scenario repeatedly on the default backend and compares outputs, without `--verify`, and the file builds `--backend` with `format!` (`hermit-cli/tests/signal_determinism.rs:191`). The manifest cell `c-programs/signal-determinism` (`tests/e2e/manifests/c-programs.yaml:8132`) runs only the `itimer-delivery` scenario, so this F12 evidence is not a manifest verify cell.
- `rr_sigaltstack` (`hermit-cli/tests/rr_suite.rs:448`) is `#[ignore]` like every rr test (`hermit-cli/tests/rr_suite.rs:233`).

No test covers F4 through F9 or F11.

## Recommended tests

- T1 (covers F1, F2, F3, F10): extend the `c-programs/sigaltstack-state` pattern: query before install, install a fixed 64 KiB static stack, query and assert the exact `ss_sp`, `ss_size` and flags 0, disable, and assert `SS_DISABLE` with zero pointer and size; run under `hermit run --verify` on ptrace.
- T2 (covers F4, F5, F6): assert `ss_flags = SS_ONSTACK` installs like 0; assert `SS_AUTODISARM` is reported back in `old_ss`, and that a query from inside an `SA_ONSTACK` handler shows the setting cleared (flags `SS_DISABLE`) while the handler runs and restored after it returns; assert `ss_flags = 0x10` returns `EINVAL`.
- T3 (covers F7, F10, F12): from an `SA_ONSTACK` handler delivered by `raise`, assert the handler's local address lies inside the alternate stack, a query reports `SS_ONSTACK`, and an install of a different stack returns `EPERM` even with an invalid mode; after return, assert a handler without `SA_ONSTACK` runs on the normal stack. Run under `--verify` on ptrace.
- T4 (covers F8): assert `ss_size = MINSIGSTKSZ - 1` returns `ENOMEM`, `ss_size = MINSIGSTKSZ` with a 2048-byte buffer either succeeds or returns `ENOMEM` identically across `--verify` runs (the strict frame check is host-dependent), and resubmitting the current setting returns 0.
- T5 (covers F9): assert `sigaltstack((stack_t *)1, NULL)` and `sigaltstack(NULL, (stack_t *)1)` each return `EFAULT`, and that the setting is unchanged afterwards.
- T6 (covers F11): install a stack, then assert by query that a `fork` child reports it, a new `pthread` reports `SS_DISABLE`, and an `execve`d image reports `SS_DISABLE`.
- U1 (covers F13; host-config): the CI host's XSAVE feature set and kernel configuration decide whether a stack between `MINSIGSTKSZ` and the frame size is rejected, so CI cannot produce both outcomes. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:713`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. sigaltstack is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1955: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept linux_determinism conditional (guest-controlled per-thread state; signal timing fixed by Hermit; frame size and the strict size check depend on host XSAVE features); target_policy investigate -> passthrough (on ptrace the kernel keeps the setting and builds the frames, and Detcore has no alternate-stack state, so the 2026-08-07 "model in the logical thread" draft is rejected for ptrace); tracking_status todo: F4-F9 and F11 have no test, F12 evidence (hermit-cli/tests/signal_determinism.rs:400) is not a verify cell; 12 flags; 6 tests; no coverage token recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host XSAVE features and kernel configuration set the signal-frame size); F8 split, new F13 (frame-size ENOMEM) with U1 host-config; no open questions


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sigaltstack`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sigaltstack&body=Discussion+for+the+%60sigaltstack%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsigaltstack.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sigaltstack%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: sigaltstack" (https://github.com/rrnewton/hermit/issues/1955), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

sigaltstack is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.

### Legacy strategy draft (2026-08-07)

Represent alternate-stack configuration and on-stack state in the logical thread and have deterministic signal delivery construct frames from that model. Validate ranges and flags without delegating state ownership to the host.

### Legacy required tests (2026-08-07)

- Cover install, query, disable, replacement, invalid flags/sizes/pointers, and changes while on-stack.
- Deliver nested and queued signals and verify selected stack, frame addresses, masks, and restoration.
- Exercise clone/exec inheritance and interaction with signal return.
- Compare strict repeats and record/replay across every signal-capable backend.

### Legacy acceptance text (2026-08-07)

Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.
