---
schema: syscall-pages/v1
syscall: sync
nr_x86_64: 162
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/491
  - https://github.com/rrnewton/hermit/issues/512
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sync&body=Discussion+for+the+%60sync%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsync.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sync%60%2C+or+in+what+the+page+says%3F%0A
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

`sync()` asks the kernel to write every filesystem's dirty data and metadata to storage, and waits for that writeback. It takes no arguments and always returns 0. Hermit forwards it to the host kernel unchanged. The guest sees only the return value, which never varies.

## Linux semantics

`sync` starts writeback on every mounted filesystem, waits for it to complete, then flushes block devices. Linux waits for the I/O to finish. POSIX only requires that writeback be scheduled, and historical implementations differed. The call is declared `void` in the C library, and the raw system call returns 0. It reports no error: since Linux 5.8, writeback errors are reported by `syncfs`, `fsync` and `fdatasync`, never by `sync`. The call affects state outside the calling process (the host page cache and storage) but does not change anything the guest can read back through file contents, metadata or descriptors.

### Flags and options

_None: the syscall takes no flags or options._

## Determinism analysis

The guest-visible result is always 0, with no output buffer and no errno. How long writeback takes and how much data reaches storage depend on host state, but neither is a guest-visible result under Hermit: time inside the call is not visible, because the logical clock advances by a fixed cost per syscall (see Hermit design), and durability matters only after a host crash, which is outside a run.

Reconciliation with the duplicate https://github.com/rrnewton/hermit/issues/512: that issue classified `sync` as CONDITIONAL, on the grounds that completion cost and eventual durability depend on the host. This body keeps `deterministic`, the classification of https://github.com/rrnewton/hermit/issues/491. Both issues agree that the call has no data result. The conditions the duplicate names, wall time and external durability, are not results a guest can observe under Hermit. Both issues recommend PASS-THRU, so the reconciliation changes no policy.

### Nondeterminism sources

- None guest-visible. Dirty host pages, device errors, the filesystem implementation and storage latency decide how long the call blocks on the host and what eventually reaches storage. Hermit charges a fixed logical cost instead of wall time, and the call reports no device error to its caller.

## Hermit design

Target policy: `passthrough`. The code already does this.

- `sync` is classified `PassThrough` at `detcore/src/syscall_classification.rs:827` (hermit f4055eb9cae4), in the issue-reviewed pass-through block at `detcore/src/syscall_classification.rs:807`. That block's comment (`detcore/src/syscall_classification.rs:808`-`809`) says these calls stay conditional on Hermit's fixed-container and stable-state assumptions.
  The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins the class at `detcore/src/syscall_classification.rs:2116`. The census list holds it at `detcore/src/syscall_classification.rs:1751`.
- Logical time: `detcore/src/lib.rs:1936` charges `syscall_time::cost_ns`, and `sync` falls to `_ => FAST_NS` (250 ns, `detcore/src/syscall_time.rs:13` and `156`). It is not in the `SYNC_NS` group, which holds blocking waits such as `nanosleep` and `poll` (`detcore/src/syscall_time.rs:68`-`76`). However long writeback takes on the host, the guest's clock moves by the same 250 ns. Code facts show `subscribed_record_replay: false`.

Why passthrough, not `emulate` as a no-op: forwarding keeps the guest's durability request honest for programs whose output outlives the run. A no-op would return the same 0 but silently drop that request. The cost is host wall time while writeback runs, which affects only how long a run takes. If that cost ever matters, for example for guests that call `sync` in a loop, a no-op emulation would be indistinguishable to the guest. That is a performance choice, not a determinism one.

## Recommended tests

- T1 (covers none): under `hermit run --verify` on ptrace, a guest writes a file, calls raw `syscall(SYS_sync)` and asserts a return of 0. It then reads the file back and asserts its contents are unchanged. Both runs must print identical output. The test records that the call exercises the forwarded path and that the guest's logical clock (`clock_gettime(CLOCK_MONOTONIC)` before and after) moves by the same amount in each run.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/491: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/512 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (always returns 0; forwarded at detcore/src/lib.rs:2843, charged a fixed FAST_NS at detcore/src/syscall_time.rs:156); merged duplicate https://github.com/rrnewton/hermit/issues/512 section by section, keeping deterministic over its CONDITIONAL because wall time and durability are not guest-visible; flags sentinel and 1 test; no test calls SYS_sync, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sync`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sync&body=Discussion+for+the+%60sync%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsync.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sync%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sync" (https://github.com/rrnewton/hermit/issues/491), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sync` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `sync` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
sync()                          = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Merged duplicate https://github.com/rrnewton/hermit/issues/512

Duplicate https://github.com/rrnewton/hermit/issues/512 was filed 2026-07-24 as "Syscall classification: sync" (legacy variant A1). Its comment history (0 comments) stays on that issue. Its summary, manual reference (https://man7.org/linux/man-pages/man2/sync.2.html), nondeterminism sources and kernel note (Linux waits for I/O completion; historical implementations differed) are merged into the sections above. Its CONDITIONAL classification is reconciled under Determinism analysis. Its host evidence is kept here:

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=sync`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `sync` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
sync()                          = 0
```

*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
