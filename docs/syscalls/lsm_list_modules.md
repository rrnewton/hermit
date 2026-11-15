---
schema: syscall-pages/v1
syscall: lsm_list_modules
nr_x86_64: 461
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/639
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsm_list_modules&body=Discussion+for+the+%60lsm_list_modules%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsm_list_modules.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsm_list_modules%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_host_kernel_probe_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`lsm_list_modules(ids, size, flags)` (Linux 6.8 and later) writes the IDs of the Linux Security Modules active on the running kernel, in their stacking order. Hermit refuses it with a fixed `ENOSYS` before anything reaches the host (`detcore/src/lib.rs:2329`), the answer a kernel without the LSM syscalls gives, consistent with its `ENOSYS` refusal of the sibling calls `lsm_get_self_attr` and `lsm_set_self_attr`.

Manual reference: Linux man-pages lsm_list_modules(2).

## Linux semantics

`ids` points to an array of `u64`, and `size` points to a `u32` that holds the array's size in bytes on entry. The kernel rejects a non-zero `flags` with `EINVAL`, reads `*size`, and always writes back the size it needs (the number of active LSMs times 8). If the caller's buffer is too small it returns `E2BIG`; otherwise it writes one `LSM_ID_*` value per active module (`LSM_ID_CAPABILITY` 100, `LSM_ID_SELINUX` 101, `LSM_ID_SMACK` 102, `LSM_ID_TOMOYO` 103, `LSM_ID_APPARMOR` 104, `LSM_ID_YAMA` 105, `LSM_ID_LOADPIN` 106, `LSM_ID_SAFESETID` 107, `LSM_ID_LOCKDOWN` 108, `LSM_ID_BPF` 109, `LSM_ID_LANDLOCK` 110, `LSM_ID_IMA` 111, `LSM_ID_EVM` 112, `LSM_ID_IPE` 113, from `<linux/lsm.h>`) and returns the count. A bad `size` or `ids` pointer gives `EFAULT`.

The list is fixed at boot: it depends on which LSMs the kernel was built with and on the `lsm=` boot parameter, and does not change while the system runs. The call needs no privilege and has no side effects. `capability` is always active, so on a kernel that has the call the list is never empty and a zero-sized query always gets `E2BIG`.

### Flags and options

- F1: `flags` must be 0; any other value is `EINVAL`, checked before `size` is read.
- F2: size query: `*size` smaller than the needed size (for example 0 with `ids = NULL`) gives `E2BIG`, with the needed size written to `*size`.
- F3: full query: `*size` at least the needed size: the IDs are written to `ids` in stacking order, `*size` is set to the bytes used, and the return value is the number of modules.
- F4: bad pointers: an unreadable or unwritable `size`, or an unwritable `ids` when the buffer is large enough, gives `EFAULT`.

## Determinism analysis

`linux_determinism: conditional`, changed from the legacy `nondeterministic`. The result is a pure function of the running kernel's build configuration and boot command line: two calls on the same booted host always return the same IDs in the same order, and nothing a process does changes them. It differs between hosts, kernel builds and boots with a different `lsm=`, and the call does not exist before Linux 6.8. That is host-configuration dependence, which this tracking set records as `conditional` (as for other calls that repeat given fixed host state), not run-to-run variation on one host.

### Nondeterminism sources

- Host kernel version: before 6.8 the call returns `ENOSYS`.
- Host kernel configuration (`CONFIG_SECURITY_*`, `CONFIG_LSM`) and the `lsm=` boot parameter decide the set and the order (F2, F3).
- Distribution choices: an AppArmor host and an SELinux host return different lists.

## Hermit design

Target policy: `refuse`. The code already does this at hermit f4055eb9cae4:

- `lsm_list_modules` is classified `Determinized` (`detcore/src/syscall_classification.rs:556`); the comment at `detcore/src/syscall_classification.rs:544` gives the reason: it exposes the host's active security stack.
- `is_host_kernel_probe_syscall` (`detcore/src/syscall_classification.rs:1277`) matches it, and the dispatch arm at `detcore/src/lib.rs:2329` returns `Err(Error::Errno(Errno::ENOSYS))`. Nothing is forwarded, neither `size` nor `ids` is read or written, and the answer does not depend on `--strict` or `--panic-on-unsupported-syscalls`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1334`), hence `refused: true`.
- `lsm_list_modules` is the last row of the pinned syscall table, which `Sysno::iter()` skips. The Detcore unit test `passthru_opt_covers_the_final_row_of_the_pinned_table` (`detcore/src/lib.rs:3105`) records that it once ran natively under `--passthru-opt`, the default for `hermit record` and `hermit replay`, and now requires Detcore to subscribe it so the refusal applies there too.

Under Hermit every row of F1 to F4 gives `-1`/`ENOSYS`, and `*size` keeps the caller's value.

Why `refuse` and not the alternatives:

- `passthrough` would make the guest see the host's security stack, which differs between the hosts Hermit runs on.
- `emulate` with a fixed list (for example only `LSM_ID_CAPABILITY`) would be deterministic. It would, however, describe a kernel that has the LSM syscalls, while Hermit refuses the two calls added with it in Linux 6.8, `lsm_get_self_attr` and `lsm_set_self_attr`, with `ENOSYS` (`detcore/src/syscall_classification.rs:1207`, dispatched at `detcore/src/lib.rs:2150`). Presenting all three as absent is the consistent "pre-6.8 kernel" answer.
- `model` has nothing to model: the call has no guest-controlled state.

What remains is test evidence. The only test calls the size query with `ids = NULL` and `*size = 0` (`tests/c/lsm_list_modules_enosys.c:26`). Non-zero `flags`, a full-size buffer and bad pointers are not exercised.

## Recommended tests

- T1 (covers F2): `lsm_list_modules(NULL, &size, 0)` with `size = 0` returns `-1`/`ENOSYS`, not `E2BIG`, and leaves `size` at 0, under `hermit run --strict --verify` on ptrace.
  Existing candidates: `tests/c/lsm_list_modules_enosys.c` in cell `c-programs/lsm-list-modules-enosys` (`tests/e2e/manifests/c-programs.yaml:3186`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:3205`), and `host_kernel_probes_fall_back_deterministically` (`hermit-cli/tests/host_kernel_probes.rs:30`), which compiles that guest and runs it with `--backend=ptrace` (`hermit-cli/tests/host_kernel_probes.rs:62`). The existing guest does not check that `size` is unchanged.
- T2 (covers F1, F3): with a 64-entry sentinel-filled `ids` buffer and `size = 512`, both `flags = 0` and `flags = 1` return `ENOSYS`; `ids` stays sentinel-filled and `size` stays 512.
- T3 (covers F4): a NULL `size` pointer, and an `ids` pointer to an unmapped page with a large `size`, return `ENOSYS`, never `EFAULT`.
- T4 (covers F2, F3): record and replay a guest that calls `lsm_list_modules` and branches on the result; replay reproduces `ENOSYS` without a host call. The existing cell has replay disabled (`tests/e2e/manifests/c-programs.yaml:3226`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/639: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_host_kernel_probe_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2329 returns a fixed ENOSYS, matching the ENOSYS refusal of lsm_get_self_attr/lsm_set_self_attr); linux_determinism nondeterministic -> conditional (the list is fixed per booted kernel and varies only with host build and boot configuration); 4 flag behaviours and 4 tests listed; no coverage token recorded, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lsm_list_modules`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsm_list_modules&body=Discussion+for+the+%60lsm_list_modules%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsm_list_modules.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsm_list_modules%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: lsm_list_modules" (https://github.com/rrnewton/hermit/issues/639), legacy variant B.

The dispatch arm and the predicate carry `TODO-HUMAN-REVIEW(PR-847)` tags (`detcore/src/lib.rs:2326`, `detcore/src/syscall_classification.rs:1271`); the human review they ask for has not been recorded in the code.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
