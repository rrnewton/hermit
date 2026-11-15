---
schema: syscall-pages/v1
syscall: madvise
nr_x86_64: 28
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/281
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+madvise&body=Discussion+for+the+%60madvise%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmadvise.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60madvise%60%2C+or+in+what+the+page+says%3F%0A
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

Advises the kernel about expected memory-usage patterns for a range (MADV_DONTNEED, MADV_FREE, MADV_WILLNEED, etc.). Detcore decides per advice value: hints and advice with guest-visible effects go to the kernel, reclaim and memory-policy advice gets a fixed success, and resource-dependent or dangerous advice gets a fixed error.

## Linux semantics

Advisory syscall; kernel may ignore hints. MADV_DONTNEED on anonymous memory is guaranteed to zero-fill on next access; MADV_FREE defers that until reclaim.

`madvise(addr, length, advice)` applies one advice value to `[addr, addr + length)`, with `length` rounded up to whole pages. The range may span several mappings. The kernel applies the advice mapping by mapping and returns `ENOMEM` if part of the range is unmapped, after applying it to the mapped parts. Some advice values also change what a later `fork` does, or how a later fault fills a page, and those effects are visible to the program.

### Flags and options

- F1: `MADV_NORMAL`, `MADV_RANDOM`, `MADV_SEQUENTIAL`, `MADV_WILLNEED`: access-pattern and readahead hints. They do not change memory contents.
- F2: `MADV_DONTNEED` and `MADV_DONTNEED_LOCKED` (Linux 5.18; also applies to locked pages): discard the pages. Afterwards a private anonymous page reads as zeros, a private file page is read again from the file, and a shared mapping keeps its contents.
- F3: `MADV_FREE` (Linux 4.5): private anonymous mappings only, `EINVAL` otherwise. The kernel may reclaim the pages lazily; until it does, they keep their old contents, and a write cancels the free.
- F4: `MADV_DONTFORK`, `MADV_DOFORK`: a child created by `fork` does not inherit the range, or inherits it again.
- F5: `MADV_WIPEONFORK`, `MADV_KEEPONFORK` (Linux 4.14): the child sees the range zero-filled, or no longer does. Only private anonymous mappings are allowed; the kernel stops with `EINVAL` at the first other mapping, after applying the advice to the mappings before it.
- F6: `MADV_DONTDUMP`, `MADV_DODUMP`: exclude the range from core dumps, or include it again.
- F7: `MADV_GUARD_INSTALL`, `MADV_GUARD_REMOVE` (Linux 6.13): install guard markers that make any access raise `SIGSEGV`, discarding the pages; or remove them, after which the range faults in afresh.
- F8: `MADV_MERGEABLE`, `MADV_UNMERGEABLE` (KSM), `MADV_HUGEPAGE`, `MADV_NOHUGEPAGE` (transparent huge pages), `MADV_COLD`, `MADV_PAGEOUT` (Linux 5.4, reclaim): memory-policy controls with no effect on contents. Each has its own mapping-type restrictions that give `EINVAL`.
- F9: `MADV_POPULATE_READ`, `MADV_POPULATE_WRITE` (Linux 5.14), `MADV_COLLAPSE` (Linux 6.1): do synchronous work, faulting pages in or collapsing them into huge pages. The result depends on available memory (`ENOMEM`, `EAGAIN`) and on the mapping (`EFAULT`, `EINVAL`).
- F10: `MADV_REMOVE`: punch a hole in the backing store of a shared mapping (tmpfs, shmem or a filesystem that supports it), visible through every mapping of the file.
- F11: `MADV_HWPOISON`, `MADV_SOFT_OFFLINE`: inject memory failures; these need `CAP_SYS_ADMIN` and give `EPERM` without it.
- F12: arguments: an unaligned `addr` or an unknown advice value gives `EINVAL`. `length == 0` returns 0 for a known advice value. A range that overflows the address space gives `EINVAL`. A partly unmapped range gives `ENOMEM`.
- F13: kernel support: a kernel older than the advice gives `EINVAL` for it, as for an unknown value: `MADV_GUARD_INSTALL` and `MADV_GUARD_REMOVE` before Linux 6.13, `MADV_DONTNEED_LOCKED` before 5.18, `MADV_WIPEONFORK` and `MADV_KEEPONFORK` before 4.14.

## Determinism analysis

Most advice is a pure hint with no effect on computed results, hence effectively deterministic. The exception is destructive/lazy advice: MADV_DONTNEED zeroes anonymous pages (a deterministic, observable effect) and MADV_FREE lazily reclaims pages whose reclaim timing is nondeterministic.

Two more sources depend on the host. The populate and collapse advice (F9) can fail with `ENOMEM` or `EAGAIN` depending on free memory and huge-page availability. The `ENOMEM` for a partly unmapped range (F12) depends on the process's own layout, so it repeats when the layout does. Everything else (F2, F4-F7, F10) has deterministic effects given the same mappings. That makes `linux_determinism` `conditional`.

### Nondeterminism sources

MADV_FREE: whether a freed page still holds its old contents on a subsequent read depends on host memory pressure/reclaim timing (nondeterministic). Other advice values have no observable nondeterminism.

### Cross-host variation

- Kernel version: the forwarded advice of F2, F5 and F7 succeeds on a kernel that knows it and gives `EINVAL` on an older one (F13). The fixed-result advice (F3, F8, F9, F11) does not reach the kernel and does not vary.

## Hermit design

Target policy: `model`. The kernel performs the advice whose effects the guest can see, and Detcore owns the decision for every advice value. The dispatch arm is `detcore/src/lib.rs:2404`; the policy table is `madvise_action` (`detcore/src/syscalls/memory.rs:35-85`), and the handler is `handle_madvise` (`detcore/src/syscalls/memory.rs:114-187`):

- Arguments are checked first, independently of the host (`validate_common_args`, `detcore/src/syscalls/memory.rs:87-99`, called at `detcore/src/syscalls/memory.rs:121`): an unaligned `addr` or an overflowing range gives `EINVAL`. With `length == 0`, an unknown advice value gives `EINVAL` and every other value gives 0 (`detcore/src/syscalls/memory.rs:123-128`). That covers F12 except for `ENOMEM`.
- Hints (F1) are forwarded through `record_or_replay` when the backend supports madvise (`detcore/src/syscalls/memory.rs:39-41`, `detcore/src/syscalls/memory.rs:139-140`). In record/replay mode they are accepted as no-ops (`detcore/src/syscalls/memory.rs:129-136`).
- Advice with guest-visible effects (F2, F4-F7) is forwarded through `record_or_replay` (`detcore/src/syscalls/memory.rs:45-54`, `detcore/src/syscalls/memory.rs:150-151`). A backend without madvise support gets `ENOSYS` instead (`detcore/src/syscalls/memory.rs:153-160`). Support defaults to true (`detcore-model/src/config.rs:83-86`), the ptrace backend sets it to true (`hermit-cli/src/metadata.rs:414`), and the KVM backend sets it to false (`hermit-cli/src/lib.rs:2105`).
- `MADV_FREE` (F3) and the memory-policy advice (F8) return a fixed 0 without reaching the kernel (`detcore/src/syscalls/memory.rs:60-66`, `detcore/src/syscalls/memory.rs:161-168`). Not reclaiming freed pages is a legal Linux outcome, so `MADV_FREE` always keeps the old contents under Hermit.
- `MADV_REMOVE` (F10) gives `EINVAL` (`detcore/src/syscalls/memory.rs:70`): the comment says Detcore cannot yet update file resources and replay every alias. Populate and collapse (F9) give `EINVAL` (`detcore/src/syscalls/memory.rs:75-77`), the error Linux uses for advice it does not support, so callers take their fallback path. `MADV_HWPOISON` and `MADV_SOFT_OFFLINE` (F11) give `EPERM` (`detcore/src/syscalls/memory.rs:81`). Unknown values give `EINVAL` (`detcore/src/syscalls/memory.rs:83`, `detcore/src/syscalls/memory.rs:178-185`).
- In record mode the recorder handles the forwarded advice (`hermit-cli/src/recorder.rs:562`, `hermit-cli/src/recorder/mmap.rs:262`). Replay replaces file mappings with anonymous ones, so the recorder records the page contents to refill after `MADV_DONTNEED`, `MADV_DONTNEED_LOCKED` and `MADV_GUARD_REMOVE` (`hermit-cli/src/recorder/mmap.rs:273-276`), and how far `MADV_WIPEONFORK` got before failing at a file mapping (`hermit-cli/src/recorder/mmap.rs:281-298`). The replayer's handler is at `hermit-cli/src/replayer.rs:459`.
- Unit tests `madvise_known_linux_advice_has_an_explicit_policy` (`detcore/src/syscalls/memory.rs:233`) and `common_argument_validation_is_host_independent` (`detcore/src/syscalls/memory.rs:285`) pin the table and the argument checks.

The legacy recommendation was pass-through. The code no longer does that: it forwards only the advice whose effects are deterministic, and decides the rest itself.

Why `model` and not the alternatives:

- `passthrough` would expose the reclaim timing of `MADV_FREE` (F3), the memory-dependent failures of F9, and the host memory-failure injection of F11.
- `emulate` for every value would mean Detcore zeroing pages, changing fork inheritance and installing guard pages itself. Those effects belong to the kernel, and the kernel produces them deterministically.
- `refuse` would break glibc's allocator and every runtime that returns memory with `MADV_DONTNEED` or `MADV_FREE`.

What remains before the code fully realizes `model`:

- **Mapping-dependent errors of the fixed-success advice.** `MADV_FREE` and F8 return 0 without the kernel, so a partly unmapped range gives 0 instead of `ENOMEM`, and `MADV_FREE` on a shared or file mapping gives 0 instead of `EINVAL`. The comment at `detcore/src/syscalls/memory.rs:56-59` says this is deliberate. Hints in record/replay mode skip the `ENOMEM` check in the same way (`detcore/src/syscalls/memory.rs:129-136`). Whether these errors should be reproduced is an open question below.
- **`MADV_REMOVE` always fails.** On a tmpfs or shmem shared mapping Linux succeeds, so a program that relies on it to free shared memory sees `EINVAL` under Hermit and must take a fallback path.
- **Human review.** The handler is tagged `TODO-HUMAN-REVIEW(#548)` (`detcore/src/syscalls/memory.rs:113`, https://github.com/rrnewton/hermit/issues/548), and the recorder handler is tagged with https://github.com/rrnewton/hermit/pull/3589 (`hermit-cli/src/recorder/mmap.rs:261`).

`linux_determinism` stays `conditional`. Hermit removes the reclaim and memory-pressure sources by never forwarding the advice they affect.

## Recommended tests

- T1 (covers F1, F3, F12): `MADV_WILLNEED` and `MADV_FREE` keep the page contents. `MADV_POPULATE_READ`, an unknown advice value and an unaligned `addr` with `length == 0` give `EINVAL`. The run must be identical under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/madvise_determinism.c:195-203` (cell `c-programs/madvise-determinism`, `tests/e2e/manifests/c-programs.yaml:3320`, verify ci on ptrace), also run by `madvise_policy_verifies_in_run_record_and_kvm_modes` (`hermit-cli/tests/madvise.rs:38`). The rr-suite entries `rr_madvise` and `rr_madvise_wipeonfork` are ignored (`hermit-cli/tests/fail_closed_allowed_ignores.tsv:90-91`).
- T2 (covers F2): `MADV_DONTNEED` zeroes a private anonymous page, rereads a private file page and a read-only file page from the file, and keeps a shared file page; repeat with `MADV_DONTNEED_LOCKED` on a locked page. Run, record and replay must all agree.
  Existing candidate: the same program (anonymous, private file, read-only file, shared file, memfd and own-text cases), run in all three modes by `hermit-cli/tests/madvise.rs:38`. `MADV_DONTNEED_LOCKED` is not exercised.
- T3 (covers F4, F5): after `MADV_DONTFORK`, the child faults on the range; after `MADV_DOFORK`, it inherits it again. After `MADV_WIPEONFORK`, the child sees zeros; across an anonymous page followed by a private file page, the call gives `EINVAL` and the child sees only the anonymous page wiped. After `MADV_KEEPONFORK`, the child keeps the contents.
  Existing partial coverage: the partial `MADV_WIPEONFORK` case in `tests/c/madvise_determinism.c:141-165`.
- T4 (covers F6, F7): `MADV_DONTDUMP`/`MADV_DODUMP` return 0. `MADV_GUARD_INSTALL` makes an access raise `SIGSEGV`, caught by a handler; `MADV_GUARD_REMOVE` makes the range usable again and it reads as zeros. Skip the guard part on kernels older than 6.13.
- T5 (covers F8, F9, F10, F11): each of `MADV_MERGEABLE`, `MADV_UNMERGEABLE`, `MADV_HUGEPAGE`, `MADV_NOHUGEPAGE`, `MADV_COLD`, `MADV_PAGEOUT` returns 0; `MADV_POPULATE_WRITE`, `MADV_COLLAPSE` and `MADV_REMOVE` give `EINVAL`; `MADV_HWPOISON` and `MADV_SOFT_OFFLINE` give `EPERM`. Every result must be identical under `--verify`.
- T6 (covers F3, F8, F12): `MADV_FREE` and `MADV_COLD` on a partly unmapped range, and `MADV_FREE` on a `MAP_SHARED` mapping. Record what Hermit returns next to native Linux (`ENOMEM`, `EINVAL`), so the open question below has evidence.
- U1 (covers F13; host-config): the CI host's kernel knows every advice value listed, so CI cannot produce the old-kernel `EINVAL`. Detcore forwards these values to the kernel (`detcore/src/syscalls/memory.rs:45-54`, `detcore/src/syscalls/memory.rs:150-151`) and returns its answer unchanged.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/281#issuecomment-5872819801
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/281: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; madvise records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/281 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> model (Detcore decides per advice: forward hints and guest-visible advice, fixed success for reclaim and memory-policy advice, fixed errors for populate, remove and memory-failure advice); linux_determinism stays conditional; 12 behaviours and 6 tests listed; open question: should the fixed-success advice reproduce the mapping-dependent ENOMEM/EINVAL errors; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (kernel version); new F13, the old-kernel EINVAL for forwarded advice (host-config U1); no open question resolved
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/madvise_determinism.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `madvise`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+madvise&body=Discussion+for+the+%60madvise%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmadvise.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60madvise%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: madvise" (https://github.com/rrnewton/hermit/issues/281), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/281.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=madvise`, and by confirming hermit currently routes `madvise` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
madvise(0x7fbb19e22000, 4096, MADV_DONTNEED) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/281#issuecomment-5872819801

### Open questions

- Should `MADV_FREE` and the memory-policy advice (`MADV_MERGEABLE`, `MADV_UNMERGEABLE`, `MADV_HUGEPAGE`, `MADV_NOHUGEPAGE`, `MADV_COLD`, `MADV_PAGEOUT`), and hints in record/replay mode, reproduce Linux's mapping-dependent errors: `ENOMEM` for a partly unmapped range, and `EINVAL` for `MADV_FREE` on a shared or file mapping? Today they return a fixed 0 (`detcore/src/syscalls/memory.rs:56-66`, `detcore/src/syscalls/memory.rs:129-136`, `detcore/src/syscalls/memory.rs:161-168` at hermit f4055eb9cae4). Forwarding them would bring back the reclaim nondeterminism of `MADV_FREE`, so reproducing the errors would need Detcore to check the mappings itself. This is an owner decision on how exact error parity must be; T6 gathers the evidence.
