---
schema: syscall-pages/v1
syscall: lsm_get_self_attr
nr_x86_64: 459
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/638
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsm_get_self_attr&body=Discussion+for+the+%60lsm_get_self_attr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsm_get_self_attr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsm_get_self_attr%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_host_security_identity_probe_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`lsm_get_self_attr(attr, ctx, size, flags)` (Linux 6.8+) reads one security attribute of the calling thread, such as its SELinux or AppArmor context, from every active Linux Security Module (LSM) that implements it. Hermit refuses it: Detcore returns `ENOSYS` for every argument combination, in every mode, without forwarding the call to the host (`detcore/src/lib.rs:2149-2153` at hermit f4055eb9cae4), so a guest sees the same answer as on a kernel older than 6.8.

## Linux semantics

Prototype: `int lsm_get_self_attr(unsigned int attr, struct lsm_ctx *ctx, u32 *size, u32 flags)`. Constants are in `<linux/lsm.h>`. There is no glibc wrapper; callers use `syscall(2)`.

- The kernel asks each active LSM that implements `attr` for its value and packs one `struct lsm_ctx` per answering module into the caller's buffer `ctx`. Each entry holds the LSM id (`LSM_ID_SELINUX` 101, `LSM_ID_SMACK` 102, `LSM_ID_APPARMOR` 104, ...), entry length, context length and the context bytes, typically a NUL-terminated label string such as `unconfined_u:unconfined_r:unconfined_t:s0` or `unconfined`.
- `*size` is in/out. On entry it holds the buffer size; on return the kernel writes the total size it needed. On success the return value is the number of `lsm_ctx` entries written, not a byte count.
- Argument checks run first: `LSM_ATTR_UNDEF` gives `EINVAL`, unknown flag bits give `EINVAL`, and an unreadable `size` gives `EFAULT`. Modules that do not implement `attr` are skipped. If none answers, the call fails with `EOPNOTSUPP`.
- The call has no side effect on kernel state. Its output is the host's security configuration: which LSMs are built in and enabled, which policy is loaded, and the label the thread runs under, which it inherits from whatever launched it.
- Which attributes a module implements depends on the module: SELinux implements all six `LSM_ATTR_*` values; AppArmor implements a subset (current, exec, prev); a host with neither has no module answering any attribute.

### Flags and options

- F1: `attr = LSM_ATTR_CURRENT` (100): the thread's current security context.
- F2: `attr = LSM_ATTR_EXEC` (101): the context the thread will take at its next `execve`.
- F3: `attr = LSM_ATTR_FSCREATE` (102): the label for files the thread creates.
- F4: `attr = LSM_ATTR_KEYCREATE` (103): the label for keys the thread creates.
- F5: `attr = LSM_ATTR_PREV` (104): the context the thread had before its last transition.
- F6: `attr = LSM_ATTR_SOCKCREATE` (105): the label for sockets the thread creates.
- F7: `attr = LSM_ATTR_UNDEF` (0) gives `EINVAL`; an attribute no active module implements, including an undefined value, gives `EOPNOTSUPP`.
- F8: `flags = 0`: one `lsm_ctx` entry from each active module that implements `attr`; the return value is the entry count.
- F9: `flags = LSM_FLAG_SINGLE` (0x1): the caller fills `ctx->id` and only that module answers. A NULL `ctx` or `ctx->id == LSM_ID_UNDEF` gives `EINVAL`.
- F10: any other `flags` bit gives `EINVAL`.
- F11: size negotiation: if `*size` is smaller than the total, including the `ctx = NULL`, `*size = 0` length query, the call fails with `E2BIG` and writes the required size to `*size`.
- F12: an unreadable or unwritable `size` or `ctx` pointer gives `EFAULT`.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The call reads no clock, scheduler or randomness state, and it repeats exactly on one host whose LSM stack, loaded policy and launch context stay the same. It differs across hosts and launch paths:

### Nondeterminism sources

- The active LSM stack (`lsm=` boot parameter and build configuration): the same call returns two entries, one entry or `EOPNOTSUPP` on different machines (F1-F8).
- The loaded policy and the label the Hermit process itself runs under, which comes from the user's login, systemd unit or container runtime; these strings are copied into guest memory (F1-F6).
- The required size written to `*size` (F11) and the entry count returned (F8) follow from the two sources above.
- Policy reloads or label changes by another process on the host can change `LSM_ATTR_PREV`/`LSM_ATTR_CURRENT` between two runs.

## Hermit design

Target policy: `refuse`, which the code at hermit f4055eb9cae4 already realizes.

- `classify_syscall` (`detcore/src/syscall_classification.rs:52`) lists `lsm_get_self_attr` as Determinized (`detcore/src/syscall_classification.rs:512`), so Detcore intercepts it in run mode and in record/replay.
- The predicate `is_host_security_identity_probe_syscall` (`detcore/src/syscall_classification.rs:1207-1215`) holds exactly `lsm_get_self_attr` and `lsm_set_self_attr`. Its doc comment states the reason: LSM self-attributes depend on the kernel's configured and active security modules, so Detcore reports the standard feature-absence errno and leaves callers on their portable fallbacks.
- The dispatch arm (`detcore/src/lib.rs:2149-2153`) returns `Err(Error::Errno(Errno::ENOSYS))` before reading any argument, so F1-F12 all collapse to one result and no guest memory or `*size` is written. Unlike the zero-copy pipe arm just above it (`detcore/src/lib.rs:2138-2144`), it does not consult `panic_on_unsupported_syscalls`, so the answer is the same with and without fail-closed mode; this matches `refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`, membership at `detcore/src/syscall_classification.rs:1332`), which is why `code_facts.refused` is true.
- The unit test `host_security_identity_probes_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2406`) pins the classification and that the predicate holds exactly these two syscalls.

Why `refuse` and not the alternatives:

- `passthrough` copies host labels into the guest and makes the result depend on the host's LSM stack (see Determinism analysis).
- `emulate` with a fixed answer, for example a synthetic `EOPNOTSUPP` ("no module implements this") or a fake label, would claim a kernel feature Hermit does not have. A fake label would also be inconsistent with what the guest's later permission checks actually see, since the host kernel still enforces the real label.
- `model` would need a virtual LSM with its own labels, which Detcore does not have and the owner has not asked for.
- `ENOSYS` is what a kernel older than 6.8 returns, so callers written for older kernels take the path they already have. The cost is that a guest which requires the syscall cannot run; that is the intended trade.

Remaining gap, not in this syscall's arm: the classic fallback interface `/proc/self/attr/<name>` exposes the same host labels. No code under `detcore/src` or `hermit-cli/src` at this SHA mentions `/attr/`, so the fallback is presumably served from the host; this body records that as a follow-up, not as part of this syscall's policy.

## Recommended tests

- T1 (covers F1, F8, F11): `lsm_get_self_attr(LSM_ATTR_CURRENT, NULL, &size /* 0 */, 0)` returns -1 with `errno == ENOSYS` under `hermit run --strict --verify --panic-on-unsupported-syscalls` on ptrace. Existing: `tests/c/lsm_get_self_attr_enosys.c`, run by `host_security_identity_probes_fall_back_deterministically` at `hermit-cli/tests/host_security_identity.rs:30` (ptrace only, `hermit-cli/tests/host_security_identity.rs:68`) and by the manifest cell `c-programs/lsm-get-self-attr-enosys` (`tests/e2e/manifests/c-programs.yaml:3119`, verify mode with `ci: ptrace: true`).
- T2 (covers F2, F3, F4, F5, F6, F7, F9, F10, F12): sweep the arguments with a raw `syscall()`: every `LSM_ATTR_*` value, `LSM_ATTR_UNDEF`, an undefined attr such as 999, `LSM_FLAG_SINGLE` with a valid buffer and with NULL `ctx`, `flags = 0x2`, and a `size` pointer of `(void *)1`. Every call must return `ENOSYS` without crashing the guest, and a pre-filled `size` and buffer must be unchanged afterwards. Run under `hermit run --verify` on ptrace, with and without `--strict`.
- T3 (covers F1): under `hermit record` and then `hermit replay` on ptrace, a guest calling `lsm_get_self_attr(LSM_ATTR_CURRENT, ...)` sees `ENOSYS` in both runs and replay does not diverge.
- T4 (covers none): a guest that falls back from `ENOSYS` to reading `/proc/self/attr/current` prints what it read, run under `hermit run --verify` on hosts with different LSM stacks, to measure the `/proc` gap noted under Hermit design.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/638: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_host_security_identity_probe_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2149-2153 returns ENOSYS for every argument in every mode, the pre-6.8 answer, instead of exposing host LSM labels); linux_determinism stays conditional; 12 flag behaviours and 4 tests listed; T2-T4 do not exist yet and nothing was run, so tracking_status stays todo and ptrace_verification untested
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · covered: T1 covered by hermit-cli/tests/host_security_identity.rs and T1 covered by tests/c/lsm_get_self_attr_enosys.c (read at this SHA, not run by this agent; it exercises only LSM_ATTR_CURRENT, NULL ctx, size 0, flags 0)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lsm_get_self_attr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsm_get_self_attr&body=Discussion+for+the+%60lsm_get_self_attr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsm_get_self_attr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsm_get_self_attr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: lsm_get_self_attr" (https://github.com/rrnewton/hermit/issues/638), legacy variant B.

The legacy recommended action was **NEEDS-HANDLER**. The refusal arm landed later under `TODO-HUMAN-REVIEW(PR-860)` (`detcore/src/lib.rs:2146`); the companion `lsm_set_self_attr` is https://github.com/rrnewton/hermit/issues/279, and `lsm_list_modules` is refused with the same `ENOSYS` by `is_host_kernel_probe_syscall` (`detcore/src/syscall_classification.rs:1277`).

The inventory row for `tests/c/lsm_get_self_attr_enosys.c` in `tests/e2e/manifests/inventory/test-files.json:845-847` describes the program as `ci=false`, while its verify mode in `tests/e2e/manifests/c-programs.yaml:3136-3138` sets `ci: ptrace: true`; the manifest is what runs, so the inventory text looks stale.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
