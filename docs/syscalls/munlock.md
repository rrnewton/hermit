---
schema: syscall-pages/v1
syscall: munlock
nr_x86_64: 150
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/309
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+munlock&body=Discussion+for+the+%60munlock%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmunlock.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60munlock%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 1
---

## Summary

`munlock(addr, len)` removes the memory lock from the pages covering `[addr, addr + len)`, so the kernel may page them out again. Detcore passes it to the kernel unchanged (`detcore/src/lib.rs:2843`).

## Linux semantics

`addr` is rounded down and `addr + len` rounded up to page boundaries. Each mapping in the range is marked unlocked, in address order. `munlock` is not checked against `RLIMIT_MEMLOCK` and needs no privilege. Locks do not nest, so one `munlock` undoes any number of `mlock` calls. Unlocking a range that was never locked succeeds. `munlock` does not clear `mlockall(MCL_FUTURE)`, so mappings created later are still locked.

Host results measured for this issue on Linux 7.1.3: a never-locked range returns 0; `munlock(addr + 100, 0)` returns 0 and leaves a locked page locked; a length that wraps returns `EINVAL`; a range with a hole returns `ENOMEM` after the page before the hole was already unlocked; an unmapped range returns `ENOMEM`; a `PROT_NONE` page returns 0; with the soft `RLIMIT_MEMLOCK` at 0 `munlock` still returns 0.

### Flags and options

- F1: `len == 0`: returns 0 and unlocks nothing
- F2: page rounding: the unlocked range runs from `addr` rounded down to `addr + len` rounded up
- F3: a fully mapped range: every page is unlocked, whether it was locked once, several times or never; `VmLck`, smaps `Locked` and the `lo` flag drop; returns 0
- F4: `addr + len` wraps: `EINVAL`
- F5: an unmapped range or a range with a hole: `ENOMEM`; pages before the hole are already unlocked
- F6: no limit or privilege check: succeeds with `RLIMIT_MEMLOCK` 0 and without `CAP_IPC_LOCK`, and on `PROT_NONE` mappings
- F7: `mlockall(MCL_FUTURE)` stays in force, so later mappings are still locked

## Determinism analysis

`linux_determinism: deterministic`. The result depends only on the arguments and the caller's own memory map. There is no limit check, no privilege check and no page fault, so no host state enters.

### Nondeterminism sources

- None. The only effect is on whether pages may be swapped, which a program cannot observe except through smaps and `VmLck`, which follow from its own calls.

## Hermit design

Target policy: `passthrough`. This matches the code at hermit f4055eb9cae4.

- `classify_syscall` returns `PassThrough` for `munlock` in the main pass-through block, under the comment "Fixed credentials, process-local unlocks, and guest-owned filesystem flushes are repeatable under the fixed-container model." (`detcore/src/syscall_classification.rs:747`; the arm is at `detcore/src/syscall_classification.rs:753`, and `detcore/src/syscall_classification.rs:749` carries `TODO-HUMAN-REVIEW(PR-654)`). The unit test `representative_policies_stay_in_their_reviewed_sections` pins it (`detcore/src/syscall_classification.rs:2060`).
- Dispatch is the generic arm at `detcore/src/lib.rs:2843`, which calls `passthrough` (`detcore/src/lib.rs:367`). Under record and replay, `passthru_opt` (`detcore/src/lib.rs:3140`) leaves it unsubscribed (`detcore/src/lib.rs:1084`).

Passing through is correct because the call reads only the guest's own memory map. `VmLck` in `/proc/self/status` is reported as 0 (`detcore/src/procfs.rs:1372`), and smaps `Locked` is kept from the host (`detcore/src/procfs.rs:3033`) and follows from the guest's own `mlock` and `munlock` calls.

### Gaps and open points

- If `mlock` becomes a model that does not lock physically, `munlock` must update the same Detcore state, or smaps would report locks the model never made. `munlock` itself can stay a pass-through as long as the model keeps `mlock`'s physical effect.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, `munlock` on a never-locked page returns 0; lock a page twice, `munlock(addr + 100, 0)` leaves it locked, `munlock(addr + 100, 1)` unlocks it; print smaps `Locked` after each step.
  Existing candidate for the never-locked case: `tests/c/syscall_quick_wins.c:57`, driven by `deterministic_passthrough_syscalls_verify` (`hermit-cli/tests/syscall_quick_wins.rs:29`), which runs `--backend=ptrace run --strict --verify` and checks the trace contains `inbound syscall: munlock(`.
- T2 (covers F4, F5): a wrapping length returns `EINVAL`; a range with a hole and an unmapped range return `ENOMEM`, and smaps afterwards shows the page before the hole unlocked.
- T3 (covers F6): `munlock` of a `PROT_NONE` page returns 0. The zero-limit case cannot be set up inside Hermit, because Detcore refuses to change `RLIMIT_MEMLOCK` (`detcore/src/syscalls/sysinfo.rs:147`); it needs a host-side launch with a zero soft limit.
- T4 (covers F7): `mlockall(MCL_CURRENT | MCL_FUTURE)`, `munlock` one page, then `mmap` a new page; smaps shows the new mapping locked.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/309#issuecomment-5872881584
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/309: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; munlock records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/309 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism deterministic and target_policy passthrough kept (reads only the guest's own memory map; no limit, privilege or fault); 7 flag behaviours and 4 tests; the never-locked case is exercised by hermit-cli/tests/syscall_quick_wins.rs (coverage not recorded)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `munlock`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+munlock&body=Discussion+for+the+%60munlock%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmunlock.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60munlock%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: munlock" (https://github.com/rrnewton/hermit/issues/309), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/309.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=munlock`, and by confirming hermit currently routes `munlock` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
munlock(0x406000, 4096)         = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/309#issuecomment-5872881584
