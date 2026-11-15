---
schema: syscall-pages/v1
syscall: mincore
nr_x86_64: 27
linux_determinism: nondeterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/285
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mincore&body=Discussion+for+the+%60mincore%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmincore.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mincore%60%2C+or+in+what+the+page+says%3F%0A
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

Reports which pages of a mapping are currently resident in RAM (returns a per-page residency vector). Under Hermit the kernel still checks the arguments, but Detcore replaces the residency vector with a constant answer: every page resident.

## Linux semantics

`mincore(addr, length, vec)` writes one byte per page of `[addr, addr + length)` into `vec`. The low bit of each byte is set when the page is resident in memory; the other bits are reserved. The result is a snapshot of kernel memory-management state and can be stale by the time the caller reads it. The call has no side effect on the mapping.

The kernel checks the arguments in this order: page alignment of `addr`, then that the range lies in user address space, then that `vec` is writable for the number of pages, then that every page of the range is mapped.

### Flags and options

- F1: residency vector: `vec` receives `ceil(length / page size)` bytes, one per page, with the low bit set for a resident page. A private anonymous page that was never touched reports not resident.
- F2: `length == 0` returns 0 and writes nothing.
- F3: an `addr` that is not page-aligned gives `EINVAL`.
- F4: a range that is not entirely mapped, or that extends past the user address space, gives `ENOMEM`.
- F5: a `vec` that cannot be written for the whole page count gives `EFAULT`.
- F6: file mappings: since Linux 5.0, page-cache residency of a file mapping is reported only when the caller owns the file or could open it for writing (Linux 5.2 relaxed the stricter 5.0 rule to this). Otherwise only pages mapped into the caller's own page tables count as resident.

## Determinism analysis

Residency depends on the host page cache, memory pressure, prior faulting, and readahead - none of which are a function of the guest's own logic - so the returned vector varies run-to-run.

The checks in F2-F5 depend only on the arguments and on the process's own mappings, so they repeat whenever the address-space layout repeats. Only the bits of the vector (F1, F6) come from host state. Since the call exists to return those bits, `linux_determinism` stays `nondeterministic`.

### Nondeterminism sources

The residency bitmap reflects host memory-management state (page cache, reclaim, readahead), which is not reproducible.

## Hermit design

Target policy: `emulate`. Detcore synthesizes the residency answer: every mapped page reports resident. The kernel is used only for its argument and mapping checks. The dispatch arm is `detcore/src/lib.rs:2407`, and the handler is `handle_mincore` at `detcore/src/syscalls/memory.rs:203`:

- An unaligned `addr` returns `EINVAL` before any kernel call (`detcore/src/syscalls/memory.rs:209-212`), as F3 requires.
- `length == 0` returns 0 without a kernel call (`detcore/src/syscalls/memory.rs:214-216`), as F2 requires.
- An `addr + length` that overflows returns `EINVAL` (`detcore/src/syscalls/memory.rs:218`).
- A NULL `vec` returns `EFAULT` before the kernel call (`detcore/src/syscalls/memory.rs:220`).
- Otherwise the real call is injected (`detcore/src/syscalls/memory.rs:221`), so the kernel's `ENOMEM` and `EFAULT` checks (F4, F5) are kept. On success Detcore overwrites `vec` with `ceil(length / page size)` bytes of 1 (`detcore/src/syscalls/memory.rs:219`, `detcore/src/syscalls/memory.rs:222-223`).
- The doc comment (`detcore/src/syscalls/memory.rs:189-200`) records why the handler exists: GNU grep calls `mincore` under the KVM backend, and leaving it unsupported aborted the guest under `--strict`.
- The handler calls `guest.inject` directly rather than `record_or_replay`. The record/replay tool layer does not subscribe to `mincore`; it reaches the catch-all live injection in the recorder (`hermit-cli/src/recorder.rs:648`) and in the replayer (`hermit-cli/src/replayer.rs:575`).

Why `emulate` and not the alternatives:

- `model` would need a deterministic account of which pages are resident, which would mean simulating the page cache and reclaim. Nothing in the guest's own actions determines residency, so there is nothing for a model to follow.
- `passthrough` returns host state, which differs from run to run.
- `refuse` breaks programs such as grep that call `mincore` as an optimization hint.
- All-resident is a legal Linux answer: a system with no memory pressure that has already faulted every page in reports exactly this. Callers use the vector only as a hint.

What remains before the code fully realizes `emulate`:

- **Error order for an overflowing range.** By my reading of the kernel's argument checks, an `addr + length` past the end of user address space fails the range check with `ENOMEM` (F4). Detcore returns `EINVAL` for an arithmetic overflow (`detcore/src/syscalls/memory.rs:218`). This is a hypothesis from code reading; T3 settles it.
- **NULL `vec` on an unmapped range.** Linux checks `vec` only for writability of the page count, which a NULL pointer passes, and then fails the mapping check with `ENOMEM`. Detcore returns `EFAULT` before the kernel sees the call (`detcore/src/syscalls/memory.rs:220`). This is also a hypothesis from code reading; T3 settles it.
- **Never-touched pages (F1) and the file-mapping rule (F6) are not reproduced by design.** Detcore reports them resident. This is the intended emulation, not a gap, but a program that uses `mincore` to detect pages it has not touched sees a different answer than on a fresh Linux run.
- **Human review.** The arm and the handler are tagged `TODO-HUMAN-REVIEW(#775)` (`detcore/src/lib.rs:2405-2406`, `detcore/src/syscalls/memory.rs:202`, https://github.com/rrnewton/hermit/issues/775).

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5): on an anonymous mapping, check that every page reports resident, that `length == 0` returns 0, that a NULL `vec` gives `EFAULT`, that an unmapped range gives `ENOMEM`, and that an unaligned `addr` gives `EINVAL`. The run must be bitwise identical under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/mincore_residency.c` (cell `c-programs/mincore-residency`, `tests/e2e/manifests/c-programs.yaml:12511`, verify ci on, ptrace enabled, `bitwise_parity: true`). It makes all six checks and exits 1 unless all pass. The rr-suite entry `rr_mincore` (`hermit-cli/tests/rr_suite.rs:306`) is ignored (`hermit-cli/tests/fail_closed_allowed_ignores.tsv:98`).
- T2 (covers F1, F6): `mincore` on a private anonymous mapping that was never touched, and on a read-only mapping of a file the caller neither owns nor can write, and has not read. Hermit must report every page resident in both runs of `--verify`.
- T3 (covers F4, F5): an `addr + length` that overflows the address space, and a NULL `vec` on an unmapped range. Record what Linux returns natively and what Hermit returns; if they differ (the code suggests `EINVAL` versus `ENOMEM` and `EFAULT` versus `ENOMEM`), file the difference as an error-order defect.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/285#issuecomment-5872828669
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/285: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; mincore records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/285 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (the kernel validates the arguments, then Detcore reports every page resident); linux_determinism stays nondeterministic; 6 behaviours and 3 tests listed; two error-order differences are code-reading hypotheses; no coverage token recorded, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/mincore_residency.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `mincore`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+mincore&body=Discussion+for+the+%60mincore%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmincore.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60mincore%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: mincore" (https://github.com/rrnewton/hermit/issues/285), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/285.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=mincore`, and by confirming hermit currently routes `mincore` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
mincore(0x406000, 4096, [0])    = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/285#issuecomment-5872828669
