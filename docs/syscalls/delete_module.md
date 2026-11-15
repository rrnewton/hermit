---
schema: syscall-pages/v1
syscall: delete_module
nr_x86_64: 176
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/344
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+delete_module&body=Discussion+for+the+%60delete_module%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fdelete_module.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60delete_module%60%2C+or+in+what+the+page+says%3F%0A
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

`delete_module(name, flags)` unloads a loaded kernel module by name. Hermit refuses it: Detcore answers every call with a fixed `EPERM` and never forwards it to the host kernel (`detcore/src/lib.rs:2164`).

Rank 65 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls.

Manual reference: Linux man-pages delete_module(2).

## Linux semantics

The module subsystem first checks `CAP_SYS_MODULE` in the initial user namespace and the `kernel.modules_disabled` sysctl; without the capability, or with modules disabled, the call returns `EPERM` before `name` is read. The host strace under Additional information shows an all-ones `name` pointer with every flag bit set still getting `EPERM`.

With the capability, the kernel copies `name`, looks up the live module, refuses if other modules depend on it, and checks its reference count. Since Linux 3.13 the kernel never waits for references to drop: a module in use fails at once with `EWOULDBLOCK`, unless a forced unload is requested and allowed. On success the module's exit function runs and the module disappears for the whole host.

### Flags and options

- F1: privilege gate: without `CAP_SYS_MODULE` in the initial user namespace, or with `kernel.modules_disabled=1`, the call returns `EPERM` before `name` or `flags` are examined.
- F2: `O_NONBLOCK`: do not wait for the module's references to drop. Since Linux 3.13 this is the only behaviour, so a module in use gives `EWOULDBLOCK` with or without the flag.
- F3: `O_TRUNC`: force the unload even if the module is in use or has no exit function. Honoured only in kernels built with `CONFIG_MODULE_FORCE_UNLOAD`; it taints the kernel.
- F4: `name`: a bad pointer gives `EFAULT`; no loaded module of that name gives `ENOENT`; a module that others depend on, or that is not in the live state, gives `EBUSY`. Reachable only past F1.
- F5: success: the module's exit function runs and the module is removed for every process on the host.

## Determinism analysis

`linux_determinism: conditional`, changed from the legacy `nondeterministic`. The legacy classification described the privileged path, which does depend on module references, load state and concurrent users. For a caller without `CAP_SYS_MODULE` in the initial user namespace, which includes root inside a container's user namespace, Linux returns `EPERM` before it reads any argument (F1; the host strace under Additional information confirms it for an all-invalid argument set), so the call repeats exactly. `conditional` states both halves.

### Nondeterminism sources

- Loaded-module and reference state (F2, F4)
- Host privilege and lockdown policy, and kernel configuration such as `CONFIG_MODULE_FORCE_UNLOAD` (F1, F3)
- Concurrent kernel users of the module, which decide whether it is in use at the moment of the call (F2, F5)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it. `classify_syscall` lists `delete_module` as Determinized with the rest of the privileged system-administration family (`detcore/src/syscall_classification.rs:289` to `detcore/src/syscall_classification.rs:315`). `Detcore::handle_syscall_event` then matches it on the `Sysno`, before the typed match, with the guard `is_privileged_admin_refused_syscall` (`detcore/src/lib.rs:2164` to `detcore/src/lib.rs:2168`; the set itself is at `detcore/src/syscall_classification.rs:902`), and returns `Err(Error::Errno(Errno::EPERM))` (`detcore/src/lib.rs:2167`). The arm reads no argument and never forwards the call to the host kernel. It does not consult `panic_on_unsupported_syscalls`, so the answer is the same with and without fail-closed mode, and the comment above the arm (`detcore/src/lib.rs:2154`) states the intent that it be identical across `--verify` and record/replay. The same set feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`), which is what `code_facts.refused: true` reports.

Classification-only evidence: the unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks that `delete_module` is Determinized and in the refusal set, and that the set holds nothing else. It does not start Hermit, so it is not coverage for any test below.

How close this is to Linux: the capability check is the kernel's first one, so Linux itself returns `EPERM` for every argument value when the caller lacks `CAP_SYS_MODULE`. Hermit's fixed answer therefore matches what an unprivileged caller sees natively for F1 to F5; the host strace under Additional information is a concrete case.

Why `refuse`:

- Under ptrace the guest runs as root inside the container's user namespace (the comment at `detcore/src/syscall_classification.rs:2298` to `detcore/src/syscall_classification.rs:2303` describes the `CLONE_NEWUSER` uid map). Linux checks `CAP_SYS_MODULE` against the initial user namespace, which such a root never holds, so a fixed `EPERM` is what the guest would see from the kernel in that container anyway. The refusal removes the dependency on how Hermit itself was launched.
- `passthrough` would let a guest change host-global kernel state if Hermit ever ran with the capability, which is an isolation hole, not a determinism feature.
- `model` or `emulate` would mean pretending that a kernel module was loaded or unloaded. A container has no kernel of its own, so there is no guest-visible state such an emulation could keep consistent (`/proc/modules`, the module's own side effects), and no workload that Hermit targets needs it.

Nothing is missing in the code. The gap is evidence: no test names `SYS_delete_module` (`tests_naming: 0`), so verification on the default backend stays untested.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): under `hermit run --verify` on ptrace, call raw `syscall(SYS_delete_module, ...)` with: the name of a module that is commonly loaded (for example `"loop"`) and flags 0; the same name with `O_NONBLOCK`, with `O_TRUNC`, and with both; a name no module has (`"hermit_no_such_module"`); and `name = (char *)1`. Assert every call returns -1 with `errno == EPERM`, that both verify runs print identical output, and that a guest-side read of `/proc/modules` is unchanged afterwards.
- T2 (covers F1): record a guest that calls `delete_module` under `hermit record`, replay it, and assert the replay returns `EPERM` at the same point and the run reproduces.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/344#issuecomment-5872962546
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/344: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; delete_module records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/344 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (code returns a fixed EPERM before reading arguments, detcore/src/lib.rs:2164-2168; Linux's first check is the same EPERM, host strace confirms it); linux_determinism nondeterministic -> conditional (unprivileged callers always get EPERM; only the privileged unload depends on host state); 5 flag behaviours and 2 tests listed; no test names the syscall, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `delete_module`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+delete_module&body=Discussion+for+the+%60delete_module%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fdelete_module.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60delete_module%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: delete_module" (https://github.com/rrnewton/hermit/issues/344), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/344.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=delete_module -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(176, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
delete_module(0xffffffffffffffff, O_NONBLOCK|O_TRUNC|0xfffff5ff) = -1 EPERM (Operation not permitted)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/344#issuecomment-5872962546
