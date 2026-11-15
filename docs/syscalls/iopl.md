---
schema: syscall-pages/v1
syscall: iopl
nr_x86_64: 172
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/618
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+iopl&body=Discussion+for+the+%60iopl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fiopl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60iopl%60%2C+or+in+what+the+page+says%3F%0A
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

`iopl(level)` changes the calling thread's I/O privilege level, which on x86 decides whether user code may access I/O ports directly. Hermit refuses it: Detcore returns a fixed `EPERM` for every `level` without forwarding the call (`detcore/src/lib.rs:2164`), which differs from Linux for `iopl(0)` (native success) and `level > 3` (native `EINVAL`).

Manual reference: [iopl(2)](https://man7.org/linux/man-pages/man2/iopl.2.html).

## Linux semantics

The kernel checks in this order:

1. `level > 3`: `EINVAL`.
2. `level` equal to the thread's current level: return 0 and change nothing. A normal process runs at level 0, so `iopl(0)` succeeds for any caller.
3. Raising the level requires `CAP_SYS_RAWIO`, checked with `capable()` in the initial user namespace, and a kernel that is not locked down; otherwise `EPERM`. Lowering is always allowed.

A 2026-10-03 probe on Linux 7.1.3, unprivileged, returned `iopl(0) = 0`, `iopl(4) = -1 EINVAL` and `iopl(3) = -1 EPERM`.

Since Linux 5.5 the kernel no longer changes the real `EFLAGS.IOPL` bits: level 3 is emulated by granting the thread access to all 65536 I/O ports through its I/O bitmap, so `cli`/`sti` still fault; levels 1 and 2 grant no port access. The level is per-thread state inherited by children. A kernel built without `CONFIG_X86_IOPL_IOPERM` returns `ENOSYS` for every call.

### Flags and options

- F1: `level > 3`: `EINVAL`, checked first.
- F2: `level` equal to the current level (0 for a normal process): returns 0 with no change, without any capability.
- F3: raising the level without `CAP_SYS_RAWIO` in the initial user namespace, or under kernel lockdown: `EPERM`.
- F4: lowering the level: always allowed; returns 0.
- F5: successful raise to 3 by a privileged caller: the thread may access every I/O port; levels 1 and 2 grant no port access on Linux 5.5 and later.
- F6: kernel built without `CONFIG_X86_IOPL_IOPERM`: `ENOSYS` for every `level`.

## Determinism analysis

`linux_determinism: conditional`. The result is a function of `level`, the current level inherited from the parent, the caller's `CAP_SYS_RAWIO`, the kernel's lockdown state and its build configuration. For an unprivileged process on a given kernel it is fixed (`iopl(0)` succeeds, `level > 3` is `EINVAL`, 1 to 3 is `EPERM`), but the same call gives `ENOSYS` on a kernel built without I/O port support. A successful raise lets the guest read and write hardware ports whose state is outside any deterministic model.

### Nondeterminism sources

- `CAP_SYS_RAWIO` in the initial user namespace, and kernel lockdown
- The kernel build option `CONFIG_X86_IOPL_IOPERM`
- The I/O privilege level inherited from the parent
- After a successful raise, hardware I/O port state

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the dispatch arm at `detcore/src/lib.rs:2164-2168` returns `Err(Error::Errno(Errno::EPERM))` for every syscall in `is_privileged_admin_refused_syscall` (`detcore/src/syscall_classification.rs:902`), which lists `iopl` among 16 privileged administration calls. The arm runs before the typed match and before any `record_or_replay` call: it ignores `level`, never forwards the call, and record, replay and both `--verify` runs compute the same `EPERM`. The refusal applies in every mode, not only when Hermit fails closed. The rationale comments are at `detcore/src/lib.rs:2154-2163` and `detcore/src/syscall_classification.rs:889-901`, tagged `TODO-HUMAN-REVIEW(#722)` (https://github.com/rrnewton/hermit/issues/722).

Why `refuse`:

- Raw I/O port access reaches hardware state that no deterministic model covers, and a guest given it could affect the host.
- `CAP_SYS_RAWIO` is checked in the initial user namespace, so `EPERM` for a raise is what Linux returns to the identity Hermit presents (virtual uid 0 inside a user namespace, `detcore/src/syscall_classification.rs:657-668`).
- The fixed result removes the host's build-configuration dependence (F6).

Target (owner decision 6): `iopl(0)` returns 0 and changes nothing, and every other `level` gets the fixed `EPERM`, returned before any argument check. `iopl(0)` is one of the decision's named exceptions, because Linux allows it unprivileged: in `SYSCALL_DEFINE1(iopl)` (`arch/x86/kernel/ioport.c`) the order is `level > 3` gives `EINVAL`, then `level` equal to the thread's current level returns 0, then a raise needs `capable(CAP_SYS_RAWIO)` and no `LOCKDOWN_IOPORT` lockdown, else `EPERM`. Under Hermit the guest's level is always 0, because a raise is never granted, so `iopl(0)` is exactly the "equal to the current level" case (F2) and needs no per-thread state. `level > 3` gets `EPERM`, not Linux's `EINVAL` (F1): the decision keeps the fixed errno ahead of argument checks everywhere except the unprivileged successes. The 0 is computed in Detcore, never forwarded, so it does not depend on the host's `CONFIG_X86_IOPL_IOPERM` (F6). F4 cannot arise, because the level never rises above 0. Today the arm at `detcore/src/lib.rs:2164-2168` returns `EPERM` for `iopl(0)` too; the change is an `iopl`-specific arm ahead of it (or a check inside it) that returns 0 when `level == 0`.

The unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks only that `iopl` is classified Determinized and is in the refusal set; it does not execute the syscall, and as a unit test it cannot serve as a coverage path.

## Recommended tests

- T1 (covers F1, F2, F4): under `hermit run --verify` on ptrace, a guest calls `iopl(0)`, then `iopl(4)`, then `iopl(0)` again, and prints each result. The target (owner decision 6) is 0, -1 with `EPERM`, and 0: the unprivileged success Linux allows, then the fixed errno where Linux returns `EINVAL`. Hermit at f4055eb9cae4 returns `EPERM` for all three, so the test fails until the `iopl(0)` exception lands. Because the native result for `iopl(4)` is `EINVAL`, this test also tells the refusal from forwarding.
- T2 (covers F3, F5): `iopl(1)`, `iopl(2)` and `iopl(3)`, from the guest and from a child it forks, must each return -1 with `EPERM`, and both runs must print the same output.
- T3 (covers F6): the T1 and T2 results must not depend on the host: Hermit never returns `ENOSYS`, whatever the host kernel's `CONFIG_X86_IOPL_IOPERM` setting.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/618: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (fixed EPERM for every level at detcore/src/lib.rs:2164-2168, never forwarded); linux_determinism stays conditional; 6 flag behaviours and 3 tests listed; open question whether iopl(0) should succeed and level > 3 give EINVAL as on Linux; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied: target is iopl(0) returns 0 (Linux-unprivileged exception) and every other level the fixed EPERM before any argument check, so level > 3 gets EPERM, not EINVAL; code today returns EPERM for iopl(0) (detcore/src/lib.rs:2164-2168), so a change is needed; target_policy stays refuse; T1 now asserts 0, EPERM, 0; open question removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `iopl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+iopl&body=Discussion+for+the+%60iopl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fiopl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60iopl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: iopl" (https://github.com/rrnewton/hermit/issues/618), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### 2026-10-03 host probe (syscall-tracking-p3)

Linux 7.1.3, uid 212630 with an empty effective capability set, raw `syscall(172, level)` through Python ctypes:

```text
iopl(0) = 0
iopl(4) = -1 EINVAL
iopl(3) = -1 EPERM
```
