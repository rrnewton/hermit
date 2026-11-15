---
schema: syscall-pages/v1
syscall: msync
nr_x86_64: 26
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/308
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msync&body=Discussion+for+the+%60msync%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsync.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msync%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`msync(addr, length, flags)` flushes changes made through a shared file mapping back to the file, or schedules that flush. Hermit forwards it to the host kernel unchanged; its result depends only on the guest's own mappings and flags, and the physical flush is outside guest-visible state.

## Linux semantics

The kernel checks the flags and the alignment of `addr`, rounds `length` up to a page multiple, and walks the VMAs in the range. For `MS_SYNC` it writes back the dirty pages of each shared file-backed VMA and waits for completion, returning any writeback error recorded on the file. `MS_ASYNC` has been a no-op since Linux 2.6.19, because dirty pages are already tracked and written back by the kernel. `MS_INVALIDATE` asks that other mappings of the file see the new contents; on Linux the page cache is already coherent, so its only observable effect is the locked-page check. Private and anonymous mappings have nothing to write back. In every case the data the guest reads through `read`, `pread` or another mapping of the same file is the same before and after the call, because they share the page cache.

### Flags and options

- F1: `MS_SYNC`: write back the dirty pages of shared file mappings in the range and wait; afterwards the file contents, as seen through `pread`, match the mapping
- F2: `MS_ASYNC`: accepted and returns 0 without starting I/O (a no-op since 2.6.19)
- F3: `MS_INVALIDATE`: accepted; on a range containing `mlock`ed pages it gives `EBUSY`
- F4: `MS_SYNC` and `MS_ASYNC` together, or any unknown bit in `flags`: `EINVAL`
- F5: `addr` not page aligned: `EINVAL`
- F6: part of the range is not mapped: `ENOMEM` (the mapped parts are still flushed for `MS_SYNC`)
- F7: `length` is rounded up to a page multiple; `length == 0` succeeds
- F8: private (`MAP_PRIVATE`) and anonymous mappings, including `MAP_SHARED|MAP_ANONYMOUS`: succeed without I/O
- F9: a writeback error recorded on the backing file (for example `EIO` from a failing device, or `ENOSPC` on a filesystem that allocates at writeback) is reported by `MS_SYNC`; running out of space when a page is first dirtied raises `SIGBUS` at the store instead, not an `msync` error

## Determinism analysis

The return value is a function of the flags, the alignment, the guest's mapping layout and, for F9, the health of the backing filesystem. The guest-visible file contents do not depend on the call at all, since the page cache is shared, so the only output is the return value. Physical flush timing affects nothing the guest can observe. On a given layout the result repeats exactly, which is why `linux_determinism` stays deterministic; F9 is a host failure, like `ENOSPC` on any write, rather than a source of run-to-run variation on a healthy host.

### Nondeterminism sources

- Host filesystem failures behind F9.
- Changes to the backing file made outside the container.

## Hermit design

Target policy: `passthrough`. The code already does this. `msync` is classified `PassThrough` at `detcore/src/syscall_classification.rs:795`, in the pass-through block that starts at `detcore/src/syscall_classification.rs:690`, under the comment at `detcore/src/syscall_classification.rs:767`: "Stable guest-owned metadata and synchronous writeback operations are repeatable in Hermit's fixed mount namespace and filesystem image." The test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins it as PassThrough at `detcore/src/syscall_classification.rs:2079`, and it is in the census at `detcore/src/syscall_classification.rs:1728`. The blanket arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. It has no entry in the virtual cost table, so it takes the default at `detcore/src/syscall_time.rs:156`.

Why passthrough, not `model` or `emulate`:

- The state it acts on, the page cache and the shared mapping, belongs to the kernel. On ptrace the guest's mappings are real mappings, so only the kernel can write them back.
- Its result reads only guest-owned state, and the physical flush is outside guest logical time, so there is nothing to virtualize.

The ptrace cell `c-programs/msync-writeback` (`tests/e2e/manifests/c-programs.yaml:14596`) is the main evidence: it runs under `--verify` in CI on ptrace and liteinst with `bitwise_parity: true`. Its manifest disables KVM with the note that KVM's in-memory mapping model "does not flush MAP_SHARED writes to the backing file"; that is a KVM backend gap, outside this ptrace-scoped tracking issue.

Code facts show `subscribed_record_replay: false`: `msync` is not in the `--passthru-opt` list near `detcore/src/lib.rs:1214`. `hermit record` uses its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places outside the exported tree; the cell above has replay disabled ("never been run for this guest").

The open gap is test evidence for the remaining flags. `tests_naming` is 0: no test calls `SYS_msync` directly.

## Recommended tests

- T1 (covers F1, F5): under `hermit run --verify` on ptrace, write two patterns through a `MAP_SHARED` file mapping, call raw `syscall(SYS_msync, ..., MS_SYNC)` after each, and assert `pread` returns the pattern; assert `EINVAL` for an unaligned `addr`.
  Existing partial coverage via glibc `msync()`: `tests/c/msync_writeback.c:57`, `tests/c/msync_writeback.c:65` and `tests/c/msync_writeback.c:73` do exactly this (cell `c-programs/msync-writeback`, `tests/e2e/manifests/c-programs.yaml:14596`, ptrace verify in CI); the test only needs to issue the raw syscall to count as direct coverage.
- T2 (covers F2, F3, F4): assert `MS_ASYNC` and `MS_INVALIDATE` return 0 on a shared file mapping, `MS_SYNC|MS_ASYNC` and `flags = 0x100` give `EINVAL`, and `MS_INVALIDATE` on a range that has been `mlock`ed gives `EBUSY`.
- T3 (covers F6, F7): map three pages, unmap the middle one, and assert `MS_SYNC` over all three gives `ENOMEM`; assert `length == 0` and a `length` of one byte both succeed.
- T4 (covers F8): assert `MS_SYNC` succeeds on a `MAP_PRIVATE` file mapping (and the file is unchanged by writes through it), on a private anonymous mapping and on a `MAP_SHARED|MAP_ANONYMOUS` mapping.
  Existing partial coverage via glibc `msync()`: `tests/c/mmap_stress_determinism.c:95` (`MS_SYNC` on a shared anonymous mapping; cell `c-programs/mmap-stress-determinism`, `tests/e2e/manifests/c-programs.yaml:3834`, ptrace verify in CI).
- T5 (covers none): map a file on a device that fails writes (a `dm-error` or `dm-flakey` target, for example), dirty a page, and assert `MS_SYNC` reports `EIO`. Such a device is set up on the host, outside the container, so this test does not cover F9; U1 records F9.
- U1 (covers F9; host-transient): writeback errors (`EIO` from failing storage, `ENOSPC` on a filesystem that allocates at writeback) depend on host storage health and free space shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F9 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/308#issuecomment-5872879564
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/308: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; msync records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/308 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (classified PassThrough at detcore/src/syscall_classification.rs:795, forwarded at detcore/src/lib.rs:2843; the write-back state is the kernel's); listed 9 flag behaviours and 5 tests; c-programs/msync-writeback covers F1 and F5 through glibc, but no coverage token is recorded because no test calls SYS_msync directly, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant stays false; F9 writeback errors recorded as host-transient (U1); T5 no longer claims F9; the open question on the unreachable writeback error is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `msync`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+msync&body=Discussion+for+the+%60msync%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmsync.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60msync%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: msync" (https://github.com/rrnewton/hermit/issues/308), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/308.

### Open questions

- Whether the existing glibc-based cell `c-programs/msync-writeback` may count as coverage for F1 and F5 (glibc's `msync()` issues `SYS_msync` directly), or a raw-syscall test is required as for the other bodies.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=msync`, and by confirming hermit currently routes `msync` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
msync(0x406000, 4096, MS_ASYNC) = 0
```

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/308#issuecomment-5872879564
