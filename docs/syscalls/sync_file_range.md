---
schema: syscall-pages/v1
syscall: sync_file_range
nr_x86_64: 277
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/492
  - https://github.com/rrnewton/hermit/issues/513
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sync_file_range&body=Discussion+for+the+%60sync_file_range%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsync_file_range.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sync_file_range%60%2C+or+in+what+the+page+says%3F%0A
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

`sync_file_range(fd, offset, nbytes, flags)` starts writeback of the dirty page-cache pages in a byte range of an open file, waits for it, or both. Hermit forwards it to the host kernel unchanged. The guest sees only the return value, and that value depends on the arguments and the descriptor type, not on timing.

## Linux semantics

On x86_64 the argument order is `(fd, offset, nbytes, flags)`. The variant with `flags` second, `sync_file_range2`, exists only on some other architectures. The kernel resolves `fd` first, so a bad descriptor gives `EBADF` before any other check. It then validates the flags and the range, and accepts only regular files, block devices, directories and symlinks. The descriptor does not need to be open for writing.

The call never changes file data, and it does not flush metadata, so it gives no durability guarantee on its own. Once a wait flag is set, the call can block for as long as the storage takes. A wait-after step returns any writeback error recorded on the file's mapping. The call returns 0 or a negative errno and writes no memory in the caller.

### Flags and options

- F1: `flags == 0`: nothing is done; once fd and range validate, the call returns 0
- F2: `SYNC_FILE_RANGE_WRITE` (2) alone: start asynchronous writeback of the dirty pages in the range that are not already under writeback. It can still block briefly when the device queue is full.
- F3: `SYNC_FILE_RANGE_WAIT_BEFORE` (1): first wait for writeback already in progress on pages in the range
- F4: `SYNC_FILE_RANGE_WAIT_AFTER` (4): after the write step, wait for writeback of the range to finish and report writeback errors
- F5: `WAIT_BEFORE|WRITE|WAIT_AFTER` (7, named `SYNC_FILE_RANGE_WRITE_AND_WAIT` in newer uapi headers): a data-integrity flush of the range, like `fdatasync` limited to the range and without metadata. The other combinations (3, 5, 6) do the listed steps in order.
- F6: range: `nbytes == 0` means from `offset` to the end of the file. A range wholly past EOF succeeds and writes nothing.
- F7: invalid arguments: unknown flag bits, a negative `offset`, a negative `nbytes`, or `offset + nbytes` overflowing gives `EINVAL`
- F8: descriptor: a bad fd, a closed fd or an `O_PATH` fd gives `EBADF`, checked before F7. A pipe, FIFO or socket gives `ESPIPE`. An `O_RDONLY` fd on a regular file is accepted.
- F9: host-dependent blocking time under F3 to F5. F10 holds the host-dependent errors.
- F10: host-transient failures: `EIO` (a writeback error), `ENOSPC` (writeback that runs out of space, for example on a delayed-allocation filesystem) and `ENOMEM` from the host

## Determinism analysis

The return value is a function of the arguments, the descriptor's type and open state, and the storage's health. None of those depend on timing or on another guest thread. The call repeats whenever the guest opens the same kind of file and the host storage does not fail.

### Nondeterminism sources

- Writeback errors (F10). They come from the host storage, not from guest state, and Hermit does not try to make a failing disk deterministic.
- Blocking time under the wait flags. The guest does not see it in virtual time: Detcore charges every syscall a fixed cost (`detcore/src/lib.rs:1936`). `sync_file_range` has no entry in the cost table, so it gets the fallback at `detcore/src/syscall_time.rs:156`, however long the host waits.
- When pages actually reach the disk. The guest cannot observe this except through a crash, which is outside Hermit's model.

## Hermit design

Target policy: `passthrough`. The code already does this. `sync_file_range` is classified `PassThrough` at `detcore/src/syscall_classification.rs:801`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. Its group comment at `detcore/src/syscall_classification.rs:767` calls these "synchronous writeback operations" that "are repeatable in Hermit's fixed mount namespace and filesystem image". The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the classification at `detcore/src/syscall_classification.rs:2088`.

Why passthrough, not `emulate`:

- The call has no data effect, and Hermit has no page-cache model, so there is nothing for a model to keep.
- The only guest-visible outputs are the errors in F7 and F8, and the kernel computes them from the guest's own arguments and descriptors. An emulation that returned 0 would have to reimplement those checks to stay faithful, for no determinism gain.
- Real writeback on the host is harmless to the guest. Its cost appears in wall time only.

Code facts show `subscribed_record_replay: false`, so Detcore's record/replay subscription does not intercept the call.

The gap is test evidence. `tests/c/sync_file_range.c` exercises F2, F5 and the `EBADF` case of F8 through glibc's `sync_file_range()` wrapper. `tests/c/syscall_file_metadata.c:220` is the only test that issues raw `SYS_sync_file_range`, and only with `SYNC_FILE_RANGE_WRITE`. No test exercises F1, F3, F4 or F7, or the `ESPIPE` and `O_PATH` cases of F8.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F5, F6): under `hermit run --verify` on ptrace, write 8 KiB to a fresh `mkstemp` file. Then call raw `syscall(SYS_sync_file_range, ...)` with each `flags` value from 0 to 7, over the ranges `(0, 0)`, `(0, 4096)`, `(4096, 4096)` and `(1 << 20, 4096)` (past EOF). Assert that every call returns 0 and that the file contents are unchanged.
  Existing partial coverage: `tests/c/sync_file_range.c:68` (`WRITE` over the whole file), `tests/c/sync_file_range.c:77` (all three flags on the first page) and `tests/c/sync_file_range.c:91` (`WRITE` on the second page), through the glibc wrapper (cell `c-programs/sync-file-range`, `tests/e2e/manifests/c-programs.yaml:13971`, verify ci on ptrace). Also raw `WRITE` at `tests/c/syscall_file_metadata.c:220` (cell `c-programs/syscall-file-metadata`, `tests/e2e/manifests/c-programs.yaml:9275`, and the cargo test `deterministic_file_metadata_syscalls_verify` at `hermit-cli/tests/syscall_file_metadata.rs:51`, which passes `--backend=ptrace`).
- T2 (covers F7): assert `EINVAL` for `flags = 8`, for `flags = 0xffffffff`, for `offset = -1`, for `nbytes = -1`, and for `offset = LLONG_MAX` with `nbytes = 2`.
- T3 (covers F8): assert `EBADF` for fd `-1`, for a closed fd and for an `O_PATH` fd, the last also with `flags = 8` to show that `EBADF` wins over `EINVAL`. Assert `ESPIPE` on a pipe and on a socketpair, and success on an `O_RDONLY` descriptor.
  Existing partial coverage: `tests/c/sync_file_range.c:100` (fd `-1`).
- T4 (covers F9): run a full `WAIT_BEFORE|WRITE|WAIT_AFTER` flush over a 64 MiB file and assert 0, so a long host wait is exercised under `--verify`. `EIO`, `ENOSPC` and host `ENOMEM` are F10, recorded by U1.
- U1 (covers F10; host-transient): writeback `EIO` and `ENOSPC`, and `ENOMEM`, depend on host storage health, free space and memory shared with other processes. The plain passthrough (`detcore/src/syscall_classification.rs:801`, `detcore/src/lib.rs:2843`) hands them to the guest today. F10 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/492: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/513 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (forwarded at detcore/src/lib.rs:2843; no data effect, errors depend only on arguments and descriptor type); merged duplicate https://github.com/rrnewton/hermit/issues/513, whose CONDITIONAL rating lost because its sources are host storage failures and latency, which virtual time does not see; 9 flag behaviours and 4 tests listed; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant stays false (blocking time is hidden by the fixed syscall cost, and writeback errors are transient); F9 narrowed to blocking time, new F10 EIO/ENOSPC/ENOMEM (host-transient U1); the open question on unreachable storage errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sync_file_range`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sync_file_range&body=Discussion+for+the+%60sync_file_range%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsync_file_range.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sync_file_range%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sync_file_range" (https://github.com/rrnewton/hermit/issues/492), legacy variant A1. Manual reference: [sync_file_range(2)](https://man7.org/linux/man-pages/man2/sync_file_range.2.html).

Duplicate https://github.com/rrnewton/hermit/issues/513 (filed 2026-07-24 as "Syscall classification: sync_file_range", legacy variant A1) is merged into this body. Its comment history (0 comments) stays on that issue, and this issue's comment history stays on https://github.com/rrnewton/hermit/issues/492.

### Merge reconciliation

- Classification: https://github.com/rrnewton/hermit/issues/492 said deterministic and https://github.com/rrnewton/hermit/issues/513 said CONDITIONAL. Deterministic won. The duplicate's sources are dirty-page state, writeback congestion, filesystem support, storage errors and signals. Dirty-page state and congestion change only latency, which Hermit's virtual time does not observe. Filesystem support is not a factor: every filesystem goes through the same generic range-writeback path, and only the descriptor type matters (F8). Storage errors are host failures (F9), which this set treats as nondeterminism sources without changing the rating; see mkdir. The duplicate also lists signals, but the kernel does not interrupt the writeback wait with `EINTR`, and none of its evidence shows such a return.
- Both issues recommended PASS-THRU, and the code agrees.
- Both noted that the call gives no general durability guarantee, because metadata may stay unsynced. That point is kept in Linux semantics.

### Open questions

- `rr_sync_file_range` at `hermit-cli/tests/rr_suite.rs:512` runs rr's own `sync_file_range` program, but the `rr_test!` macro marks it `#[ignore]` (`hermit-cli/tests/rr_suite.rs:233`), so it cannot be cited as CI coverage.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/492)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sync_file_range` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `sync_file_range` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
sync_file_range(4, 0, 0, 0)     = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/513)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=sync_file_range`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `sync_file_range` in the UNCLASSIFIED temporary-passthrough block. A one-byte temporary-file range completed successfully.

Observed strace line:
```text
sync_file_range(3, 0, 1, SYNC_FILE_RANGE_WAIT_BEFORE|SYNC_FILE_RANGE_WRITE|SYNC_FILE_RANGE_WAIT_AFTER) = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
