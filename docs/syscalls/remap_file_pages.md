---
schema: syscall-pages/v1
syscall: remap_file_pages
nr_x86_64: 216
linux_determinism: deterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/398
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+remap_file_pages&body=Discussion+for+the+%60remap_file_pages%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fremap_file_pages.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60remap_file_pages%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_remap_file_pages_enosys_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`remap_file_pages(start, size, prot, pgoff, flags)` is the deprecated interface that makes a range of an existing shared mapping show a different window of the same file, so that file pages appear in a non-linear order. Detcore refuses it: every call returns a fixed `ENOSYS` without reading the arguments or reaching the kernel (`detcore/src/lib.rs:2246`), so guests take the documented `mmap` fallback.

Manual reference: Linux man-pages remap_file_pages(2); kernel `Documentation/mm/remap_file_pages.rst`.

## Linux semantics

The call was deprecated in Linux 3.16, and since Linux 4.0 the kernel no longer builds a true non-linear mapping. It emulates the call: after checking the arguments, it replaces `[start, start + size)` with a new `MAP_SHARED | MAP_FIXED` mapping of the same file at page offset `pgoff`, with the protection copied from the existing mapping. Each remapped window becomes its own VMA (kernel memory area), so a program that scatters many windows splits the mapping into many VMAs. On first use per boot the kernel logs a deprecation warning to the host kernel log, which the guest cannot see. Before 4.0 the call worked only on file systems that supported non-linear mappings and kept the result inside one VMA, so the behaviour depended on the host kernel version.

The remapped range must lie inside an existing shared mapping. The new pages map the same file (for `MAP_SHARED | MAP_ANONYMOUS` and memfd mappings, the same shmem object). Checks run in this order: `prot`, then the rounded range and `pgoff` for overflow, then the target mapping under the mmap lock. Waiting for that lock is killable, so a fatal signal can end the call with `EINTR`; the caller is dying, so the guest never observes it.

The 2026-07-24 host trace under Additional information (`remap_file_pages(0x407000, 4096, PROT_NONE, 0, MAP_FILE) = -1 EINVAL`) shows the call reaching the 6.17 host kernel and failing with `EINVAL`, because the target address was not in a shared mapping (F6). The host kernel therefore provides the interface; Hermit's `ENOSYS` is not what that host returns.

### Flags and options

- F1: `prot` must be 0. Any other value fails with `EINVAL` before the address is examined. The new mapping takes `PROT_READ`/`PROT_WRITE`/`PROT_EXEC` from the existing mapping.
- F2: `flags` containing `MAP_NONBLOCK`: the remapped range is not prefaulted.
- F3: `flags` without `MAP_NONBLOCK`: the range is prefaulted (`MAP_POPULATE`), reading the file pages in. Every bit of `flags` other than `MAP_NONBLOCK` (for example `MAP_FILE`, `MAP_PRIVATE` or an undefined bit) is silently ignored, not rejected.
- F4: `start` and `size` are rounded down to page multiples. A `size` that rounds to zero, or a range whose end wraps around the address space, fails with `EINVAL`.
- F5: `pgoff` is the file offset in pages of the new window. `pgoff + size / PAGE_SIZE` wrapping fails with `EINVAL`. A `pgoff` beyond end of file is accepted; touching such a page later raises `SIGBUS`, as for `mmap`.
- F6: target mapping kind: `start` must lie in a `VM_SHARED` mapping (a shared file mapping, a memfd or tmpfs mapping, or `MAP_SHARED | MAP_ANONYMOUS`). A private mapping or an unmapped `start` fails with `EINVAL`.
- F7: a range that runs past the end of the mapping containing `start` is accepted only if the following mappings are contiguous, map the same file and have identical flags. Otherwise `EINVAL`.
- F8: target mapping locked with `mlock` or `MAP_LOCKED`: the replacement mapping is created `MAP_LOCKED`.
- F9: success returns 0, and the range now maps file pages starting at `pgoff`. The replacement can split VMAs, so it fails with `ENOMEM` when the split would exceed `vm.max_map_count`. A security module that denies the file mapping (`security_mmap_file`) makes the call fail with its error, typically `EACCES` or `EPERM`.

## Determinism analysis

`linux_determinism: deterministic`, kept from the legacy classification. With the 4.0+ emulation, the result is a function of the arguments, the caller's own mappings and the backing file. Every error in F1 and F4-F7 depends only on the arguments and the caller's address space.

The call repeats under the same conditions as `mmap` of the same file. The remaining host inputs are configuration and file state shared with every `mmap`: the `vm.max_map_count` limit, the security-module policy, and the contents of the file, which another process can change through its own shared mapping. The deprecation warning goes to the host kernel log, not to the guest. The pre-4.0 implementation differed (see Linux semantics), so the result also depends on the host kernel version, which is the reason Detcore's doc comment gives for not exposing it.

### Nondeterminism sources

- Host kernel version: before 4.0 the call built a true non-linear VMA and was refused by file systems without support; from 4.0 it is emulated with separate VMAs
- `vm.max_map_count` and the current VMA count, which decide whether the split fails with `ENOMEM` (F9)
- Security-module policy on mapping the file (F9)
- Other processes, inside or outside the guest, that write the shared file while the window is mapped

## Hermit design

Target policy: `refuse`, with a fixed `ENOSYS`. The code at hermit f4055eb9cae4 already realizes it:

- `classify_syscall` lists `remap_file_pages` as Determinized (`detcore/src/syscall_classification.rs:426`). The predicate `is_remap_file_pages_enosys_syscall` matches exactly this syscall (`detcore/src/syscall_classification.rs:1097`). Its doc comment (`detcore/src/syscall_classification.rs:1092`) states the reason: non-linear layouts vary by host kernel, Detcore does not model page-offset remapping, and callers can use the documented `mmap` fallback.
- The dispatch arm in `handle_syscall_event` returns `Err(Errno::ENOSYS)` (`detcore/src/lib.rs:2246`). It reads no argument and forwards nothing, so the guest's mappings are unchanged. The arm is not conditioned on the fail-closed policy, which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1326`), which backends that run guest syscalls outside this dispatcher consult to enforce the same refusal (`detcore/src/lib.rs:226`).
- Unit tests pin membership only: `remap_file_pages_is_deterministically_unavailable` (`detcore/src/syscall_classification.rs:2649`) and the refusal-aggregate test, which lists `Sysno::remap_file_pages` (`detcore/src/syscall_classification.rs:2729`). The guest result is exercised by the integration test and e2e programs named under Recommended tests.

Consequence for every flag: F1 to F9 all return `ENOSYS` under Hermit. Well-formed calls that Linux would perform do nothing, and malformed calls that Linux rejects with `EINVAL` (F1, F4-F7) also get `ENOSYS`. A guest that probes for the interface sees it as absent, as on a kernel without it, and falls back to `mmap` with `MAP_FIXED`.

Why `refuse` and not the alternatives:

- `passthrough` would let the kernel replace part of a shared mapping without Detcore knowing. Detcore keeps a model of shared mappings: `handle_mmap` records the backing object and file offset of each shared mapping (`detcore/src/syscalls/files.rs:2938`, `detcore/src/tool_local.rs:2460`), and shared futex keys are resolved from that model as object plus offset (`detcore/src/memory.rs:100`, `detcore/src/tool_local.rs:2440`). After an unseen remap, the model would give the old offset, so a futex in the remapped window could get the wrong key. Passthrough would also import the pre-4.0 versus 4.0+ difference into the guest.
- `emulate` could follow the kernel's own approach: rewrite the call into a tracked `mmap(start, size, prot_of_vma, MAP_SHARED | MAP_FIXED, fd, pgoff * PAGE_SIZE)` and update the shared-mapping model. That needs a descriptor for the mapping's file, the checks in F1 and F4-F7 reproduced in Detcore, and a known caller. No guest workload in the test corpus is known to need it.
- `model` adds nothing over `emulate` here, because the kernel itself no longer keeps a non-linear mapping.

The arm and the classification entry carry `TODO-HUMAN-REVIEW(PR-882)` markers (`detcore/src/lib.rs:2243`, `detcore/src/syscall_classification.rs:424`). They ask for a review of this boundary; they do not report a known defect.

Every test runs under `hermit run --verify` on ptrace and asserts `-1` with `errno == ENOSYS`, and that the mapping still shows its original file pages afterwards (page 1 of the mapping still reads file page 1).

## Recommended tests

- T1 (covers F3, F6, F9): a well-formed call (`prot = 0`, `flags = 0`, valid `pgoff`) on a `MAP_SHARED | MAP_ANONYMOUS` mapping, a memfd mapping and an unlinked temporary file mapping returns `ENOSYS`.
  Existing candidates: `tests/c/remap_file_pages_anonymous_enosys.c` (cell `c-programs/remap-file-pages-anonymous-enosys`, `tests/e2e/manifests/c-programs.yaml:7283`, verify ci on ptrace at line 7302), `tests/c/remap_file_pages_memfd_enosys.c` (`tests/e2e/manifests/c-programs.yaml:7350`, ptrace at line 7369) and `tests/c/remap_file_pages_tmpfile_enosys.c` (`tests/e2e/manifests/c-programs.yaml:7417`, ptrace at line 7436). The cargo test `remap_file_pages_refusals_verify` (`hermit-cli/tests/remap_file_pages_refusal.rs:30`) runs all three with `--backend=ptrace --strict --verify --panic-on-unsupported-syscalls` (`hermit-cli/tests/remap_file_pages_refusal.rs:71`). None of them checks the mapping contents after the call.
- T2 (covers F1, F4, F5): `prot = PROT_READ`, `size = 0`, `size = PAGE_SIZE - 1`, a `start + size` that wraps, and a `pgoff` that wraps all return `ENOSYS`, not the host's `EINVAL`. Record the native result next to Hermit's in the test output.
- T3 (covers F6, F7): a private file mapping, an unmapped `start`, and a range spanning two adjacent shared mappings of different files all return `ENOSYS`.
- T4 (covers F2, F8): `flags = MAP_NONBLOCK`, an undefined `flags` bit, and a target mapping locked with `mlock` all return `ENOSYS`, and the lock state of the mapping is unchanged (`mincore` or `/proc/self/smaps` `Locked:`).
- T5 (covers F9): two processes share a file mapping and use a shared futex in page 1. After a refused `remap_file_pages` on that page, a `FUTEX_WAKE` from one process still wakes a `FUTEX_WAIT` in the other, showing that Detcore's shared-mapping model is unchanged.
- T6 (covers F3): record a guest that calls `remap_file_pages`, then replay it; both phases see `ENOSYS`. The three e2e cells disable record/replay on ptrace (for example `tests/e2e/manifests/c-programs.yaml:7327`), so this needs a replay cell or a cargo test.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/398: linux_determinism deterministic (from **Classification:**); target_policy from legacy NEEDS-INVESTIGATION; code at this SHA: determinized, dispatch pred:is_remap_file_pages_enosys_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every call without reading arguments, detcore/src/lib.rs:2246; passthrough would desync the shared-mapping model that keys shared futexes); linux_determinism stays deterministic; 9 flag behaviours and 6 tests listed; three ptrace verify cells and the cargo test hermit-cli/tests/remap_file_pages_refusal.rs are T1 candidates but no run has been checked against this list, so no coverage token, ptrace_verification stays untested and tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `remap_file_pages`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+remap_file_pages&body=Discussion+for+the+%60remap_file_pages%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fremap_file_pages.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60remap_file_pages%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: remap_file_pages" (https://github.com/rrnewton/hermit/issues/398), legacy variant A1.

The legacy evidence below says Hermit routed `remap_file_pages` through a temporary-passthrough block. That was true on 2026-07-24; at hermit f4055eb9cae4 the syscall is Determinized and refused with `ENOSYS` (see Hermit design).

### Future work

- If a real workload needs the interface, implement `emulate` as described under Hermit design: translate the call into a tracked `MAP_SHARED | MAP_FIXED` `mmap` of the mapping's file and update the shared-mapping model, reproducing the checks in F1 and F4-F7.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=remap_file_pages` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `remap_file_pages` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
remap_file_pages(0x407000, 4096, PROT_NONE, 0, MAP_FILE) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
