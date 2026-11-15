---
schema: syscall-pages/v1
syscall: lsm_set_self_attr
nr_x86_64: 460
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/279
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsm_set_self_attr&body=Discussion+for+the+%60lsm_set_self_attr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsm_set_self_attr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsm_set_self_attr%60%2C+or+in+what+the+page+says%3F%0A
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

`lsm_set_self_attr(attr, ctx, size, flags)` (Linux 6.8+) sets one Linux Security Module (LSM) attribute of the calling thread, such as its SELinux or AppArmor context or the label for files it creates. Hermit refuses it: Detcore returns `ENOSYS` for every argument combination, in every mode, without forwarding the call to the host (`detcore/src/lib.rs:2149-2153` at hermit f4055eb9cae4), so the guest sees the same answer as on a kernel older than 6.8 and never changes the host-enforced label.

## Linux semantics

Prototype: `int lsm_set_self_attr(unsigned int attr, struct lsm_ctx *ctx, u32 size, u32 flags)`. Constants are in `<linux/lsm.h>`. There is no glibc wrapper; callers use `syscall(2)`.

- `ctx` points to one `struct lsm_ctx`: the target module's id (`LSM_ID_SELINUX` 101, `LSM_ID_SMACK` 102, `LSM_ID_APPARMOR` 104, ...), the entry length, the context length and the context bytes (for example an SELinux type transition or an AppArmor `changeprofile` request).
- The kernel checks the generic arguments in a fixed order before any module sees them: nonzero `flags` gives `EINVAL`; `size` smaller than `struct lsm_ctx` gives `EINVAL`; `size` larger than a page gives `E2BIG`; an unreadable `ctx` gives `EFAULT`; inconsistent `len`/`ctx_len` fields give `EINVAL`. The host strace under Additional information, `lsm_set_self_attr(LSM_ATTR_UNDEF, NULL, 0, 0) = -1 EINVAL`, is the `size` check winning.
- The request then goes only to the module whose id matches `ctx->id`. If no active module has that id, the call fails with `EOPNOTSUPP`. The module decides whether `attr` is writable and whether policy permits the change, and returns its own error otherwise (typically `EINVAL`, `EACCES` or `EPERM`).
- On success the call changes kernel state for the calling thread: its current label, or the label applied at its next `execve`, or the labels given to files, keys or sockets it creates. Later host permission checks for the guest are decided against that new label.

### Flags and options

- F1: `attr = LSM_ATTR_CURRENT` (100): change the thread's current context, if the module and policy allow a dynamic transition.
- F2: `attr = LSM_ATTR_EXEC` (101): set the context applied at the thread's next `execve`.
- F3: `attr = LSM_ATTR_FSCREATE` (102): set the label for files the thread creates.
- F4: `attr = LSM_ATTR_KEYCREATE` (103): set the label for keys the thread creates.
- F5: `attr = LSM_ATTR_SOCKCREATE` (105): set the label for sockets the thread creates.
- F6: `attr = LSM_ATTR_PREV` (104), `LSM_ATTR_UNDEF` (0), an undefined value, or an attribute the target module does not let callers write: rejected by the module with an error it chooses.
- F7: `ctx->id` selects the one module that handles the request; an id that names no active module gives `EOPNOTSUPP`.
- F8: size validation: `size < sizeof(struct lsm_ctx)` (including `ctx = NULL, size = 0`) gives `EINVAL`; `size` above one page gives `E2BIG`; `len`/`ctx_len` fields that do not fit `size` give `EINVAL`.
- F9: any nonzero `flags` gives `EINVAL`; no flag is defined for this call.
- F10: an unreadable `ctx` pointer gives `EFAULT`.
- F11: the loaded policy refuses the transition: the module returns a permission error (`EACCES` or `EPERM`, depending on the module).

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Given identical inputs, one host and an unchanged policy, the call returns the same result and makes the same change. Whether it succeeds, and which errno it returns when it fails, depends on host state that differs between machines.

### Nondeterminism sources

- The active LSM stack: the same request gets `EOPNOTSUPP` on a host without the named module and succeeds or fails by policy on a host with it (F6, F7).
- The loaded policy and the label the Hermit process itself runs under, inherited from the user's login, systemd unit or container runtime (F1-F5, F11).
- The call writes nothing into guest memory, so it cannot inject host data directly, but the return code is host-dependent. A successful call would also change the host label that governs the guest's later file, socket and `execve` permission checks, so later, otherwise deterministic calls could start failing differently on different hosts.

## Hermit design

Target policy: `refuse`, which the code at hermit f4055eb9cae4 already realizes.

- `classify_syscall` (`detcore/src/syscall_classification.rs:52`) lists `lsm_set_self_attr` as Determinized (`detcore/src/syscall_classification.rs:514`), so Detcore intercepts it in run mode and in record/replay.
- The predicate `is_host_security_identity_probe_syscall` (`detcore/src/syscall_classification.rs:1207-1215`) holds exactly `lsm_get_self_attr` and `lsm_set_self_attr`. Its doc comment states the reason: LSM self-attributes depend on the kernel's configured and active security modules, so Detcore reports the standard feature-absence errno.
- The dispatch arm (`detcore/src/lib.rs:2149-2153`) returns `Err(Error::Errno(Errno::ENOSYS))` before reading any argument, so F1-F11 all collapse to one result and the host label never changes. It does not consult `panic_on_unsupported_syscalls`, unlike the zero-copy pipe arm just above it (`detcore/src/lib.rs:2138-2144`), so the answer is the same with and without fail-closed mode; this matches `refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`, membership at `detcore/src/syscall_classification.rs:1332`), which is why `code_facts.refused` is true.
- The unit test `host_security_identity_probes_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2406`) pins the classification and that the predicate holds exactly these two syscalls.

Why `refuse` and not the alternatives:

- `passthrough` would let the guest change the host-enforced label of a traced thread, with host-dependent results; the legacy 2026-07-24 note below found the call passed through without crashing, but that is not a determinism argument.
- `emulate` (accept and pretend) would report a label change the host kernel does not make, so later permission checks would contradict what the guest was told.
- `model` would need a virtual LSM, which Detcore does not have and the owner has not asked for.
- `ENOSYS` is what a kernel older than 6.8 returns, so callers that support older kernels keep their existing path (the classic `/proc/self/attr/<name>` write interface).

Remaining gap, not in this syscall's arm: that `/proc/self/attr/<name>` write interface performs the same label changes. No code under `detcore/src` or `hermit-cli/src` at this SHA mentions `/attr/`, so a guest can presumably still change its host label through `/proc`; this body records that as a follow-up, not as part of this syscall's policy.

## Recommended tests

- T1 (covers F1, F8): `lsm_set_self_attr(LSM_ATTR_CURRENT, NULL, 0, 0)` returns -1 with `errno == ENOSYS` under `hermit run --strict --verify --panic-on-unsupported-syscalls` on ptrace, where Linux would return `EINVAL`. Existing: `tests/c/lsm_set_self_attr_enosys.c`, run by `host_security_identity_probes_fall_back_deterministically` at `hermit-cli/tests/host_security_identity.rs:30` (ptrace only, `hermit-cli/tests/host_security_identity.rs:68`) and by the manifest cell `c-programs/lsm-set-self-attr-enosys` (`tests/e2e/manifests/c-programs.yaml:3253`, verify mode with `ci: ptrace: true`).
- T2 (covers F2, F3, F4, F5, F6, F7, F9, F10, F11): sweep the arguments with a raw `syscall()` and a well-formed `struct lsm_ctx`: every `LSM_ATTR_*` value, `LSM_ATTR_UNDEF`, an undefined attr, `ctx->id` set to `LSM_ID_SELINUX`, `LSM_ID_APPARMOR` and an unused id, `flags = 1`, a `ctx` pointer of `(void *)1`, and a context string the host policy would reject. Every call must return `ENOSYS`. Run under `hermit run --verify` on ptrace, with and without `--strict`.
- T3 (covers F1, F3): after the refused call, the guest reads `/proc/self/attr/current` and creates a file whose label it reads back with `getxattr("security.selinux")` or equivalent; the label must equal the one read before the call, showing the refusal had no host side effect.
- T4 (covers F1): under `hermit record` and then `hermit replay` on ptrace, a guest calling `lsm_set_self_attr(LSM_ATTR_CURRENT, ...)` sees `ENOSYS` in both runs and replay does not diverge.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/279#issuecomment-5872815172
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/279: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_host_security_identity_probe_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; lsm_set_self_attr records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/279 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2149-2153 returns ENOSYS for every argument in every mode, the pre-6.8 answer, and never changes the host label); linux_determinism stays conditional; 11 flag behaviours and 4 tests listed; T2-T4 do not exist yet and nothing was run, so tracking_status stays todo and ptrace_verification untested
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · covered: T1 covered by hermit-cli/tests/host_security_identity.rs and T1 covered by tests/c/lsm_set_self_attr_enosys.c (read at this SHA, not run by this agent; it exercises only LSM_ATTR_CURRENT, NULL ctx, size 0, flags 0)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lsm_set_self_attr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lsm_set_self_attr&body=Discussion+for+the+%60lsm_set_self_attr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flsm_set_self_attr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lsm_set_self_attr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: lsm_set_self_attr" (https://github.com/rrnewton/hermit/issues/279), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/279.

The legacy recommended action was **NEEDS-INVESTIGATION**. The refusal arm landed later under `TODO-HUMAN-REVIEW(PR-860)` (`detcore/src/lib.rs:2146`); the companion `lsm_get_self_attr` is https://github.com/rrnewton/hermit/issues/638, and `lsm_list_modules` is refused with the same `ENOSYS` by `is_host_kernel_probe_syscall` (`detcore/src/syscall_classification.rs:1277`).

The inventory row for `tests/c/lsm_set_self_attr_enosys.c` in `tests/e2e/manifests/inventory/test-files.json:857-859` describes the program as `ci=false`, while its verify mode in `tests/e2e/manifests/c-programs.yaml:3270-3272` sets `ci: ptrace: true`; the manifest is what runs, so the inventory text looks stale.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=lsm_set_self_attr`, and by confirming hermit currently routes `lsm_set_self_attr` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
lsm_set_self_attr(LSM_ATTR_UNDEF, NULL, 0, 0) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/279#issuecomment-5872815172
