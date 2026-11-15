---
schema: syscall-pages/v1
syscall: mremap
nr_x86_64: 25
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mremap&body=Discussion+for+the+%60mremap%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmremap.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mremap%60%2C+or+in+what+the+page+says%3F%0A
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

`mremap` grows, shrinks or moves an existing memory mapping and returns its new address. Under Hermit the kernel performs the call, and Detcore then updates its own model of shared mappings, which it uses to compute futex keys.

## Linux semantics

`mremap(old_address, old_size, new_size, flags, new_address)` resizes the mapping that starts at `old_address`. Page contents are kept; when the mapping grows, the new tail is filled the way the mapping's type fills it (zeros for anonymous memory, file pages for a file mapping). Without `MREMAP_MAYMOVE` the mapping must stay where it is. With it, the kernel may move it to an address it chooses, using the same top-down search as `mmap`. The return value is the new start address, or `MAP_FAILED` with `errno`.

The old range must lie inside a single mapping. Locked (`mlock`) pages stay locked after the move. Moving a mapping moves its page tables; it does not copy data.

### Flags and options

- F1: no flags: resize in place. Shrinking unmaps the tail. Growing succeeds only if the pages after the mapping are free, otherwise `ENOMEM`.
- F2: `MREMAP_MAYMOVE`: if the mapping cannot grow in place, the kernel moves it to an address of its own choosing and returns that address.
- F3: `MREMAP_FIXED` (requires `MREMAP_MAYMOVE`): move the mapping to exactly `new_address`, unmapping whatever was mapped there. `new_address` must be page-aligned, and the new range must not overlap the old one (`EINVAL`).
- F4: `MREMAP_DONTUNMAP` (Linux 5.7, requires `MREMAP_MAYMOVE` and `old_size == new_size`): move the page tables but leave the old range mapped. Afterwards the old range of a private anonymous mapping reads as zeros, and a file or shared mapping refaults from its backing object. Before Linux 5.13 only private anonymous mappings were allowed.
- F5: `old_size == 0` with `MREMAP_MAYMOVE` on a shared mapping: create a second mapping of the same pages at a new address and leave the old one in place.
- F6: `old_size` and `new_size` are rounded up to whole pages. `new_size == 0` is `EINVAL`.
- F7: errors: an unaligned `old_address`, unknown flag bits, or `MREMAP_FIXED`/`MREMAP_DONTUNMAP` without `MREMAP_MAYMOVE` give `EINVAL`. An old range that is not a single valid mapping gives `EFAULT`. Running out of address space or map entries gives `ENOMEM`. Exceeding `RLIMIT_MEMLOCK` for a locked mapping gives `EAGAIN`.

## Determinism analysis

Page contents are preserved, so the data a program sees after `mremap` is deterministic. The address is not. When the kernel moves a mapping (F2), it picks the new address with its free-range search, which depends on where the address space starts (ASLR) and on every earlier `mmap`, `munmap` and `brk` in the process. Whether a grow succeeds in place (F1), and so whether a `MREMAP_MAYMOVE` call moves at all, depends on whether the next pages happen to be free, which is the same layout question. The errors in F7 depend on that layout and on resource limits, which are inputs the program set or inherited.

Linux repeats `mremap` exactly when the start of the address space is fixed and the process's mapping history is the same. That makes `linux_determinism` `conditional`.

## Hermit design

Target policy: `model`. The kernel keeps the address space and performs every resize and move. Detcore keeps the one piece of mapping state it needs to decide on its own: which shared object backs each address, so that futexes on shared memory get the same key in every process that maps it. The dispatch arm is `detcore/src/lib.rs:2409`, and the handler is `handle_mremap` at `detcore/src/syscalls/files.rs:2959`:

- The call goes to the kernel unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:2967`). The kernel's flag checks, its choice of address and its errors are therefore kept, and an error returns before Detcore touches its model.
- After a successful call, Detcore updates its shared-mapping model through `remap_memory` (`detcore/src/syscalls/files.rs:2970-2972`, `detcore/src/tool_local.rs:2482-2493`). `MemoryMetadata::remap` (`detcore/src/memory.rs:206-232`) finds the tracked shared mapping that covers the whole old range, unmaps the old and new ranges in the model, and records the new range with the same object and offset. With `old_size == 0` the old range is empty, so the old alias stays and a new one is added, as F5 requires.
- The model is what `futex_id` reads (`detcore/src/memory.rs:100-112`): an address inside a tracked shared mapping gets a key made of the shared object and offset; any other address gets a private key. The unit test `unmap_and_remap_preserve_only_live_aliases` (`detcore/src/memory.rs:339`) checks the alias bookkeeping.
- The address the kernel returns is deterministic when the guest's address-space layout is. Hermit's recorder relies on the same assumption for anonymous mappings ("ASLR is disabled", `hermit-cli/src/recorder/mmap.rs:206-207`), and the `c-programs/mmap-layout-pointer-order` cell checks it directly for `mremap` (T1).
- The record/replay tool layer does not subscribe to `mremap`. It reaches the catch-all live injection in the recorder (`hermit-cli/src/recorder.rs:648`) and in the replayer (`hermit-cli/src/replayer.rs:575`), so replay repeats the resize against the replayed address space.

Why `model` and not the alternatives:

- `passthrough` alone would leave Detcore's shared-mapping model stale after a move. A futex on the moved range would then get a private key, and a wake from another process that maps the same object would miss its waiter.
- `emulate` would mean Detcore keeps the whole address space itself, which it does not do for `mmap` either (`handle_mmap`, `detcore/src/syscalls/files.rs:2889`).
- `refuse` would break every `realloc` of a large block in glibc, which uses `mremap`.

What remains before the code fully realizes `model`:

- **`MREMAP_DONTUNMAP` on a shared mapping (F4).** Since Linux 5.13 the old range stays mapped to the same shared object. `MemoryMetadata::remap` always unmaps the old range in the model (`detcore/src/memory.rs:227`), so after such a call a futex on the old range would get a private key while Linux treats it as shared. This is a hypothesis from reading the code, not a measurement; T3 confirms or refutes it.
- **No test exercises F3, F4, F5 or F7.** The existing candidates below only grow and shrink private anonymous mappings with `MREMAP_MAYMOVE`.

`linux_determinism` stays `conditional`. Under Hermit the address is deterministic because the layout and the mapping history are.

## Recommended tests

- T1 (covers F1, F2, F6): grow and shrink a private anonymous mapping with and without `MREMAP_MAYMOVE`, including sizes that are not page multiples. Contents must be preserved, and the returned addresses and whether the mapping moved must be identical under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/mmap_stress_determinism.c:60-80` (cell `c-programs/mmap-stress-determinism`, `tests/e2e/manifests/c-programs.yaml:3834`, verify ci on), `tests/c/mmap_layout_pointer_order.c:170-185` (cell `c-programs/mmap-layout-pointer-order`, `tests/e2e/manifests/c-programs.yaml:16690`, verify ci on ptrace), and `mmap_stress_layout_and_checksum_are_deterministic` at `hermit-cli/tests/mmap_stress_determinism.rs:107`. None of them calls `mremap` without `MREMAP_MAYMOVE`. The rr-suite `mremap` programs (`hermit-cli/tests/rr_suite.rs:342-354`) are ignored, for example `rr_mremap` at `hermit-cli/tests/fail_closed_allowed_ignores.tsv:119`.
- T2 (covers F3): move a mapping with `MREMAP_FIXED` over an existing mapping and check that the target's old contents are gone. Also check `EINVAL` for an overlapping target and for `MREMAP_FIXED` without `MREMAP_MAYMOVE`.
- T3 (covers F4): `MREMAP_DONTUNMAP` on a private anonymous mapping (the old range must read as zeros) and on a `MAP_SHARED` mapping. For the shared case, a forked child waits on a futex through the old address and the parent wakes it through the new one; the wake must succeed.
- T4 (covers F5): with `old_size == 0`, alias a `MAP_SHARED|MAP_ANONYMOUS` mapping, then wake a futex waiter through the alias from a forked child that waits on the original.
- T5 (covers F7): check every error: unaligned `old_address`, unknown flag bits, `new_size == 0`, an old range that crosses two mappings or is unmapped, a grow without `MREMAP_MAYMOVE` that hits a neighbouring mapping, and `MREMAP_DONTUNMAP` with a size change.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (the kernel performs the resize or move; Detcore updates its shared-mapping model so futex keys follow the mapping); linux_determinism stays conditional; 7 flag behaviours and 5 tests listed; the MREMAP_DONTUNMAP shared-alias gap is a code-reading hypothesis; no coverage token recorded, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mremap`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mremap&body=Discussion+for+the+%60mremap%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmremap.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mremap%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `mremap`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
