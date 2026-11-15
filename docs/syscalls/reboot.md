---
schema: syscall-pages/v1
syscall: reboot
nr_x86_64: 169
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/396
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+reboot&body=Discussion+for+the+%60reboot%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freboot.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60reboot%60%2C+or+in+what+the+page+says%3F%0A
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

`reboot(magic1, magic2, cmd, arg)` restarts, halts or powers off the machine, switches Ctrl-Alt-Del handling, or starts kexec or hibernation; in a child PID namespace it terminates that namespace instead. Hermit refuses it: Detcore returns a fixed `EPERM` without forwarding the call or reading its arguments (`detcore/src/lib.rs:2164`), so a guest can never reboot the host.

Manual reference: [reboot(2)](https://man7.org/linux/man-pages/man2/reboot.2.html).

## Linux semantics

The kernel checks in this order:

1. `CAP_SYS_BOOT` in the user namespace that owns the caller's PID namespace; otherwise `EPERM`. This comes before the magic numbers are examined.
2. `magic1` must be `LINUX_REBOOT_MAGIC1` (0xfee1dead) and `magic2` one of `LINUX_REBOOT_MAGIC2` (672274793), `MAGIC2A` (85072278), `MAGIC2B` (369367448) or `MAGIC2C` (537993216); otherwise `EINVAL`.
3. In a child PID namespace the command is handled by the namespace (see F9). In the initial PID namespace it acts on the whole machine.

The legacy sweep did not invoke `reboot` for safety, and neither did the 2026-10-03 probes for this body; the order above is from the kernel source, and the legacy report records that an unprivileged container receives `EPERM`.

### Flags and options

- F1: magic numbers: a wrong `magic1` or `magic2` gives `EINVAL`, for a privileged caller only.
- F2: `LINUX_REBOOT_CMD_RESTART` (0x01234567): restart the machine.
- F3: `LINUX_REBOOT_CMD_RESTART2` (0xa1b2c3d4): restart with the command string in `arg`.
- F4: `LINUX_REBOOT_CMD_HALT` (0xcdef0123): halt the machine.
- F5: `LINUX_REBOOT_CMD_POWER_OFF` (0x4321fedc): power off, or halt if the machine cannot power off.
- F6: `LINUX_REBOOT_CMD_CAD_ON` (0x89abcdef) and `LINUX_REBOOT_CMD_CAD_OFF` (0): make Ctrl-Alt-Del reboot immediately, or send `SIGINT` to init instead. This is host-global state.
- F7: `LINUX_REBOOT_CMD_KEXEC` (0x45584543): boot a kernel previously loaded with `kexec_load`; `EINVAL` when none is loaded.
- F8: `LINUX_REBOOT_CMD_SW_SUSPEND` (0xd000fce2): hibernate; fails when hibernation is unavailable. An unknown `cmd` gives `EINVAL`.
- F9: child PID namespace: `RESTART` and `RESTART2` kill the namespace's init with `SIGHUP`, `HALT` and `POWER_OFF` with `SIGINT`, and the caller exits; any other command gives `EINVAL`.
- F10: no `CAP_SYS_BOOT` over the PID namespace: `EPERM`, before the magic numbers are checked.

## Determinism analysis

`linux_determinism: conditional`. For a caller without `CAP_SYS_BOOT` the result is always `EPERM` and nothing happens. A privileged call either ends the machine, which ends every process outside any deterministic schedule, changes host-global Ctrl-Alt-Del state, or, in a child PID namespace, kills that namespace's init at a time decided by host signal delivery. None of these can repeat under a deterministic run.

### Nondeterminism sources

- `CAP_SYS_BOOT` over the caller's PID namespace
- Whether the caller is in the initial PID namespace or a child one
- Host state: a loaded kexec image, hibernation support, power-off support
- A successful call in the initial PID namespace halts or reboots the host, which is catastrophic and must never be forwarded

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the dispatch arm at `detcore/src/lib.rs:2164-2168` returns `Err(Error::Errno(Errno::EPERM))` for every syscall in `is_privileged_admin_refused_syscall` (`detcore/src/syscall_classification.rs:902`), which lists `reboot` among 16 privileged administration calls. The arm runs before the typed match and before any `record_or_replay` call: it reads none of the four arguments, never forwards the call, and record, replay and both `--verify` runs compute the same `EPERM`. The refusal applies in every mode, not only when Hermit fails closed. The rationale comments are at `detcore/src/lib.rs:2154-2163` and `detcore/src/syscall_classification.rs:889-901`, tagged `TODO-HUMAN-REVIEW(#722)` (https://github.com/rrnewton/hermit/issues/722).

Why `refuse`:

- Forwarding is never safe: in the initial PID namespace a privileged success ends the host, and in a child PID namespace it kills the namespace's init, outside Detcore's scheduler.
- The kernel checks the capability before the magic numbers, so `EPERM` for every argument is exactly what Linux returns to a caller without `CAP_SYS_BOOT` over its PID namespace. Hermit presents virtual uid 0 (`detcore/src/syscall_classification.rs:657-668`); a uid 0 process whose PID namespace belongs to another user namespace gets the same `EPERM`, so the result is a coherent Linux state.
- `emulate` (for example, ending the guest as if the machine had restarted) would need a Hermit-level definition of what "reboot" means for a guest, and success would claim a capability the guest does not have.

The code already realizes the policy. The unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks only that `reboot` is classified Determinized and is in the refusal set; it does not execute the syscall, and as a unit test it cannot serve as a coverage path.

## Recommended tests

- T1 (covers F1, F10): under `hermit run --verify` on ptrace, a guest calls `reboot` with a wrong `magic1` and a valid command, and with a wrong `magic2`: each must return -1 with `EPERM`.
- T2 (covers F2, F3, F4, F5, F6, F7, F8, F9): run only under Hermit, never natively, and only on a CI host where the test user has no `CAP_SYS_BOOT` in the initial user namespace, so that even a regression to forwarding cannot reboot the host. With the valid magic numbers, the guest calls `reboot` with each command (`RESTART`, `RESTART2` with a short string, `HALT`, `POWER_OFF`, `CAD_ON`, `CAD_OFF`, `KEXEC`, `SW_SUSPEND`) and an unknown command. Each must return -1 with `EPERM`; afterwards the guest and a child it forked first must still be running (a forwarded `RESTART` in a child PID namespace whose user namespace grants `CAP_SYS_BOOT` would kill them, per F9), and the guest prints a marker that both runs must match.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/396: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (fixed EPERM at detcore/src/lib.rs:2164-2168, never forwarded; the kernel checks CAP_SYS_BOOT before the magic numbers, so EPERM matches Linux for every argument); linux_determinism stays conditional; 10 flag behaviours and 2 tests listed; no coverage token, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `reboot`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+reboot&body=Discussion+for+the+%60reboot%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freboot.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60reboot%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: reboot" (https://github.com/rrnewton/hermit/issues/396), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=reboot` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `reboot` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
(not invoked directly for safety; classification from man page + kernel behavior)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Note on the test hazard

`reboot` is the only syscall in this refusal set where a forwarding regression could take the CI host down. T1 and T2 are safe only because the CI user lacks `CAP_SYS_BOOT` in the initial user namespace; a reviewer adding them should confirm that for the runner.
