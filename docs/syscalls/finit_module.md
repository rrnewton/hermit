---
schema: syscall-pages/v1
syscall: finit_module
nr_x86_64: 313
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/355
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+finit_module&body=Discussion+for+the+%60finit_module%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffinit_module.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60finit_module%60%2C+or+in+what+the+page+says%3F%0A
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

`finit_module(fd, param_values, flags)` loads a kernel module whose image the kernel reads from the open file `fd`, runs its init function, and leaves it loaded host-wide. Hermit refuses it: Detcore answers every call with a fixed `EPERM` and never forwards it to the host kernel (`detcore/src/lib.rs:2164`).

Manual reference: Linux man-pages finit_module(2).

## Linux semantics

The kernel's first check is `may_init_module()`, as for `init_module`: the caller needs `CAP_SYS_MODULE` in the initial user namespace and `kernel.modules_disabled` must be 0, otherwise `EPERM`. Only after that does the kernel reject unknown `flags` bits (`EINVAL`) and look at `fd`. The host strace under Additional information shows the order: `fd = -1`, an all-ones `param_values` pointer and every undefined flag bit still give `EPERM`.

With the capability, the kernel reads the whole file through `fd`, optionally decompresses it, and then validates and loads it exactly as `init_module` does: signature and lockdown policy, ELF and version-magic checks, symbol resolution against the running kernel, parameter parsing, and the module's init function. The result is global to the host.

### Flags and options

- F1: privilege gate: without `CAP_SYS_MODULE` in the initial user namespace, or with `kernel.modules_disabled=1`, the call returns `EPERM` before `fd`, `param_values` or `flags` are examined.
- F2: `MODULE_INIT_IGNORE_MODVERSIONS` (1): skip the symbol version (modversions) checks.
- F3: `MODULE_INIT_IGNORE_VERMAGIC` (2): skip the kernel version-magic check.
- F4: `MODULE_INIT_COMPRESSED_FILE` (4): the file is compressed and the kernel decompresses it. A kernel built without in-kernel module decompression rejects it.
- F5: any other flag bit: `EINVAL`, checked after F1.
- F6: `fd`: not an open descriptor, or not open for reading, gives `EBADF`; a file that is open for writing elsewhere gives `ETXTBSY`. Reachable only past F1.
- F7: `param_values`: a NUL-terminated string of `name=value` assignments parsed against the module's declared parameters. A malformed value fails the load; an unknown name is logged and ignored.
- F8: image and load outcome: a malformed ELF or version mismatch gives `ENOEXEC`, a rejected signature `EKEYREJECTED`, a module of the same name already loaded `EEXIST`. On success the init function runs and the module stays loaded for the whole host.

## Determinism analysis

`linux_determinism: conditional`, changed from the legacy `nondeterministic`. The legacy classification described the privileged success path, which does depend on host state. For a caller without `CAP_SYS_MODULE` in the initial user namespace, which includes root inside a container's user namespace, Linux returns `EPERM` before it reads any argument (F1; the host strace under Additional information confirms it for an all-invalid argument set), so the call repeats exactly. `conditional` states both halves, and matches `init_module`.

### Nondeterminism sources

- Loaded-module and exported-symbol state of the running kernel (F2, F3, F8)
- Host signature and lockdown policy, the `kernel.modules_disabled` sysctl, and kernel configuration such as in-kernel decompression (F1, F4)
- The contents of the file behind `fd` and concurrent writers to it (F6)
- Concurrent loaders of the same module name, and the module's init function (F8)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it. `classify_syscall` lists `finit_module` as Determinized with the rest of the privileged system-administration family (`detcore/src/syscall_classification.rs:289` to `detcore/src/syscall_classification.rs:315`). `Detcore::handle_syscall_event` then matches it on the `Sysno`, before the typed match, with the guard `is_privileged_admin_refused_syscall` (`detcore/src/lib.rs:2164` to `detcore/src/lib.rs:2168`; the set itself is at `detcore/src/syscall_classification.rs:902`), and returns `Err(Error::Errno(Errno::EPERM))` (`detcore/src/lib.rs:2167`). The arm reads no argument and never forwards the call to the host kernel. It does not consult `panic_on_unsupported_syscalls`, so the answer is the same with and without fail-closed mode, and the comment above the arm (`detcore/src/lib.rs:2154`) states the intent that it be identical across `--verify` and record/replay. The same set feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), which is what `code_facts.refused: true` reports.

Classification-only evidence: the unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks that `finit_module` is Determinized and in the refusal set, and that the set holds nothing else. It does not start Hermit, so it is not coverage for any test below.

How close this is to Linux: `EPERM` is the kernel's first answer for a caller without the capability, before flag validation and before `fd` is checked, so Hermit's fixed answer matches what an unprivileged caller sees natively for F1 to F8. The host strace under Additional information is a concrete case: Linux and Hermit both return `EPERM` for `fd = -1` with undefined flag bits.

Why `refuse`:

- Under ptrace the guest runs as root inside the container's user namespace (the comment at `detcore/src/syscall_classification.rs:2298` to `detcore/src/syscall_classification.rs:2303` describes the `CLONE_NEWUSER` uid map). Linux checks `CAP_SYS_MODULE` against the initial user namespace, which such a root never holds, so a fixed `EPERM` is what the guest would see from the kernel in that container anyway. The refusal removes the dependency on how Hermit itself was launched.
- `passthrough` would let a guest change host-global kernel state if Hermit ever ran with the capability, which is an isolation hole, not a determinism feature.
- `model` or `emulate` would mean pretending that a kernel module was loaded or unloaded. A container has no kernel of its own, so there is no guest-visible state such an emulation could keep consistent (`/proc/modules`, the module's own side effects), and no workload that Hermit targets needs it.

Nothing is missing in the code. The gap is evidence: no test names `SYS_finit_module` (`tests_naming: 0`), so verification on the default backend stays untested.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6, F7, F8): under `hermit run --verify` on ptrace, call raw `syscall(SYS_finit_module, ...)` with: a readable regular file that is not a module and flags 0; the same file with each of `MODULE_INIT_IGNORE_MODVERSIONS`, `MODULE_INIT_IGNORE_VERMAGIC` and `MODULE_INIT_COMPRESSED_FILE`; flags `0x8`; `fd = -1`; a write-only descriptor; and a non-empty `param_values`. Assert every call returns -1 with `errno == EPERM` and both verify runs print identical output.
- T2 (covers F1): record a guest that calls `finit_module` under `hermit record`, replay it, and assert the replay returns `EPERM` at the same point and the run reproduces.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/355: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (code returns a fixed EPERM before reading arguments, detcore/src/lib.rs:2164-2168; Linux's first check is the same EPERM, host strace confirms it for fd=-1 and invalid flags); linux_determinism nondeterministic -> conditional (unprivileged callers always get EPERM; only the privileged load depends on host state); 8 flag behaviours and 2 tests listed; no test names the syscall, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `finit_module`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+finit_module&body=Discussion+for+the+%60finit_module%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffinit_module.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60finit_module%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: finit_module" (https://github.com/rrnewton/hermit/issues/355), legacy variant A2.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=finit_module -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(313, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
finit_module(-1, 0xffffffffffffffff, MODULE_INIT_IGNORE_MODVERSIONS|MODULE_INIT_IGNORE_VERMAGIC|MODULE_INIT_COMPRESSED_FILE|0xfffffff8) = -1 EPERM (Operation not permitted)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
