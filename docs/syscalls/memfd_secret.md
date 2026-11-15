---
schema: syscall-pages/v1
syscall: memfd_secret
nr_x86_64: 447
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/283
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+memfd_secret&body=Discussion+for+the+%60memfd_secret%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmemfd_secret.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60memfd_secret%60%2C+or+in+what+the+page+says%3F%0A
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

`memfd_secret(flags)` creates a file descriptor for "secret" memory: pages that are removed from the kernel's direct map, so neither other processes nor most of the kernel can read them. Hermit refuses it: Detcore returns `ENOSYS` for every argument without consulting the host (`detcore/src/lib.rs:2343-2347` at hermit f4055eb9cae4), which is what a kernel built without secret memory returns.

## Linux semantics

Added in Linux 5.14 and implemented only when the kernel is built with `CONFIG_SECRETMEM`. Linux 5.14 to 6.4 also required the boot parameter `secretmem.enable=1`; Linux 6.5 turned it on by default, and `secretmem.enable=0` still turns it off. On a kernel without the feature the call returns `ENOSYS`. The 2026-07-24 trace under Additional information (Linux 6.17) shows `ENOSYS`, and the current measurement host (Linux 7.1.3) is also built with `# CONFIG_SECRETMEM is not set` in `/boot/config-7.1.3-0_fbk0_0_g295ad0959f34`.

When the feature is present, the kernel first checks availability, then the flags, then allocates the lowest free descriptor. The descriptor starts with size 0. The caller sizes it with `ftruncate` and maps it with `mmap`; the pages are allocated at first touch and removed from the direct map at that point. The mapping is locked, so it counts against `RLIMIT_MEMLOCK`, and it is left out of core dumps. While any secret memory exists, the kernel refuses hibernation.

### Flags and options

- F1: `flags == 0`: create a secret-memory descriptor without close-on-exec.
- F2: `O_CLOEXEC`: create the descriptor with `FD_CLOEXEC` set. It is the only accepted flag.
- F3: any other bit in `flags`: `EINVAL`. This check runs after the availability check, so a kernel without the feature returns `ENOSYS` for every `flags` value.
- F4: availability: `ENOSYS` on kernels before 5.14, on kernels built without `CONFIG_SECRETMEM`, when secret memory is disabled at boot (`secretmem.enable`), or when the architecture cannot change the direct map.
- F5: use of the returned descriptor: `ftruncate` sets its size; `mmap` must be `MAP_SHARED` (a private mapping is `EINVAL`), and a mapping that would exceed `RLIMIT_MEMLOCK` fails with `EAGAIN`; `read` and `write` on the descriptor are not supported.
- F6: resource errors on creation: `EMFILE` when the descriptor table is full, `ENOMEM` when the kernel cannot allocate the backing file.

## Determinism analysis

`linux_determinism: conditional`, changed from the rendered `deterministic`. The data in secret memory is written only by the guest, and the descriptor number is the lowest free slot, which Hermit already makes repeatable for every descriptor-returning call. The result still depends on things outside the guest:

### Nondeterminism sources

- Host kernel configuration and boot parameters decide whether the call exists at all (F4): the same binary gets a descriptor on one host and `ENOSYS` on another.
- Memory accounting: whether `mmap` and the first-touch page faults succeed depends on `RLIMIT_MEMLOCK` and on free host memory (F5, F6).
- The call repeats on one host with one configuration and one descriptor table, which is why it is `conditional` and not `deterministic`.

## Hermit design

Target policy: `refuse`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` (`detcore/src/syscall_classification.rs:52`) lists `Sysno::memfd_secret` among the Determinized calls (`detcore/src/syscall_classification.rs:539`), under the comment tag `TODO-HUMAN-REVIEW(PR-839)` which explains the choice: a fixed `ENOSYS` keeps the guest's behaviour host-independent and lets feature probes take their portable fallback.
- `is_optional_memory_feature_syscall` (`detcore/src/syscall_classification.rs:1263`) selects `memfd_secret`, `process_mrelease` and `map_shadow_stack`.
- The dispatch arm in `Detcore::handle_syscall_event` (`detcore/src/lib.rs:2343-2347`) returns `Err(Error::Errno(Errno::ENOSYS))` for that predicate, before the typed match and without a condition on fail-closed mode. The guest never reaches the host kernel, so the host's `CONFIG_SECRETMEM` setting cannot leak in.
- The predicate is one term of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319`, term at `detcore/src/syscall_classification.rs:1333`), which is why the code facts show `refused: true`.

Every flag (F1-F6) therefore collapses into the F4 outcome: `-1` with `ENOSYS`, no descriptor allocated, no `EINVAL` for bad flags.

Why `refuse` and not the alternatives:

- `emulate`, for example with an ordinary `memfd_create` file, would hand the guest a descriptor that claims to be secret memory while its pages stay in the direct map. The guarantee this call exists for cannot be kept by an emulation, and a program that relies on it would be misled.
- `model` would need Detcore to own direct-map removal and the memlock accounting, which is host kernel work it cannot do from user space.
- `passthrough` makes the result depend on the host's kernel configuration (see Determinism analysis).
- `ENOSYS` is an outcome every portable program must already handle, since kernels before 6.5 had the feature off unless booted with `secretmem.enable=1`, and many current kernels are built without it.

## Recommended tests

- T1 (covers F1, F4): `syscall(SYS_memfd_secret, 0)` returns `-1` with `errno == ENOSYS` under `hermit run --strict --verify --panic-on-unsupported-syscalls` on ptrace, and the verify run reports determinism.
  Existing candidates: `tests/c/memfd_secret_enosys.c` (cell `c-programs/memfd-secret-enosys`, `tests/e2e/manifests/c-programs.yaml:3458`, verify with `ci` true on ptrace) and `optional_memory_features_fall_back_deterministically` at `hermit-cli/tests/optional_memory_features.rs:30`, which compiles that program and runs it with `--backend=ptrace` (`hermit-cli/tests/optional_memory_features.rs:68`). Both pass `flags == 0` only.
- T2 (covers F2, F3): `O_CLOEXEC`, an undefined bit such as `0x1`, and `0x80000000` each return `ENOSYS`, not `EINVAL`, and allocate no descriptor: an `open("/dev/null")` before and after the calls returns the same descriptor number.
- T3 (covers F5, F6): with `RLIMIT_NOFILE` lowered until every slot is used, the call still returns `ENOSYS`, not `EMFILE`. Because no descriptor is ever returned, no F5 operation can be reached; assert that `ftruncate` and `mmap` on the next free descriptor number fail with `EBADF`.
- T4 (covers F4): host independence: run T1 on a host whose kernel has `CONFIG_SECRETMEM=y` (natively the call returns a descriptor there) and check that Hermit still returns `ENOSYS`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/283#issuecomment-5872824249
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/283: linux_determinism deterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_optional_memory_feature_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; memfd_secret records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/283 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (Detcore returns ENOSYS for every argument at detcore/src/lib.rs:2343-2347, matching a kernel without CONFIG_SECRETMEM; an emulation could not keep the secrecy guarantee); linux_determinism deterministic -> conditional (availability depends on host kernel config and boot parameters, mapping on memlock limits); 6 flag behaviours and 4 tests listed; existing ptrace tests cover flags == 0 only and no coverage token is recorded, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by hermit-cli/tests/optional_memory_features.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `memfd_secret`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+memfd_secret&body=Discussion+for+the+%60memfd_secret%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmemfd_secret.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60memfd_secret%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: memfd_secret" (https://github.com/rrnewton/hermit/issues/283), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/283.

### 2026-07-24 host strace evidence

The routing described in this subsection is historical. At hermit f4055eb9cae4 the call no longer passes through; Detcore refuses it with `ENOSYS` (see Hermit design).

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=memfd_secret`, and by confirming hermit currently routes `memfd_secret` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
memfd_secret(0)                 = -1 ENOSYS (Function not implemented)
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/283#issuecomment-5872824249
