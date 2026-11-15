---
schema: syscall-pages/v1
syscall: init_module
nr_x86_64: 175
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/610
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+init_module&body=Discussion+for+the+%60init_module%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finit_module.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60init_module%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_privileged_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`init_module(module_image, len, param_values)` loads a kernel module from an ELF image in the caller's memory, runs its init function, and leaves it loaded host-wide. Hermit refuses it: Detcore answers every call with a fixed `EPERM` and never forwards it to the host kernel (`detcore/src/lib.rs:2164`).

Manual reference: Linux man-pages init_module(2).

## Linux semantics

The kernel's first check is `may_init_module()`: the caller needs `CAP_SYS_MODULE` in the initial user namespace, and the `kernel.modules_disabled` sysctl must be 0. Otherwise the call returns `EPERM` before it reads `module_image`, `len` or `param_values`, so an unprivileged caller gets `EPERM` for every argument value.

With the capability, the kernel copies the image, checks it against the module signature policy and kernel lockdown, validates the ELF file and its version magic against the running kernel, resolves its undefined symbols against the symbols currently exported by the kernel and other loaded modules, parses the parameters, and runs the module's init function. A successful load is global: every process on the host sees the module in `/proc/modules` and whatever the module does. Two loads of the same module name serialize; the second waits for the first and then fails with `EEXIST` if the first succeeded.

### Flags and options

- F1: privilege gate: without `CAP_SYS_MODULE` in the initial user namespace, or with `kernel.modules_disabled=1`, the call returns `EPERM`. This check runs before any argument is read.
- F2: `module_image` and `len`: the image is copied from user memory and validated. A bad pointer gives `EFAULT`, a malformed ELF file or a version-magic mismatch gives `ENOEXEC`, and a signature the kernel's policy rejects gives `EKEYREJECTED` (or `EPERM` under lockdown). These errors are reachable only past F1.
- F3: `param_values`: a NUL-terminated string of `name=value` assignments parsed against the module's declared parameters. A malformed value fails the load; an unknown name is logged and ignored.
- F4: name collision: a module of the same name already loaded gives `EEXIST`; a concurrent load of the same name waits for the first one to finish.
- F5: success: the module's init function runs in kernel context. A negative return from it becomes the syscall's error; on success the module stays loaded for the whole host.

## Determinism analysis

`linux_determinism: conditional`, kept from the migration. For a caller without `CAP_SYS_MODULE` in the initial user namespace, which includes root inside a container's user namespace, the result is always `EPERM` (F1), whatever the arguments: it repeats. With the capability, the result depends on host state the guest does not control.

### Nondeterminism sources

- The running kernel's version and configuration, which decide version-magic and symbol resolution (F2)
- Modules already loaded, and other processes loading the same name at the same time (F4)
- Host signature and lockdown policy, and the `kernel.modules_disabled` sysctl (F1, F2)
- Whatever the module's init function does with hardware or other kernel state (F5)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it. `classify_syscall` lists `init_module` as Determinized with the rest of the privileged system-administration family (`detcore/src/syscall_classification.rs:289` to `detcore/src/syscall_classification.rs:315`). `Detcore::handle_syscall_event` then matches it on the `Sysno`, before the typed match, with the guard `is_privileged_admin_refused_syscall` (`detcore/src/lib.rs:2164` to `detcore/src/lib.rs:2168`; the set itself is at `detcore/src/syscall_classification.rs:902`), and returns `Err(Error::Errno(Errno::EPERM))` (`detcore/src/lib.rs:2167`). The arm reads no argument and never forwards the call to the host kernel. It does not consult `panic_on_unsupported_syscalls`, so the answer is the same with and without fail-closed mode, and the comment above the arm (`detcore/src/lib.rs:2154`) states the intent that it be identical across `--verify` and record/replay. The same set feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), which is what `code_facts.refused: true` reports.

Classification-only evidence: the unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks that `init_module` is Determinized and in the refusal set, and that the set holds nothing else. It does not start Hermit, so it is not coverage for any test below.

How close this is to Linux: because `may_init_module()` is the kernel's first check, Linux itself returns `EPERM` for every argument value when the caller lacks the capability. Hermit's fixed answer therefore matches what an unprivileged caller sees natively for F1 to F5, including invalid pointers.

Why `refuse`:

- Under ptrace the guest runs as root inside the container's user namespace (the comment at `detcore/src/syscall_classification.rs:2298` to `detcore/src/syscall_classification.rs:2303` describes the `CLONE_NEWUSER` uid map). Linux checks `CAP_SYS_MODULE` against the initial user namespace, which such a root never holds, so a fixed `EPERM` is what the guest would see from the kernel in that container anyway. The refusal removes the dependency on how Hermit itself was launched.
- `passthrough` would let a guest change host-global kernel state if Hermit ever ran with the capability, which is an isolation hole, not a determinism feature.
- `model` or `emulate` would mean pretending that a kernel module was loaded or unloaded. A container has no kernel of its own, so there is no guest-visible state such an emulation could keep consistent (`/proc/modules`, the module's own side effects), and no workload that Hermit targets needs it.

Nothing is missing in the code. The gap is evidence: no test names `SYS_init_module` (`tests_naming: 0`), so verification on the default backend stays untested.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): under `hermit run --verify` on ptrace, call raw `syscall(SYS_init_module, ...)` with (a) a NULL image and length 0, (b) a small buffer that is not an ELF file, (c) a valid-looking pointer with an unmapped address and a non-empty `param_values` string. Assert each returns -1 with `errno == EPERM`, and that both verify runs print identical output. No case may change host state: a guest-side read of `/proc/modules` before and after must be equal.
- T2 (covers F1): record a guest that calls `init_module` under `hermit record`, replay it, and assert the replay returns `EPERM` at the same point and the run reproduces.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/610: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (code returns a fixed EPERM before reading arguments, detcore/src/lib.rs:2164-2168; Linux's first check is the same EPERM for a caller without CAP_SYS_MODULE); linux_determinism stays conditional; 5 flag behaviours and 2 tests listed; no test names the syscall, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `init_module`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+init_module&body=Discussion+for+the+%60init_module%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Finit_module.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60init_module%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: init_module" (https://github.com/rrnewton/hermit/issues/610), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
