---
schema: syscall-pages/v1
syscall: kexec_load
nr_x86_64: 246
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/623
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+kexec_load&body=Discussion+for+the+%60kexec_load%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fkexec_load.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60kexec_load%60%2C+or+in+what+the+page+says%3F%0A
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

`kexec_load(entry, nr_segments, segments, flags)` stages a new kernel image in memory, for a later `reboot(LINUX_REBOOT_CMD_KEXEC)` or, with `KEXEC_ON_CRASH`, for the crash path. Hermit refuses it: Detcore answers every call with a fixed `EPERM` and never forwards it to the host kernel (`detcore/src/lib.rs:2164`).

Manual reference: Linux man-pages kexec_load(2).

## Linux semantics

The kernel's first check is `kexec_load_permitted()`: the caller needs `CAP_SYS_BOOT` in the initial user namespace, and the `kernel.kexec_load_disabled` sysctl (and the per-type load limits of recent kernels) must still allow a load. Otherwise the call returns `EPERM` before it looks at `flags`, `nr_segments` or `segments`. Kernel lockdown also forbids `kexec_load` with `EPERM`, because it loads an unverified image. Without `CONFIG_KEXEC` the syscall does not exist and returns `ENOSYS`.

Past the permission checks, the kernel validates `flags` and the architecture field, limits `nr_segments` to `KEXEC_SEGMENT_MAX` (16), takes the global kexec lock (`EBUSY` if another load holds it), copies the segments into the destination memory ranges (the crash kernel must fit the memory reserved with `crashkernel=`), and replaces any image previously staged. `nr_segments == 0` unloads the staged image instead. Nothing runs until a later `reboot` or a crash; the effect is host-global.

### Flags and options

- F1: privilege gate: without `CAP_SYS_BOOT` in the initial user namespace, when `kernel.kexec_load_disabled` forbids loads, or under kernel lockdown, the call returns `EPERM` before any other argument is checked.
- F2: `KEXEC_ON_CRASH` (0x1): stage the image as the crash kernel, run on a panic. The segments must lie inside the reserved crash memory.
- F3: `KEXEC_PRESERVE_CONTEXT` (0x2): preserve hardware and software state across the kexec jump, so the new kernel can return.
- F4: `KEXEC_UPDATE_ELFCOREHDR` (0x4): the crash image's ELF core header may be updated by the kernel on CPU or memory hotplug.
- F5: `KEXEC_CRASH_HOTPLUG_SUPPORT` (0x8): the crash image supports hotplug updates (recent kernels).
- F6: architecture field (`flags & KEXEC_ARCH_MASK`): must be `KEXEC_ARCH_DEFAULT` or the native `KEXEC_ARCH_X86_64`; any other value gives `EINVAL`.
- F7: any other flag bit: `EINVAL`, checked after F1.
- F8: `nr_segments`: more than `KEXEC_SEGMENT_MAX` (16) gives `EINVAL`; 0 unloads the currently staged image of that type.
- F9: `segments`: a bad pointer gives `EFAULT`; overlapping or out-of-range destinations give `EINVAL` or `EADDRNOTAVAIL`. Reachable only past F1.
- F10: concurrency: another load or unload in progress gives `EBUSY`.
- F11: kernel built without `CONFIG_KEXEC`: `ENOSYS` for every call.

## Determinism analysis

`linux_determinism: conditional`, kept from the migration. For a caller without `CAP_SYS_BOOT` in the initial user namespace, which includes root inside a container's user namespace, Linux returns `EPERM` before it examines any argument (F1), so the call repeats exactly. With the capability, the outcome depends on host state.

### Nondeterminism sources

- Host kernel configuration (F11), the `kernel.kexec_load_disabled` sysctl and lockdown state (F1)
- The memory reserved for a crash kernel and the physical memory layout (F2, F9)
- A previously staged image and concurrent loaders (F8, F10)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it. `classify_syscall` lists `kexec_load` as Determinized with the rest of the privileged system-administration family (`detcore/src/syscall_classification.rs:289` to `detcore/src/syscall_classification.rs:315`). `Detcore::handle_syscall_event` then matches it on the `Sysno`, before the typed match, with the guard `is_privileged_admin_refused_syscall` (`detcore/src/lib.rs:2164` to `detcore/src/lib.rs:2168`; the set itself is at `detcore/src/syscall_classification.rs:902`), and returns `Err(Error::Errno(Errno::EPERM))` (`detcore/src/lib.rs:2167`). The arm reads no argument and never forwards the call to the host kernel. It does not consult `panic_on_unsupported_syscalls`, so the answer is the same with and without fail-closed mode, and the comment above the arm (`detcore/src/lib.rs:2154`) states the intent that it be identical across `--verify` and record/replay. The same set feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), which is what `code_facts.refused: true` reports.

Classification-only evidence: the unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks that `kexec_load` is Determinized and in the refusal set, and that the set holds nothing else. It does not start Hermit, so it is not coverage for any test below.

How close this is to Linux: the permission check is the kernel's first one, so for a caller without `CAP_SYS_BOOT` Linux itself returns `EPERM` for every argument value. Hermit's answer matches that for F1 to F10. The one difference is F11: on a host kernel without `CONFIG_KEXEC`, Linux gives `ENOSYS` and Hermit still gives `EPERM`. That makes the guest's result independent of the host kernel's configuration, which is the point of the refusal.

Why `refuse`:

- Under ptrace the guest runs as root inside the container's user namespace (the comment at `detcore/src/syscall_classification.rs:2298` to `detcore/src/syscall_classification.rs:2303` describes the `CLONE_NEWUSER` uid map). Linux checks `CAP_SYS_BOOT` against the initial user namespace, which such a root never holds, so a fixed `EPERM` is what the guest would see from the kernel in that container anyway. The refusal removes the dependency on how Hermit itself was launched.
- `passthrough` would let a guest change host-global kernel state if Hermit ever ran with the capability, which is an isolation hole, not a determinism feature.
- `model` or `emulate` would mean pretending that a replacement kernel was staged. A guest can never act on it: the matching `reboot(LINUX_REBOOT_CMD_KEXEC)` is refused by the same arm, and a container has no kernel to replace, so an emulated success would only report a state that does not exist.

Nothing is missing in the code. The gap is evidence: no test names `SYS_kexec_load` (`tests_naming: 0`), so verification on the default backend stays untested.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11): under `hermit run --verify` on ptrace, call raw `syscall(SYS_kexec_load, ...)` with `nr_segments = 0` and each of these `flags`: 0, `KEXEC_ON_CRASH`, `KEXEC_PRESERVE_CONTEXT`, `KEXEC_UPDATE_ELFCOREHDR`, `KEXEC_CRASH_HOTPLUG_SUPPORT`, `KEXEC_ARCH_X86_64`, an unsupported architecture value (`KEXEC_ARCH_ARM`), and an undefined bit (`0x10`). Add `nr_segments = 17`, and `nr_segments = 1` with `segments = (void *)1`. Assert every call returns -1 with `errno == EPERM` and both verify runs print identical output. Two back-to-back calls cover the F10 case only in the sense that neither returns `EBUSY`; the test should say so. F11 is covered by asserting `EPERM`, not `ENOSYS`, on whatever host CI runs.
- T2 (covers F1): record a guest that calls `kexec_load` under `hermit record`, replay it, and assert the replay returns `EPERM` at the same point and the run reproduces.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/623: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (code returns a fixed EPERM before reading arguments, detcore/src/lib.rs:2164-2168; Linux's first check is the same EPERM without CAP_SYS_BOOT; only a no-CONFIG_KEXEC host differs, ENOSYS natively); linux_determinism stays conditional; 11 flag behaviours and 2 tests listed; no test names the syscall, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `kexec_load`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+kexec_load&body=Discussion+for+the+%60kexec_load%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fkexec_load.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60kexec_load%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: kexec_load" (https://github.com/rrnewton/hermit/issues/623), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
