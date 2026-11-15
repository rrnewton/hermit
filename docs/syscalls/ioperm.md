---
schema: syscall-pages/v1
syscall: ioperm
nr_x86_64: 173
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/617
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ioperm&body=Discussion+for+the+%60ioperm%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fioperm.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ioperm%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_privileged_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`ioperm(from, num, turn_on)` grants or revokes the calling thread's permission to use x86 I/O ports `from` to `from + num - 1` with the `in` and `out` instructions. Hermit refuses it: Detcore answers every call with a fixed `EPERM` and never forwards it to the host kernel (`detcore/src/lib.rs:2164`), so the guest never gets port access.

Manual reference: Linux man-pages ioperm(2).

## Linux semantics

The kernel keeps a per-thread I/O permission bitmap covering ports 0 to 65535. `ksys_ioperm` checks the arguments in this order:

1. The range: `num == 0`, an overflow of `from + num`, or `from + num > 65536` gives `EINVAL`.
2. Enabling (`turn_on != 0`) needs `CAP_SYS_RAWIO` in the initial user namespace and no kernel lockdown of I/O ports, otherwise `EPERM`.
3. Disabling needs no capability. If the thread has no bitmap yet, the call returns 0 without allocating one; otherwise it clears the bits.

The permission is consumed by later `in`/`out` instructions, not by syscalls. An access to a port the thread has not been granted raises a general-protection fault, which the kernel delivers as `SIGSEGV`. A kernel built without `CONFIG_X86_IOPL_IOPERM` has no `ioperm` and returns `ENOSYS`. `iopl(2)`, the all-ports variant, is a separate syscall in the same Hermit refusal set and has its own tracking issue.

### Flags and options

- F1: `turn_on != 0`: grant access to the range. Needs `CAP_SYS_RAWIO` in the initial user namespace and no I/O-port lockdown; otherwise `EPERM`.
- F2: `turn_on == 0`: revoke access to the range. Needs no capability and returns 0, including when the thread holds no bitmap.
- F3: range check: `num == 0`, an overflow of `from + num`, or `from + num > 65536` gives `EINVAL`. It runs before the capability check, so it applies to unprivileged callers too.
- F4: later port I/O: an `in` or `out` on a port the thread was not granted faults, and the kernel delivers `SIGSEGV`.
- F5: kernel built without `CONFIG_X86_IOPL_IOPERM`: `ENOSYS` for every call.

## Determinism analysis

`linux_determinism: conditional`, kept from the migration. For a caller without `CAP_SYS_RAWIO` in the initial user namespace, which includes root inside a container's user namespace, the result is a function of the arguments alone: `EINVAL` for a bad range (F3), 0 for a revoke (F2), `EPERM` for a grant (F1). That repeats exactly. With the capability, a successful grant hands the thread real hardware ports, and the values later read from them are host hardware state.

### Nondeterminism sources

- Host privilege and lockdown policy (F1)
- Host kernel configuration (F5)
- Values read from real hardware ports after a successful grant (F4)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it. `classify_syscall` lists `ioperm` as Determinized with the rest of the privileged system-administration family (`detcore/src/syscall_classification.rs:289` to `detcore/src/syscall_classification.rs:315`). `Detcore::handle_syscall_event` then matches it on the `Sysno`, before the typed match, with the guard `is_privileged_admin_refused_syscall` (`detcore/src/lib.rs:2164` to `detcore/src/lib.rs:2168`; the set itself is at `detcore/src/syscall_classification.rs:902`), and returns `Err(Error::Errno(Errno::EPERM))` (`detcore/src/lib.rs:2167`). The arm reads no argument and never forwards the call to the host kernel. It does not consult `panic_on_unsupported_syscalls`, so the answer is the same with and without fail-closed mode, and the comment above the arm (`detcore/src/lib.rs:2154`) states the intent that it be identical across `--verify` and record/replay. The same set feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), which is what `code_facts.refused: true` reports.

Classification-only evidence: the unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks that `ioperm` is Determinized and in the refusal set, and that the set holds nothing else. It does not start Hermit, so it is not coverage for any test below.

How close this is to Linux: Hermit matches Linux for the case that matters, a grant (F1), which an unprivileged caller also gets `EPERM` for. It differs for three argument-dependent cases, all of them deterministic in both worlds:

- F2: Linux returns 0 for a revoke without any capability; Hermit returns `EPERM`.
- F3: Linux returns `EINVAL` for a bad range; Hermit returns `EPERM`.
- F5: on a host kernel without `CONFIG_X86_IOPL_IOPERM`, Linux returns `ENOSYS`; Hermit returns `EPERM`, which keeps the result independent of the host kernel.

F4 needs no handler. The call never reaches the kernel, so the guest thread's bitmap is never set, and any `in`/`out` it executes faults with `SIGSEGV` exactly as it would natively without permission.

Why `refuse`:

- Under ptrace the guest runs as root inside the container's user namespace (the comment at `detcore/src/syscall_classification.rs:2298` to `detcore/src/syscall_classification.rs:2303` describes the `CLONE_NEWUSER` uid map). Linux checks `CAP_SYS_RAWIO` against the initial user namespace, which such a root never holds, so a fixed `EPERM` is what the guest would see from the kernel in that container anyway. The refusal removes the dependency on how Hermit itself was launched.
- `passthrough` would let a guest change host-global kernel state if Hermit ever ran with the capability, which is an isolation hole, not a determinism feature.
- `model` or `emulate` would mean granting port access that the guest could then use with real `in`/`out` instructions. Hermit cannot virtualize those instructions on the ptrace backend, so an emulated grant would lie, and a real one would expose host hardware.

Target (owner decision 6): a revoke (`turn_on == 0`) with a valid range returns 0 and changes nothing, and every other call gets the fixed `EPERM`, returned before any argument check. A revoke is one of the decision's named exceptions, because Linux allows it unprivileged: `ksys_ioperm()` (`arch/x86/kernel/ioport.c`) checks the range first (`from + num <= from` or `from + num > IO_BITMAP_BITS` gives `EINVAL`), then refuses a grant without `capable(CAP_SYS_RAWIO)` or under `LOCKDOWN_IOPORT` lockdown with `EPERM`, then returns 0 for a revoke when the thread has no I/O bitmap. Under Hermit the guest never gets a bitmap, because a grant is never forwarded, so a valid-range revoke is exactly that "no bitmap" case (F2) and needs no state. A bad range (F3) gets `EPERM`, not Linux's `EINVAL`, for a grant and for a revoke alike: Linux does not allow a bad-range revoke either, so it is not an unprivileged success, and the decision keeps the fixed errno ahead of argument checks. The 0 is computed in Detcore, never forwarded, so it does not depend on the host's `CONFIG_X86_IOPL_IOPERM` (F5). Today the arm at `detcore/src/lib.rs:2164-2168` returns `EPERM` for a revoke too; the change is an `ioperm`-specific check that returns 0 when `turn_on == 0` and `num != 0` and `from + num` neither overflows nor exceeds 65536. A program that drops port access defensively with `ioperm(from, num, 0)` then sees the success it sees natively.

The code realizes `refuse` except for that revoke. The other gap is evidence: no test names `SYS_ioperm` (`tests_naming: 0`), so verification on the default backend stays untested.

## Recommended tests

- T1 (covers F1, F2, F3, F5): under `hermit run --verify` on ptrace, call raw `syscall(SYS_ioperm, ...)` with `(0x80, 1, 1)`, `(0x80, 1, 0)`, `(0, 0, 1)`, `(65535, 2, 1)` and `(0, 0, 0)`. The target (owner decision 6) is 0 for the valid revoke `(0x80, 1, 0)`, the unprivileged success Linux allows, and -1 with `errno == EPERM` for the grant and for every bad range, including the bad-range revoke `(0, 0, 0)`, where Linux returns `EINVAL`; both verify runs must print identical output. Hermit at f4055eb9cae4 returns `EPERM` for the revoke too, so the test fails until the revoke exception lands.
- T2 (covers F4): after the refused grant, install a `SIGSEGV` handler that records the signal and exits normally, execute `inb` on port `0x80`, and assert the handler ran. The handler keeps the guest from dying by a signal, so `--verify` can compare the runs.
- T3 (covers F1): record a guest that calls `ioperm` under `hermit record`, replay it, and assert the replay returns `EPERM` at the same point and the run reproduces.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/617: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (code returns a fixed EPERM before reading arguments, detcore/src/lib.rs:2164-2168; matches Linux for an unprivileged grant, differs for a revoke (Linux 0) and a bad range (Linux EINVAL)); linux_determinism stays conditional; 5 flag behaviours and 3 tests listed; open question on mirroring Linux's argument checks; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied: target is a valid-range revoke returns 0 (Linux-unprivileged exception) and every other call the fixed EPERM before any argument check, so a bad range gets EPERM, not EINVAL; code today returns EPERM for the revoke (detcore/src/lib.rs:2164-2168), so a change is needed; target_policy stays refuse; T1 now asserts 0 for the revoke and EPERM otherwise, with a bad-range revoke added; open question removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `ioperm`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ioperm&body=Discussion+for+the+%60ioperm%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fioperm.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ioperm%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: ioperm" (https://github.com/rrnewton/hermit/issues/617), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
