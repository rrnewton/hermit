---
schema: syscall-pages/v1
syscall: munmap
nr_x86_64: 11
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+munmap&body=Discussion+for+the+%60munmap%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmunmap.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60munmap%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`munmap` removes every mapping in a page range of the calling process's address space. Hermit forwards the call to the kernel unchanged and, after a successful return, removes the same range from Detcore's model of shared mappings, which Detcore uses to tell shared futex keys from private ones.

## Linux semantics

`munmap(addr, length)` deletes the mappings that overlap `[addr, addr + length)`, with `length` rounded up to a whole page. It returns 0 or `-1` with `errno`. The address space is shared by every thread created with `CLONE_VM`, so the effect is visible to all of them at once.

- After the call, any access to the range raises `SIGSEGV`, until a later `mmap` places something there again.
- Unmapping a range that contains no mapping is not an error.
- Closing the file descriptor does not unmap, and unmapping does not close it.
- Dirty pages of a `MAP_SHARED` file mapping stay in the page cache and are written back later; `munmap` does not imply `msync`. Pages of a `MAP_PRIVATE` or anonymous mapping are freed.
- Unmapping the middle of a mapping splits it into two kernel VMAs. That can fail with `ENOMEM` when the process would exceed the `vm.max_map_count` sysctl.
- Memory locks (`mlock`, `MAP_LOCKED`) on the range are released with it.
- A futex word inside a `MAP_SHARED` mapping is keyed by the backing object and offset, not by the virtual address. After the mapping is gone, the same address can be reused for a private mapping with a private key.

### Flags and options

- F1: `addr` must be page aligned; otherwise `EINVAL`.
- F2: `length` of 0 gives `EINVAL`; any other length is rounded up to a whole page.
- F3: a range that wraps the address space or extends past the user address limit (`TASK_SIZE`) gives `EINVAL`.
- F4: a range that contains no mapping, or only partly overlaps mappings, succeeds and removes only what is mapped.
- F5: unmapping part of a mapping (its start, end or middle) leaves the remaining parts mapped with their original offsets and protections.
- F6: for a `MAP_HUGETLB` mapping, `addr` and `length` must be multiples of the huge page size; a split at a misaligned address gives `EINVAL`.
- F7: on a `MAP_SHARED` file mapping, data written through the mapping is kept in the file after the unmap; on a `MAP_PRIVATE` or anonymous mapping, the contents are discarded.
- F8: a range sealed with `mseal` (Linux 6.10 and later) gives `EPERM`, and nothing is unmapped.
- F9: after a successful unmap, an access to the range raises `SIGSEGV` with `si_code` `SEGV_MAPERR`.
- F10: a successful unmap of a `MAP_SHARED` range ends the shared futex identity of addresses in it; a later private mapping at the same address must get a private futex key, and a later shared mapping must get the key of its own backing object.
- F11: a middle unmap that would push the process past the host's `vm.max_map_count` gives `ENOMEM`.

## Determinism analysis

`linux_determinism` is `conditional`. The kernel's work is a pure function of the address space and the arguments, and the arguments are addresses the program got from earlier `mmap`, `brk` or stack setup. The call repeats when those earlier placements repeat and when the same threads touch the range in the same order. The sources that break this:

- Address-space layout. With ASLR enabled, the addresses passed in differ between runs, and so do the results of any program that prints, hashes or compares them.
- Threads racing on the range. One thread's access to a range another thread is unmapping either succeeds or raises `SIGSEGV`, depending on which runs first.
- Host configuration. The `ENOMEM` of F11 depends on the host's `vm.max_map_count` sysctl and on how many VMAs the process already has, which includes mappings made by the loader and by libc.
- Page cache sharing. After F7, what another process on the host sees in the file depends on writeback timing, but that is a property of the file, not of this call's result.

### Cross-host variation

- Sysctl `vm.max_map_count`: how many VMAs the process may have before a middle unmap gives `ENOMEM` (F11). On the measurement host it is 67108864, far above the Linux default of 65530.
- Huge page sizes and the reserved huge page pool (`vm.nr_hugepages`): whether a `MAP_HUGETLB` mapping exists to unmap at all, and which alignment F6 requires.

## Hermit design

Target policy: `model`. The kernel stays the authority for the address space, and Detcore keeps the one piece of mapping state it needs: which ranges are shared, and with which backing object. The handler at `detcore/src/syscalls/files.rs:2945` does this:

- It captures `addr` and `length`, then forwards the call through `record_or_replay` (`detcore/src/syscalls/files.rs:2952`; `record_or_replay` is defined at `detcore/src/tool_local.rs:382`). In run mode that injects the call. The recorder and the replayer both let `munmap` through to the kernel (`hermit-cli/src/recorder.rs:563`, `hermit-cli/src/replayer.rs:460`), because the address space is rebuilt by the replayed `mmap` calls rather than recorded.
- Only after a successful return does it call `unmap_memory` (`detcore/src/syscalls/files.rs:2953`, defined at `detcore/src/tool_local.rs:2474`). A failed call returns through the `?` before the model changes, so `EINVAL`, `ENOMEM` and `EPERM` leave the model as the kernel left the address space.
- `MemoryMetadata::unmap` (`detcore/src/memory.rs:164`) rounds the length up to a 4096-byte page (`detcore/src/memory.rs:20-28`), removes every overlapping shared mapping and re-inserts the parts on either side, recomputing the object offset of the right part (F5). `futex_id` (`detcore/src/memory.rs:100`) then falls back to a private key for any address outside a tracked shared mapping (F10). The unit test `unmap_and_remap_preserve_only_live_aliases` at `detcore/src/memory.rs:339` checks the model directly.
- `handle_mmap` calls the same `unmap_memory` before it records a new shared mapping (`detcore/src/syscalls/files.rs:2930-2941`), so a `MAP_FIXED` mapping that replaces an older one also clears it.
- The scheduler orders the threads of one process, which removes the race in the second bullet of the analysis. `munmap` is charged `MEMORY_NS` of logical time (`detcore/src/syscall_time.rs:17`, `detcore/src/syscall_time.rs:89-95`).
- The guest runs with ASLR disabled, as the comment at `hermit-cli/src/recorder/mmap.rs:206-207` relies on, so the addresses the guest passes repeat between runs.

Why `model` and not the alternatives: `passthrough` would leave stale shared ranges in the futex model, so a private futex at a reused address could be matched with waiters on an old shared object. `emulate` is not possible, because the kernel must really remove the pages. `refuse` would break every program that frees memory.

What Detcore does not model, and accepts as outside state: the host's `vm.max_map_count` (F11) and the page cache contents seen by other host processes (F7).

## Recommended tests

- T1 (covers F1, F2, F3): with a raw `syscall(SYS_munmap, ...)`, check `EINVAL` for a misaligned `addr`, for `length` 0, and for a range that wraps past the top of the address space. Check that a `length` of one byte unmaps the whole page: a later access raises `SIGSEGV`.
- T2 (covers F4, F5): map three pages, unmap the middle page, and check that the first and third pages still hold their data and that an access to the middle one faults. Then unmap a range that covers the third page plus unmapped space after it, and check that it returns 0.
  Existing partial coverage: `tests/c/mmap_determinism.c` unmaps whole mappings in its `multiple`, `fixed`, `heap`, `shared` and `reuse` scenarios (`checked_munmap` at `tests/c/mmap_determinism.c:38`). Only `multiple` runs in CI (cell `c-programs/mmap-determinism`, `tests/e2e/manifests/c-programs.yaml:3757`, ptrace `guest_args` `multiple`), and none of them unmaps part of a mapping.
- T3 (covers F6): on a host with huge pages reserved, map a `MAP_HUGETLB` region and check `EINVAL` for a misaligned partial unmap, then success for an aligned one. Skip with a stated reason when no huge pages are available.
- T4 (covers F7): write through a `MAP_SHARED` file mapping, unmap it, and read the data back with `pread`; do the same with a `MAP_PRIVATE` mapping and check that the file is unchanged.
  Existing candidate: `tests/c/file_backed_mmap.c` (cell `c-programs/file-backed-mmap`, `tests/e2e/manifests/c-programs.yaml:11823`, verify ci on ptrace) covers only the `MAP_PRIVATE` half: it unmaps a copy-on-write file mapping at lines 84 and 110 and checks with `pread` that the file still holds the original payload. The `MAP_SHARED` half still needs a test.
- T5 (covers F8): on a kernel with `mseal`, seal a mapping and check that `munmap` returns `EPERM` and that the mapping stays readable. Skip with a stated reason on older kernels.
- T6 (covers F9): install a `SIGSEGV` handler, unmap a page, touch it, and check `si_code == SEGV_MAPERR` and `si_addr`, under `hermit run --verify`.
- T7 (covers F10): put a futex word in a `MAP_SHARED` anonymous mapping shared with a forked child, unmap it in the parent, map a private page at the same address with `MAP_FIXED`, and check that a `FUTEX_WAKE` on the private word does not wake the child's waiter on the shared word.
  Related existing tests: `tests/c/mmap_determinism.c` scenario `shared` (cell `c-programs/mmap-determinism-shared`, `tests/e2e/manifests/c-programs.yaml:17756`) unmaps a shared mapping at line 104 but uses no futex; `tests/e2e/determinism-stress/mmap_fork_shared.c` (cell `determinism-stress-c/mmap-fork-shared`, `tests/e2e/manifests/determinism-stress-c.yaml:134`) unmaps a shared mapping after `fork` at line 171.
- T8 (covers none): unmap a page, then `mmap` the same length without a hint, and check that the address is the same in both runs of `hermit run --verify`.
  Existing candidate: the `reuse` scenario of `tests/c/mmap_determinism.c:107-119`, which has no CI cell at this SHA.
- U1 (covers F6; host-config): a `MAP_HUGETLB` mapping needs huge pages reserved on the host, and the measurement host reserves none (`vm.nr_hugepages` is 0), so CI cannot reach the huge-page alignment `EINVAL`. `handle_munmap` forwards the call through `record_or_replay` and returns the kernel's error unchanged (`detcore/src/syscalls/files.rs:2953`).
- U2 (covers F11; host-config): the host's `vm.max_map_count` decides the threshold, and at the measurement host 67108864 a guest cannot create enough VMAs to reach it. `handle_munmap` returns the kernel's `ENOMEM` unchanged (`detcore/src/syscalls/files.rs:2953`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel unmaps; Detcore removes the range from its shared-mapping futex model only after success); linux_determinism stays conditional (address-space layout, thread races, host vm.max_map_count); 10 flag behaviours and 8 tests listed; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (vm.max_map_count, huge page sizes and pool); F5 narrowed to the surviving parts; new F11, the max_map_count ENOMEM (host-config U2); host-config U1 for the huge-page alignment EINVAL of F6; no open question resolved

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `munmap`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+munmap&body=Discussion+for+the+%60munmap%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmunmap.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60munmap%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `munmap`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
