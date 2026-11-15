---
schema: syscall-pages/v1
syscall: munlockall
nr_x86_64: 152
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/310
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+munlockall&body=Discussion+for+the+%60munlockall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmunlockall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60munlockall%60%2C+or+in+what+the+page+says%3F%0A
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

`munlockall()` removes every memory lock in the calling process: page locks from `mlock`, `mlock2` and `mlockall`, and the `MCL_FUTURE` default. Hermit forwards it to the host kernel unchanged. The call takes no arguments, checks no limit or capability, and returns 0.

## Linux semantics

`munlockall` takes the process's memory-map lock, clears the lock bits from the default flags for new mappings, and clears them from every existing mapping. That is `apply_mlockall_flags(0)`, the same helper `mlockall` uses. It acts on the whole memory map (`mm`), so all threads of the process see the effect. Pages stay mapped and resident; unlocking only lets the kernel reclaim them later. No permission, capability or `RLIMIT_MEMLOCK` check applies, so the call cannot fail with an errno a guest can observe. The only failure path, waiting for the map lock being interrupted by a fatal signal, ends the process before the return.

### Flags and options

- F1: every locked mapping is unlocked: ranges locked by `mlock`, by `mlock2` (including `MLOCK_ONFAULT`) and by `mlockall(MCL_CURRENT)`. `VmLck` drops to 0, and `smaps` loses `Locked:` and the `lo` mnemonic.
- F2: the `MCL_FUTURE` default (and `MCL_ONFAULT` with it) is cleared, so later `mmap`, `brk` and stack growth are no longer locked and no longer count against `RLIMIT_MEMLOCK`
- F3: with nothing locked, the call is a no-op that returns 0

## Determinism analysis

The return value is always 0 and the only state change is to the calling process's own lock bits, which are a function of the guest's earlier `mlock*` calls. Nothing depends on timing, another process or a host limit. Releasing locks can also undo the host-limit effect of an earlier `MCL_FUTURE` on later `mmap`/`brk` (see the `mlockall` issue), and the release itself repeats.

### Nondeterminism sources

- None in the call itself. When reclaim happens after unlocking is a host decision, but residency is hidden from the guest: `mincore` is emulated as all-resident (`detcore/src/syscall_classification.rs:130`-`132` at hermit f4055eb9cae4) and `Rss` in `smaps` is rewritten to 0 (`detcore/src/procfs.rs:2982`-`2983`).

## Hermit design

Target policy: `passthrough`. The code already does this.

- `munlockall` is classified `PassThrough` at `detcore/src/syscall_classification.rs:755`, in the pass-through block that starts at `detcore/src/syscall_classification.rs:690`.
  The comment above that group (`detcore/src/syscall_classification.rs:747`-`748`) says "Fixed credentials, process-local unlocks, and guest-owned filesystem flushes are repeatable under the fixed-container model". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it.
  The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins the class at `detcore/src/syscall_classification.rs:2061`. The census list holds it at `detcore/src/syscall_classification.rs:1730`.
- Interception charges the fixed `_ => FAST_NS` cost (250 ns) at `detcore/src/syscall_time.rs:156`, through `detcore/src/lib.rs:1936`. Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` the call does not reach Detcore.

Why passthrough, not `model` or `emulate`: Hermit keeps no record of which pages are locked, and the kernel is the only party that can clear the lock bits that `mlock`, `mlock2` and `mlockall` set. The call has no input that varies between runs. If the owner decides that `mlockall` should be emulated without host locking (open question on that issue), forwarding `munlockall` is still correct: unlocking memory that was never locked is F3, a no-op that returns 0.

Existing evidence: `tests/c/syscall_quick_wins.c:58` calls `require_zero(syscall(SYS_munlockall), "munlockall")` after a `munlock` of a page that was never locked (`tests/c/syscall_quick_wins.c:57`). It runs as cell `c-programs/syscall-quick-wins` (`tests/e2e/manifests/c-programs.yaml:9345`), whose `verify` mode has `ci: true` with ptrace enabled. That exercises F3 only. Nothing was locked, and the test does not observe lock state.

## Recommended tests

- T1 (covers F3): under `hermit run --verify` on ptrace, raw `syscall(SYS_munlockall)` with nothing locked returns 0.
  Existing coverage: `tests/c/syscall_quick_wins.c:58` in cell `c-programs/syscall-quick-wins` (`tests/e2e/manifests/c-programs.yaml:9345`, `verify` `ci: true`, ptrace enabled).
- T2 (covers F1): lock one anonymous page with `mlock`, a second region with `mlock2(..., MLOCK_ONFAULT)`, and call `mlockall(MCL_CURRENT)`. Then call `munlockall` and assert 0, and assert that no mapping in `/proc/self/smaps` still shows `lo` in `VmFlags` or a nonzero `Locked:`.
  `smaps` passes both fields through unchanged (`detcore/src/procfs.rs:3029`-`3044`). `VmLck` in `/proc/self/status` is always rewritten to `0 kB` (`detcore/src/procfs.rs:1437`-`1441`), so it cannot serve as the oracle. The test must record the host `RLIMIT_MEMLOCK` it needed.
- T3 (covers F2): call `mlockall(MCL_FUTURE)`, then `munlockall`, then `mmap` a new anonymous region. Assert that it lacks `lo`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/310#issuecomment-5872883786
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/310: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; munlockall records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/310 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (no arguments, no limit check, always 0; forwarded at detcore/src/lib.rs:2843); listed 3 behaviours and 3 tests; tests/c/syscall_quick_wins.c:58 exercises only F3 (nothing locked), so no coverage token is recorded and tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/syscall_quick_wins.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `munlockall`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+munlockall&body=Discussion+for+the+%60munlockall%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmunlockall.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60munlockall%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: munlockall" (https://github.com/rrnewton/hermit/issues/310), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/310.

Related: the `mlockall` tracking issue (https://github.com/rrnewton/hermit/issues/292) asks the owner whether `mlockall` should keep forwarding to the host. That decision does not change `munlockall`'s policy (see Hermit design).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=munlockall`, and by confirming hermit currently routes `munlockall` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
munlockall()                    = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/310#issuecomment-5872883786
