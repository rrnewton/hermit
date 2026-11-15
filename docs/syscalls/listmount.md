---
schema: syscall-pages/v1
syscall: listmount
nr_x86_64: 458
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/632
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+listmount&body=Discussion+for+the+%60listmount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flistmount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60listmount%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_mount_introspection_enosys_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`listmount(req, mnt_ids, nr_mnt_ids, flags)` (Linux 6.8 and later) writes the 64-bit unique ids of the mounts under a given mount into a caller array, a page at a time. Hermit refuses it: Detcore answers every call with `ENOSYS` before looking at the arguments and never calls the kernel (`detcore/src/lib.rs:2110-2114` at hermit f4055eb9cae4), which is what a pre-6.8 kernel returns and sends callers to `/proc/self/mountinfo`, a file Hermit sanitizes.

Manual reference: Linux man-pages listmount(2).

## Linux semantics

`listmount` and its companion `statmount` replace parsing `/proc/self/mountinfo`: `listmount` enumerates mount ids, and `statmount` describes each one. The caller passes a `struct mnt_id_req` (`size`, `spare`, `mnt_id`, `param`, `mnt_ns_id`); for `listmount`, `mnt_id` is the mount whose descendants are listed and `param` is the last id returned by the previous call, for paging. The return value is the number of ids written; a full array means the caller should call again with `param` set to the last id. The call is read-only.

Constants below are from `/usr/include/linux/mount.h` (kernel-headers 6.17.4 on this host): `struct mnt_id_req` at line 198, the request sizes at lines 207-208, `LSMT_ROOT` at line 232 and `LISTMOUNT_REVERSE` at line 233.

The ids are the 64-bit unique mount ids (also reported by `statx` with `STATX_MNT_ID_UNIQUE`), not the reusable ids printed in `mountinfo`. Listing mounts in another namespace, or below a mount the caller cannot reach from its root, needs privilege over that namespace.

### Flags and options

- F1: `req->size` is `MNT_ID_REQ_SIZE_VER0` (24) or `MNT_ID_REQ_SIZE_VER1` (32, which adds `mnt_ns_id`); any other size, or a nonzero reserved `spare`, gives `EINVAL`.
- F2: `req->mnt_id` names the mount whose descendants are listed by its unique id, or is `LSMT_ROOT` (`0xffffffffffffffff`) for the namespace's root mount; an id that names no mount in the target namespace gives `ENOENT`.
- F3: `req->param` is 0 for the first page, or the last id of the previous page to continue after it.
- F4: `req->mnt_ns_id` (VER1 only) selects another mount namespace by its id; 0 means the caller's namespace. Listing another namespace, or below a mount outside the caller's root, without privilege over it gives `EPERM`.
- F5: `nr_mnt_ids` caps how many ids are written to `mnt_ids`; the return value is the count actually written.
- F6: `flags` `LISTMOUNT_REVERSE` (0x1): list later (larger-id) mounts first.
- F7: any other `flags` bit: `EINVAL`.
- F8: `req` or `mnt_ids` not accessible: `EFAULT`.
- F9: kernel older than 6.8: `ENOSYS`, the same errno Hermit returns for every call.

## Determinism analysis

`linux_determinism: nondeterministic` (changed from conditional). The output is nothing but unique mount ids, and those come from a kernel-wide counter that only grows. Any mount created at the start of a run, by a container setup or by the guest, gets a different id in every run, even on one host, and so does any namespace id passed in F4. Which mounts appear, and how many pages they need (F3, F5), depend on the host's mount table and on concurrent mounts and unmounts. Only error results (F1, F7, F8) repeat; the success path does not for any argument.

The migrated body said conditional, from the legacy classification. It is changed to nondeterministic for the reason above, and to match `statmount`, whose ids come from the same counter.

### Nondeterminism sources

- Unique mount ids and mount-namespace ids, allocated from kernel-wide increasing counters
- Mount-namespace contents and concurrent mounts or unmounts, which change the listed set and the paging
- Capability checks against the caller's namespace (F4) and kernel version (F9)

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` lists `listmount` among the Determinized syscalls (`detcore/src/syscall_classification.rs:381`), and `is_mount_introspection_enosys_syscall` includes it (`detcore/src/syscall_classification.rs:1190-1200`). The doc comment (`detcore/src/syscall_classification.rs:1182-1189`) says `listmount` reads mount ids and topology, which are not part of a deterministic guest's modeled state.
- In `handle_syscall_event` (`detcore/src/lib.rs:1892`), the dispatch match (`detcore/src/lib.rs:2028`) returns `Err(Errno::ENOSYS)` for this predicate (`detcore/src/lib.rs:2110-2114`). It does not read `req` and does not inject the syscall, so `mnt_ids` is never written and the answer is the same for F1-F9.
- The arm does not consult `panic_on_unsupported_syscalls` (contrast the `rseq` arm at `detcore/src/lib.rs:2031-2037`), so the answer is `ENOSYS` with and without fail-closed mode, matching `refused_only_when_fail_closed: false`.
- The predicate is in the deterministic refusal set (`detcore/src/syscall_classification.rs:1331`); the unit test `mount_introspection_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2377`) pins the classification and the membership.

Why `refuse`:

- `ENOSYS` is what every kernel before 6.8 returns (F9), so callers that prefer `listmount` must already fall back to `/proc/self/mountinfo`. Hermit sanitizes that file: reads of `/proc/<pid>/mountinfo` are recognized (`detcore/src/procfs.rs:684`) and rewritten by `sanitize_mountinfo` (`detcore/src/procfs.rs:3429`), with device numbers taken from the same run-global pool as `stat`/`statx` (`detcore/src/procfs.rs:491-498`).
- `passthrough` would return the ever-increasing host ids directly.
- `model` would need virtual unique ids that stay consistent with `statmount`, with `statx`'s `STATX_MNT_ID_UNIQUE` and with the `mountinfo` sanitizer. `listmount` alone is useless without `statmount`, which is also refused, so modelling one without the other gains nothing.

The legacy recommendation was NEEDS-HANDLER. The old Summary text, which described replacing a temporary catch-all policy, is superseded: the refusal is explicit at this SHA.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): a VER1 request for mount id 1 with `param` 0, `mnt_ns_id` 0, a 64-entry array and `flags` 0 returns `ENOSYS` under `hermit run --verify` on ptrace, in both default and fail-closed (`--panic-on-unsupported-syscalls`) runs.
  Existing candidates: `tests/c/listmount_enosys.c` through cell `c-programs/listmount-enosys` (`tests/e2e/manifests/c-programs.yaml:2989`, verify `ci: ptrace: true`), and `mount_introspection_syscalls_fall_back_deterministically` at `hermit-cli/tests/mount_introspection.rs:30`, which runs that guest with `--backend=ptrace`, `--strict`, `--verify` and `--panic-on-unsupported-syscalls`.
- T2 (covers F2, F6, F7): `LSMT_ROOT` with `flags` 0, with `LISTMOUNT_REVERSE`, and with an undefined flag bit, on an array pre-filled with a sentinel. Every call must return `ENOSYS` and the sentinel must be unchanged.
- T3 (covers F1, F4, F8, F9): a 16-byte request, a made-up `mnt_ns_id`, and NULL `req`/`mnt_ids` each return `ENOSYS` rather than `EINVAL`, `EPERM`/`ENOENT` or `EFAULT`.
- T4 (covers F2, F3): a guest that tries `listmount` first and falls back to parsing `/proc/self/mountinfo` on `ENOSYS` prints the same mount list in both `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/632: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_introspection_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns ENOSYS for every call without touching the kernel, detcore/src/lib.rs:2110-2114, as a pre-6.8 kernel does; callers fall back to /proc/self/mountinfo, which detcore/src/procfs.rs:3429 sanitizes); linux_determinism conditional -> nondeterministic (unique mount ids come from a kernel-wide increasing counter, so they differ every run); 9 flag behaviours and 4 tests listed; existing ptrace candidates for T1 named but no run result seen and one request shape exercised, so tracking_status stays todo and ptrace_verification untested

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `listmount`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+listmount&body=Discussion+for+the+%60listmount%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flistmount.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60listmount%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: listmount" (https://github.com/rrnewton/hermit/issues/632), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

The legacy Determinism analysis said `listmount` "enumerates host mount-namespace state and mount identifiers that require virtualization". That claim is kept in the analysis above; the conclusion drawn from it changed from "virtualize" to "refuse", for the reasons under Hermit design.
