---
schema: syscall-pages/v1
syscall: mmap
nr_x86_64: 9
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mmap&body=Discussion+for+the+%60mmap%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmmap.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mmap%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`mmap(addr, length, prot, flags, fd, offset)` creates a memory mapping, anonymous or backed by a file, and returns its address. Detcore handles it in `handle_mmap` (`detcore/src/syscalls/files.rs:2889`): the kernel creates the mapping and chooses the address, and Detcore records every `MAP_SHARED` range in its shared-memory model, which it uses to key non-private futexes. Address determinism rests on address-space randomization being disabled for the guest; record/replay records the address and contents of file mappings and rebuilds them as anonymous memory on replay.

Manual reference: Linux man-pages mmap(2).

## Linux semantics

Without `MAP_FIXED`, the kernel picks the address, using `addr` only as a hint. On x86_64 it searches top-down from `mmap_base`, which is randomized unless the process runs with `ADDR_NO_RANDOMIZE`. The chosen address is therefore a function of the randomization setting and of every earlier mapping, unmapping and `brk` in the process.

A private file mapping is copy-on-write: stores stay in the process, and whether pages not yet written show later changes to the file is unspecified. A shared file mapping writes through to the page cache, so other mappers and `read(2)` see stores, and the guest sees stores made by anyone else. Anonymous shared memory is shared with children after `fork`. Accessing a page that lies wholly beyond the end of the file raises `SIGBUS`; the tail of the last page reads as zeros.

Errors: `EINVAL` (zero `length`, unaligned `offset`, unaligned `addr` with `MAP_FIXED`, or neither or both of `MAP_PRIVATE` and `MAP_SHARED`); `EBADF` (bad `fd` for a file mapping); `EACCES` (file not open for reading, or `PROT_WRITE` with `MAP_SHARED` on a descriptor not open for writing); `ENODEV` (filesystem without mmap); `EPERM` (`PROT_EXEC` on a `noexec` mount, or a sealed file); `ENOMEM` (no space, `RLIMIT_AS`, or `vm.max_map_count`); `EOVERFLOW` (offset plus length overflows); `EAGAIN` (`MAP_LOCKED` over `RLIMIT_MEMLOCK`); `EEXIST` (`MAP_FIXED_NOREPLACE` over an existing mapping); `EOPNOTSUPP` (unknown flag with `MAP_SHARED_VALIDATE`).

### Flags and options

- F1: `MAP_PRIVATE | MAP_ANONYMOUS` (`fd` -1): private zero-filled memory at a kernel-chosen address
- F2: `MAP_SHARED | MAP_ANONYMOUS`: zero-filled memory shared with children after `fork`; futexes in it are keyed by the shared object, not the address
- F3: `MAP_PRIVATE` file mapping: a copy-on-write view of the file starting at `offset`
- F4: `MAP_SHARED` file mapping: stores reach the file and every other mapping of it; futexes in it are keyed by file and offset
- F5: `MAP_SHARED_VALIDATE`: like `MAP_SHARED` but rejects unknown flags with `EOPNOTSUPP`; required for `MAP_SYNC`
- F6: `addr` hint without `MAP_FIXED`: used if the range is free, otherwise the kernel picks another address
- F7: `MAP_FIXED`: map exactly at `addr`, silently replacing existing mappings; `MAP_FIXED_NOREPLACE`: fail with `EEXIST` instead of replacing
- F8: `prot`: `PROT_NONE`, `PROT_READ`, `PROT_WRITE`, `PROT_EXEC`, with the access checks against the descriptor's open mode and the mount
- F9: population and accounting flags: `MAP_POPULATE`, `MAP_NONBLOCK`, `MAP_LOCKED`, `MAP_NORESERVE`
- F10: placement flags: `MAP_32BIT` (first 2 GiB on x86_64), `MAP_GROWSDOWN`, `MAP_STACK`; `MAP_DENYWRITE`, `MAP_EXECUTABLE` and `MAP_FILE` are accepted and ignored
- F11: `MAP_HUGETLB` with `MAP_HUGE_2MB` or `MAP_HUGE_1GB`: needs reserved huge pages on the host; `ENOMEM` when the pool is empty
- F12: `offset` and file length: `offset` must be page-aligned; pages wholly past end of file raise `SIGBUS` on access, and the tail of the last page reads as zeros
- F13: argument errors: `EINVAL`, `EBADF`, `EACCES`, `ENODEV`, `EPERM`, `ENOMEM`, `EOVERFLOW` and `EAGAIN`, as listed above

## Determinism analysis

`linux_determinism: conditional`. With randomization disabled, the kernel's choice of address is a pure function of the process's earlier mapping history, and anonymous memory always starts zero-filled, so a program with a deterministic syscall sequence gets the same addresses every run. It does not repeat when randomization is on (the default for ordinary processes), when the mapping history differs (a different environment size or library set shifts later mappings), when file contents differ, or when stores from another process or thread reach a shared mapping in a different order.

### Nondeterminism sources

- Address-space randomization of `mmap_base`
- Earlier mappings that differ between runs, including those made by the dynamic loader for a different library set
- File contents at the time pages are faulted in, for `MAP_PRIVATE` and `MAP_SHARED` file mappings
- Interleaving of stores to shared mappings by other threads or processes, including processes outside the guest that map the same file
- Host resources: huge page pool, `RLIMIT_MEMLOCK`, `vm.max_map_count`

## Hermit design

Target policy: `model`. The kernel performs the mapping and chooses its address; Detcore makes that choice repeatable and models the shared ranges. At hermit f4055eb9cae4:

- The dispatch arm `detcore/src/lib.rs:2403` calls `handle_mmap` (`detcore/src/syscalls/files.rs:2889`).
- For `MAP_SHARED` (F2, F4, F5), Detcore first works out the backing object: `fd == -1` is anonymous; otherwise the object is the descriptor's device and inode, or its open-file id when no stat is cached. A negative `offset` gives `EINVAL` before the call.
- The call itself goes through `record_or_replay` (`detcore/src/syscalls/files.rs:2927`), so in run mode the kernel performs it with every flag and error unchanged (F1, F3, F6 to F13).
- After success, Detcore removes the new range from its shared-memory model (`unmap_memory`, `detcore/src/tool_local.rs:2474`), which drops any shared range a `MAP_FIXED` replacement overwrote, and then records an anonymous (`map_shared_anonymous`, `detcore/src/tool_local.rs:2452`) or file-backed (`map_shared_object`, `detcore/src/tool_local.rs:2460`) shared range. `futex_id` (`detcore/src/tool_local.rs:2440`) uses that model to key non-private futexes, so two processes waiting on one shared word meet in the same wait queue. `munmap` (`detcore/src/syscalls/files.rs:2946`) and `mremap` (`detcore/src/syscalls/files.rs:2959`) keep the model in step.
- Address determinism comes from the ptrace backend, which starts the guest with `personality(PER_LINUX | ADDR_NO_RANDOMIZE)` (reverie submodule at d766df20b780, `reverie-ptrace/src/tracer.rs:3976`), together with Detcore's deterministic syscall order. Interleaving of stores to shared memory is fixed by the sequentialized scheduler.
- Record/replay: the recorder lets anonymous mappings run live, relying on randomization being off (`hermit-cli/src/recorder/mmap.rs:207`), and for file mappings records the address and the bytes up to the physical file length (`hermit-cli/src/recorder/mmap.rs:201`). The replayer (`hermit-cli/src/replayer/mmap.rs:42`) maps anonymous memory at the recorded address, asserts it got that address, writes the recorded bytes and restores the protection with `mprotect`.

Why `model`: Detcore must know which ranges are shared to key futexes correctly, which `passthrough` would not give; `emulate` would mean owning the address space; `refuse` would stop every dynamically linked program.

What the model does not cover:

- On replay, a `MAP_SHARED` file mapping becomes private anonymous memory filled once with the recorded bytes. This is read from `hermit-cli/src/replayer/mmap.rs:72`, not measured: stores no longer reach the file, and later `write(2)` calls to the file no longer show up in the mapping, so a replayed program that relies on that coherence may diverge.
- In run mode the content of a file mapping is whatever the file holds; Hermit controls it only for files the guest wrote.

## Recommended tests

- T1 (covers F1, F6, F7): map three private anonymous regions of different sizes, a `MAP_FIXED` region at a known address, and a region that reuses a just-unmapped range; print every address. All addresses must match across `hermit run --verify` runs on ptrace.
  Existing candidates: `tests/c/mmap_determinism.c` (cell `c-programs/mmap-determinism`, `tests/e2e/manifests/c-programs.yaml:3757`, verify ci on ptrace), whose scenarios are `multiple`, `fixed`, `heap`, `shared` and `reuse`; its verify cell passes only `multiple` on ptrace. `tests/c/mmap_stress_determinism.c` (cell `c-programs/mmap-stress-determinism`, `tests/e2e/manifests/c-programs.yaml:3834`) covers many anonymous maps with `mremap` and `mprotect`.
- T2 (covers F2): a `MAP_SHARED | MAP_ANONYMOUS` region written by a parent and a forked child, with a `FUTEX_WAIT`/`FUTEX_WAKE` handoff on a word inside it; values and wake order must match across runs.
  Existing candidates: `tests/c/mmap_determinism_shared.c` (cell `c-programs/mmap-determinism-shared`, `tests/e2e/manifests/c-programs.yaml:17756`), which prints only the address, and `tests/c/shm_coherency_identity.c` (cell `system-utils/shm-coherency-identity`, `tests/e2e/manifests/system-utils.yaml:1650`, verify ci on ptrace), which checks what each process observes through a shared mapping.
- T3 (covers F3, F12): a `MAP_PRIVATE` mapping of a guest-written file shows its bytes, a store is copy-on-write and does not reach the file, and the tail of the last page past end of file reads as zeros.
  Existing candidate: `tests/c/file_backed_mmap.c` (cell `c-programs/file-backed-mmap`, `tests/e2e/manifests/c-programs.yaml:11823`, verify ci on ptrace); it does not check the zero tail.
- T4 (covers F4, F5): a `MAP_SHARED` mapping of a guest-written file: stores reach the file after `msync` and `pread` shows them; a `write(2)` to the file shows up in the mapping; `MAP_SHARED_VALIDATE` with an unknown flag gives `EOPNOTSUPP`. Run it under record and replay as well to measure the replay coherence gap above.
  Existing partial candidate: `tests/c/msync_writeback.c` (cell `c-programs/msync-writeback`, `tests/e2e/manifests/c-programs.yaml:14596`, verify ci on ptrace), which covers the store, `msync` and `pread` path.
- T5 (covers F8, F13): `PROT_WRITE | MAP_SHARED` on a read-only descriptor gives `EACCES`; zero length, unaligned offset and missing `MAP_PRIVATE`/`MAP_SHARED` give `EINVAL`; a closed descriptor gives `EBADF`; `MAP_FIXED_NOREPLACE` over an existing mapping gives `EEXIST`.
- T6 (covers F9, F10, F11): `MAP_POPULATE`, `MAP_NORESERVE`, `MAP_32BIT` (address below 2 GiB) and `MAP_GROWSDOWN` succeed with addresses identical across runs; `MAP_HUGETLB` returns the same result (success or `ENOMEM`) in both runs on one host.
- T7 (covers F3, F4): record a guest that maps a file it then deletes, and replay it; the replayed contents must equal the recorded bytes.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (the kernel maps and picks the address with ASLR off; Detcore models MAP_SHARED ranges for futex keys; record/replay rebuilds file maps from recorded bytes); linux_determinism stays conditional; 13 flag behaviours and 7 tests listed; open question on replay coherence of MAP_SHARED file maps; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mmap`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mmap&body=Discussion+for+the+%60mmap%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmmap.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mmap%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `mmap`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Replay of `MAP_SHARED` file mappings: the replayer turns them into anonymous memory filled once with the recorded bytes (`hermit-cli/src/replayer/mmap.rs:72`). By reading the code, stores no longer reach the file and later writes to the file no longer appear in the mapping during replay. Not measured; T4 under record and replay measures it. Whether replay should restore the coherence is a design question for the owner.
- The `c-programs/mmap-determinism` verify cell passes only the `multiple` scenario on ptrace (`tests/e2e/manifests/c-programs.yaml:3757`); `shared` has its own wrapper cell, but `fixed`, `heap` and `reuse` have no verify cell.
- Address determinism depends on the ptrace backend's `ADDR_NO_RANDOMIZE` personality, which lives in the reverie submodule rather than in Hermit.
- For a `MAP_SHARED` file mapping with a negative offset, Detcore returns `EINVAL` without calling the kernel (`detcore/src/syscalls/files.rs:2906`). Linux checks alignment first and then page-offset overflow, so for a page-aligned negative offset it may report a different errno (for example `EOVERFLOW`). Not measured; T5 could include this case.
