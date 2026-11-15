---
schema: syscall-pages/v1
syscall: uselib
nr_x86_64: 134
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/508
  - https://github.com/rrnewton/hermit/issues/529
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+uselib&body=Discussion+for+the+%60uselib%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fuselib.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60uselib%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_unimplemented_enosys_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`uselib(library)` is the obsolete a.out-era call that loaded a shared library into the calling process. On x86_64 the 64-bit table entry has no implementation, so the kernel returns `ENOSYS`. Detcore returns that same fixed `ENOSYS` itself and never forwards the call (`detcore/src/lib.rs:2090-2094`).

Manual reference: [uselib(2)](https://man7.org/linux/man-pages/man2/uselib.2.html).

## Linux semantics

`uselib` was the loader interface for a.out shared libraries; ELF dynamic loading through `mmap` replaced it. Under the x86_64 `syscall` instruction, number 134 reaches `sys_ni_syscall`, which returns `-ENOSYS` without reading the path argument. The generated x86_64 table of both kernel trees on the evidence host maps it that way (`__SYSCALL(134, sys_ni_syscall)` in `arch/x86/include/generated/asm/syscalls_64.h` of `/usr/src/kernels/6.13.2-0_fbk17_hardened_0_g2ae417e0caa0` and `/usr/src/kernels/7.1.3-0_fbk0_0_g295ad0959f34`). The 7.1.3 tree wires the real `sys_uselib` only in the 32-bit compatibility table (`__SYSCALL(86, sys_uselib)` in `syscalls_32.h`), and the 6.13.2 configuration has `# CONFIG_USELIB is not set`.

Where an implementation exists (32-bit, with `CONFIG_USELIB`), it opens the named file for execution and asks each registered binary format to load it. The outcome then depends on the file system: `ENOENT`, `EACCES`, or `ENOEXEC` when no format can load the file. None of that is reachable through the 64-bit entry.

### Flags and options

- F1: `library` is `NULL` or points to unmapped memory: x86_64 returns `ENOSYS`, not `EFAULT`, because the path is never read.
- F2: `library` names a path that does not exist: `ENOSYS`, not `ENOENT`.
- F3: `library` names an existing readable file, such as an ELF shared object: `ENOSYS`, not `ENOEXEC` or success; no file descriptor is consumed and no mapping is created.

## Determinism analysis

`linux_determinism: deterministic`. On x86_64 the 64-bit entry is an unconditional `ENOSYS` that consults no argument and no kernel or file-system state, so the call repeats exactly.

The original text of https://github.com/rrnewton/hermit/issues/508 said that where `uselib` is present it is "a deterministic function of the (fixed) library file". The duplicate https://github.com/rrnewton/hermit/issues/529 instead called the call a fixed refusal with no state. The duplicate's claim won and the original lost: the implementation is not reachable through the 64-bit entry at all, and where it is reachable its result depends on file existence, permissions and registered binary formats, which is file-system state rather than a fixed function.

### Nondeterminism sources

- None through the x86_64 64-bit entry.
- Outside that entry, only the architecture compatibility path and the kernel build (`CONFIG_USELIB`, a.out support) could change the result. Hermit's fixed `ENOSYS` removes the kernel-build dependence.

## Hermit design

Target policy: `refuse`. The rendered body carried `passthrough` from the legacy PASS-THRU recommendation, which predates the code. At hermit f4055eb9cae4 Detcore never forwards `uselib` to the host:

- `uselib` is listed in the Determinized arm of `classify_syscall` (`detcore/src/syscall_classification.rs:275-287`; the arm ends `=> SyscallClassification::Determinized` at `detcore/src/syscall_classification.rs:688`) and in the predicate `is_unimplemented_enosys_syscall` (`detcore/src/syscall_classification.rs:856`).
- The dispatch arm at `detcore/src/lib.rs:2090-2094` returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It reads no argument and consults no configuration, so the answer is the same with or without `panic_on_unsupported_syscalls` and under the fail-closed policy.
- The predicate is part of the refusal boundary `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1319-1321`), which is why `code_facts.refused` is `true`.
- The unit test `unimplemented_enosys_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2209`) pins the classification and the exact predicate set. It does not start Hermit, so it is not ptrace coverage.

Why `refuse` and not the alternatives:

- A fixed `ENOSYS` is exactly what every supported x86_64 kernel returns for this number, so a guest sees no difference from native execution. The doc comment at `detcore/src/syscall_classification.rs:851-855` states the same intent: deterministic by construction, without depending on the host kernel being modern.
- `passthrough` contradicts the code and would bring back a host-kernel dependence: a host whose 64-bit entry did reach an implementation would make the result depend on the guest's file system and the host's registered binary formats.
- `model` or `emulate` would mean reimplementing an interface that current Linux does not provide, which no program running on a modern kernel can rely on.

The code already realizes `refuse`. What is missing is a test that starts Hermit on ptrace and calls `uselib` (none names it at this SHA). The classification, predicate and dispatch arm all still carry `TODO-HUMAN-REVIEW(#715)` (`detcore/src/syscall_classification.rs:271`, `detcore/src/syscall_classification.rs:850`, `detcore/src/lib.rs:2084`; https://github.com/rrnewton/hermit/issues/715). That review concerns the arm as written and does not argue for a different policy.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, call `syscall(SYS_uselib, ...)` with `NULL`, with `(char *)1`, with a nonexistent path, and with an existing shared object such as the guest's libc. Every call must return `-1` with `errno == ENOSYS`, and the descriptor table and `/proc/self/maps` must be unchanged after the calls.
  Shape: a C program `tests/c/uselib_enosys.c` modelled on `tests/c/futex_wake_enosys.c`, which calls a raw `syscall()` and fails unless it gets `-1`/`ENOSYS`, with a cell modelled on `c-programs/futex-wake-enosys` (`tests/e2e/manifests/c-programs.yaml:1660`, `verify` with `ci: ptrace: true`). Neither the program nor the cell exists for `uselib` at this SHA.
- T2 (covers none): record and replay a guest that calls `uselib` once; both the recording and the replay must see `-1/ENOSYS`, and the replay must not diverge.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/508: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch pred:is_unimplemented_enosys_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/529 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> refuse (Detcore returns a fixed ENOSYS at detcore/src/lib.rs:2090-2094, the value every x86_64 kernel returns for nr 134); linux_determinism stays deterministic; merged duplicate https://github.com/rrnewton/hermit/issues/529 (its 'deterministic function of the library file' claim rejected); 3 flag behaviours, 2 tests; no ptrace test calls uselib, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `uselib`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+uselib&body=Discussion+for+the+%60uselib%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fuselib.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60uselib%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: uselib" (https://github.com/rrnewton/hermit/issues/508), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=uselib` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `uselib` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

That paragraph describes the code of 2026-07-24. At hermit f4055eb9cae4 Detcore refuses the call with a fixed `ENOSYS` and never forwards it (see Hermit design).

Observed strace line:
```
uselib("/nonexistent-lib-xyz")  = -1 ENOSYS (Function not implemented)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Merged duplicate https://github.com/rrnewton/hermit/issues/529

Duplicate https://github.com/rrnewton/hermit/issues/529 was filed 2026-07-24 as "Syscall classification: uselib" (legacy variant A1) by the batch-5 classification sweep (ranks 201-250 of the then-UNCLASSIFIED list). Its comment history (0 comments) stays on that issue; the comment history of https://github.com/rrnewton/hermit/issues/508 stays there too. It was merged section by section into this body on 2026-10-03:

- Summary and manual reference: kept, the uselib(2) link is in Summary.
- Determinism: both texts classified it DETERMINISTIC and both recommended PASS-THRU. The duplicate's point that only architecture compatibility mode and kernel build support could change the result is kept under Nondeterminism sources. This body's original claim that, where present, uselib is "a deterministic function of the (fixed) library file" was rejected (see Determinism analysis).
- Kernel notes: the duplicate's statement that the interface served old a.out binaries and is unavailable for native x86_64 is kept in Linux semantics, now backed by the host kernel tables.
- Recommended action: both said PASS-THRU, and both described Hermit routing `uselib` through the UNCLASSIFIED temporary pass-through block. That was true of the code they read and is superseded: at hermit f4055eb9cae4 Detcore refuses the call with a fixed `ENOSYS` (see Hermit design), so the policy is `refuse`.
- Testing: the duplicate's evidence is kept below. It proposed no further test.

Duplicate's evidence, gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness traced by `strace -qq -f -e trace=uselib`, using deliberately invalid arguments. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` then placed `uselib` in the UNCLASSIFIED temporary-passthrough block.

```text
uselib("/tmp/hermit-no-library-b5") = -1 ENOSYS (Function not implemented)
```
