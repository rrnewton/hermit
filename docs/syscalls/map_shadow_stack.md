---
schema: syscall-pages/v1
syscall: map_shadow_stack
nr_x86_64: 453
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/280
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+map_shadow_stack&body=Discussion+for+the+%60map_shadow_stack%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmap_shadow_stack.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60map_shadow_stack%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_optional_memory_feature_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`map_shadow_stack(addr, size, flags)` maps a new shadow stack for x86 CET (Control-flow Enforcement Technology) user shadow stacks and returns its address; glibc uses it for user-created contexts when shadow stacks are enabled. Hermit refuses it: Detcore returns `ENOSYS` for every argument without consulting the host (`detcore/src/lib.rs:2343-2347` at hermit f4055eb9cae4), consistent with its `arch_prctl` policy that keeps shadow stacks disabled.

## Linux semantics

Added in Linux 6.6, x86 only, and present only in kernels built with `CONFIG_X86_USER_SHADOW_STACK`. The 2026-07-24 trace under Additional information (Linux 6.17) shows `ENOSYS`, and the current measurement host (Linux 7.1.3) is built with `# CONFIG_X86_USER_SHADOW_STACK is not set` in `/boot/config-7.1.3-0_fbk0_0_g295ad0959f34`.

On a kernel that has the feature, the checks run in this order: the CPU must support user shadow stacks (`EOPNOTSUPP` otherwise), then the flags, then the token size, then the address, then size overflow. The mapping is then created like a private anonymous `mmap` above 4 GiB, with shadow-stack page protections, so ordinary stores cannot write it. The call does not require the calling thread to have shadow stacks enabled.

### Flags and options

- F1: `addr == 0`: the kernel chooses the placement, above 4 GiB, and returns the address.
- F2: `addr != 0`: the mapping is placed exactly at `addr` without replacing anything (`MAP_FIXED_NOREPLACE` semantics): an occupied range fails with `EEXIST`, and a nonzero `addr` below 4 GiB fails with `ERANGE`.
- F3: `size`: rounded up to a page multiple; a `size` whose rounding overflows fails with `EOVERFLOW`.
- F4: `SHADOW_STACK_SET_TOKEN`: write a restore token at the top of the new shadow stack, so a thread can switch to it with `RSTORSSP`. With this flag, `size < 8` fails with `ENOSPC`.
- F5: any other bit in `flags`: `EINVAL`. On x86_64 only `SHADOW_STACK_SET_TOKEN` is accepted.
- F6: availability: `ENOSYS` on kernels before 6.6 or built without `CONFIG_X86_USER_SHADOW_STACK`; `EOPNOTSUPP` on a kernel that has the code when the CPU lacks user shadow stacks or they are disabled at boot. The availability check runs before every other check.

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification.

### Nondeterminism sources

- The returned address (F1) comes from the host's mmap layout, which address-space randomization and earlier mappings change from run to run, as for `mmap`.
- Availability (F6) depends on the host CPU, the kernel configuration and boot parameters, and the error differs: `ENOSYS` or `EOPNOTSUPP`.
- With a fixed `addr` (F2), success depends on what is already mapped there.
- The call repeats on one host when the address space layout repeats and the CPU and kernel support stay the same.

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` (`detcore/src/syscall_classification.rs:52`) lists `Sysno::map_shadow_stack` among the Determinized calls (`detcore/src/syscall_classification.rs:543`), under the comment tag `TODO-HUMAN-REVIEW(PR-839)`, which says a fixed `ENOSYS` keeps guest behaviour host-independent and lets callers use their portable fallbacks.
- `is_optional_memory_feature_syscall` (`detcore/src/syscall_classification.rs:1263`) selects it; its doc comment says `map_shadow_stack` "depends on CET support".
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2343-2347`) returns `Err(Error::Errno(Errno::ENOSYS))` for that predicate, before the typed match and without a condition on fail-closed mode. No mapping is created.
- The predicate is one term of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`, term at `detcore/src/syscall_classification.rs:1333`), which is why the code facts show `refused: true`.
- The refusal is consistent with `arch_prctl`. The handler keeps shadow stacks disabled in its virtual policy (comment at `detcore/src/syscalls/misc.rs:343`): `ARCH_SHSTK_STATUS` reports 0 (`detcore/src/syscalls/misc.rs:345`), disabling a valid feature set is an idempotent success (`detcore/src/syscalls/misc.rs:348`), and `ARCH_SHSTK_ENABLE`, `ARCH_SHSTK_LOCK` and `ARCH_SHSTK_UNLOCK` fail with `EINVAL` (`detcore/src/syscalls/misc.rs:353-356`). A guest therefore sees one coherent machine without user shadow stacks.

Every behaviour (F1-F6) collapses into the `ENOSYS` outcome of F6. On a host whose kernel has the feature but whose CPU lacks it, Linux returns `EOPNOTSUPP` where Hermit returns `ENOSYS`. Both mean "unavailable"; Hermit picks the result of a kernel without the code, which is the same on every host.

Why `refuse` and not the alternatives:

- `model` would require Hermit to support CET shadow stacks end to end: enabling them through `arch_prctl`, placing the new mapping through Detcore's deterministic address-space handling, and making sure every backend preserves shadow-stack semantics across signals and context switches. The `arch_prctl` policy above deliberately does not offer that, so a shadow stack mapped by this call could never be used.
- `passthrough` would let host CPU and kernel support leak into the guest (see Determinism analysis), and the returned address would bypass Detcore's placement.

## Recommended tests

- T1 (covers F1, F6): `syscall(SYS_map_shadow_stack, 0, 4096, 0)` returns `-1` with `errno == ENOSYS` under `hermit run --strict --verify --panic-on-unsupported-syscalls` on ptrace, and the verify run reports determinism.
  Existing candidates: `tests/c/map_shadow_stack_enosys.c` (cell `c-programs/map-shadow-stack-enosys`, `tests/e2e/manifests/c-programs.yaml:3391`, verify with `ci` true on ptrace) and `optional_memory_features_fall_back_deterministically` at `hermit-cli/tests/optional_memory_features.rs:30`, which runs it with `--backend=ptrace` (`hermit-cli/tests/optional_memory_features.rs:68`). Both use only `(0, 4096, 0)`.
- T2 (covers F2, F3, F4, F5): each of these returns `ENOSYS`, never `EEXIST`, `ERANGE`, `EOVERFLOW`, `ENOSPC` or `EINVAL`: a fixed `addr` above 4 GiB, a fixed `addr` below 4 GiB, `size == ~0UL`, `SHADOW_STACK_SET_TOKEN` with `size == 4096` and with `size == 4`, and `flags == 0x2`. After the calls, an `mmap` with `MAP_FIXED_NOREPLACE` at the fixed `addr` still succeeds, which shows no mapping was created.
- T3 (covers F6): in the same guest, `arch_prctl(ARCH_SHSTK_STATUS)` writes 0 and `arch_prctl(ARCH_SHSTK_ENABLE, ARCH_SHSTK_SHSTK)` fails with `EINVAL`, so a CET-aware runtime sees shadow stacks off consistently with T1.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/280#issuecomment-5872817447
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/280: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_optional_memory_feature_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; map_shadow_stack records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/280 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns ENOSYS for every argument at detcore/src/lib.rs:2343-2347, consistent with the arch_prctl policy that keeps shadow stacks disabled at detcore/src/syscalls/misc.rs:343-356); linux_determinism stays conditional; 6 flag behaviours and 3 tests listed; existing ptrace tests cover (0, 4096, 0) only and no coverage token is recorded, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by hermit-cli/tests/optional_memory_features.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `map_shadow_stack`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+map_shadow_stack&body=Discussion+for+the+%60map_shadow_stack%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmap_shadow_stack.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60map_shadow_stack%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: map_shadow_stack" (https://github.com/rrnewton/hermit/issues/280), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/280.

### 2026-07-24 host strace evidence

The routing described in this subsection is historical. At hermit f4055eb9cae4 the call no longer passes through; Detcore refuses it with `ENOSYS` (see Hermit design).

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=map_shadow_stack`, and by confirming hermit currently routes `map_shadow_stack` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
map_shadow_stack(NULL, 4096, 0) = -1 ENOSYS (Function not implemented)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/280#issuecomment-5872817447
