---
schema: syscall-pages/v1
syscall: fsync
nr_x86_64: 74
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/364
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsync&body=Discussion+for+the+%60fsync%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsync.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsync%60%2C+or+in+what+the+page+says%3F%0A
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

`fsync(fd)` writes the file's modified data and metadata back to stable storage and waits until the device reports completion. Hermit forwards it to the host kernel unchanged. The guest sees only `0` or an errno; the flush time is not guest-visible.

## Linux semantics

The VFS calls the filesystem's `fsync` operation for the whole file (`vfs_fsync_range` over the full range, with metadata). The kernel starts writeback of dirty pages, waits for it, writes the inode, and on most block filesystems issues a cache flush to the device. The call returns `0` or a negative errno and changes nothing a guest can read back within the run: file contents, size and offsets are the same before and after. Its only lasting effect is durability across a crash.

Error reporting uses the per-mapping writeback error sequence (`errseq`, since Linux 4.13). A writeback failure is recorded on the file's mapping, and each open file description that was open when it was recorded sees it once, at its next `fsync`. So whether an `fsync` reports `EIO` depends on whether a writeback failed since this description last checked, including writeback the kernel started on its own.

On local filesystems the wait is uninterruptible, so a signal does not cut it short.

### Flags and options

- F1: regular file opened for writing with dirty data: data and metadata are flushed and `0` is returned
- F2: regular file opened read-only: allowed, with the same result as F1. Linux does not require write access for `fsync`.
- F3: directory descriptor: flushes the directory's own metadata, which is what makes a newly created or renamed entry durable. The file's own `fsync` does not do this. Returns `0` on filesystems that implement it; tmpfs treats it as a no-op.
- F4: `fd` is not open, or is an `O_PATH` descriptor: `EBADF`
- F5: `fd` refers to an object with no sync operation: a pipe or FIFO, a socket, an eventfd, or a character device such as a terminal or `/dev/null`: `EINVAL`. POSIX also allows `EROFS` here.
- F6: a writeback error was recorded since this open file description last checked: `EIO`, reported once. A repeated `fsync` on the same description then returns `0`.
- F7: the filesystem needs space at writeback time and has none, for example NFS or a filesystem with delayed allocation: `ENOSPC` or `EDQUOT`

## Determinism analysis

`linux_determinism: conditional`. With a healthy local filesystem, the result is a function of the descriptor alone: its type (F1 to F5) and whether it is open. Neither the contents nor the timing of the flush reaches the guest. Under Hermit, threads run one at a time, so the order of this call against the guest's own writes, `close` and `dup` calls is fixed.

The condition is the host's storage. F6 and F7 depend on whether a writeback failed, and on when the kernel's background writeback ran relative to this call. That is host timing, not guest state. The call repeats exactly when the storage under the guest's files does not fail during the run.

### Nondeterminism sources

- Device or filesystem failure (F6), and running out of space or quota at writeback (F7). Whether the error lands on this call or on an earlier `write` or `close` depends on host writeback timing.
- Changes to the file made outside the container, which can dirty or fail pages Hermit does not control.
- The type of an inherited descriptor. `fsync` on fd 0, 1 or 2 returns `0` for a regular file and `EINVAL` for a pipe or a terminal (F5), so the result depends on what the launcher attached. This is a launch input, like `argv`, not a property of the call.
- Signal interruption, listed by the legacy issue, does not apply on local filesystems because the wait is uninterruptible. Hermit also delivers signals at deterministic points.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `fsync` is classified `PassThrough` at `detcore/src/syscall_classification.rs:751`, inside the pass-through block that begins at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:747` gives the reason: "guest-owned filesystem flushes are repeatable under the fixed-container model".
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` hands the call to the record/replay layer through `record_or_replay_preserving_tool_errors`, which keeps a host errno as the guest's result.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins `fsync` as `PassThrough` in the list at `detcore/src/syscall_classification.rs:2059`.

Why passthrough, not `model` or `emulate`:

- The call has no guest-visible output beyond its return value, and for a healthy filesystem that value is fixed by the descriptor type, which the kernel already knows.
- Emulating it, for example returning `0` without flushing, would change durability, which a guest that crashes or is killed mid-run can observe on the host. It would also hide real storage failures (F6, F7).
- Physical flush latency is wall-clock time. Hermit's guest time does not follow it, as the comment at `detcore/src/syscall_classification.rs:744` notes for the sibling `fdatasync`.

Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore never sees `fsync`. `hermit record` has its own subscriptions, in the recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record treats `fsync`.

The open gap is test evidence. Existing ptrace verify cells check F1 and F4 only; no test checks F2, F3 or F5, and F6 and F7 need fault injection.

## Recommended tests

- T1 (covers F1, F4): under `hermit run --verify` on ptrace, write to a fresh file and assert raw `syscall(SYS_fsync, fd)` returns `0`; assert `fsync(-1)` returns `EBADF`.
  Existing coverage: `tests/c/fsync_durability.c:47` and `tests/c/fsync_durability.c:62` (glibc `fsync`, which issues `SYS_fsync` on x86_64; cell `c-programs/fsync-durability`, `tests/e2e/manifests/c-programs.yaml:11947`, verify ci on ptrace) and raw `SYS_fsync` at `tests/c/syscall_quick_wins.c:72` (cell `c-programs/syscall-quick-wins`, `tests/e2e/manifests/c-programs.yaml:9345`).
- T2 (covers F2, F3): raw `SYS_fsync` on an `O_RDONLY` descriptor of a regular file and on an `O_RDONLY|O_DIRECTORY` descriptor of the guest's scratch directory each return `0`.
- T3 (covers F4, F5): assert `EBADF` for a closed descriptor and for an `O_PATH` descriptor. Assert `EINVAL` for both ends of a pipe, a socketpair end, an eventfd and `/dev/null`. The exact errno must be asserted, not only its stability across runs.
- T4 (covers none): with a fault-injecting block device (dm-error or dm-flakey) or a size-limited filesystem that allocates at writeback, assert that `EIO` is reported once and the next `fsync` on the same descriptor returns `0`, and that `ENOSPC` surfaces. A device or filesystem the test owns would make these errors guest-determined; the test does not cover F6 or F7, which are the host storage's own failures, recorded as U1.
- U1 (covers F6, F7; host-transient): `EIO` from a failed writeback and `ENOSPC` or `EDQUOT` at writeback depend on host storage health, free space and quota shared with other processes, and on host writeback timing. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F6 and F7 count toward done only once Hermit hides these errors or reports them as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/364: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (the code forwards at detcore/src/lib.rs:2843; the result is fixed by the descriptor type unless host storage fails); listed 7 flag behaviours and 4 tests; no coverage token is recorded, F2/F3/F5 have no test and F6/F7 need fault injection, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): F6 (EIO) and F7 (ENOSPC, EDQUOT) recorded as host-transient (U1); T4 now covers none, since a test-owned faulty device does not produce the host's own failures; the open question on unreachable writeback errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fsync`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fsync&body=Discussion+for+the+%60fsync%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffsync.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fsync%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fsync" (https://github.com/rrnewton/hermit/issues/364), legacy variant A2.

The legacy summary also said: "Rank 85 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls." Manual reference: Linux man-pages fsync(2). The legacy determinism text said the call "exposes only completion/errno; physical latency is not guest logical time, matching the batch-1 fdatasync policy"; that view is kept above.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fsync -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(74, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fsync(-1)                       = -1 EBADF (Bad file descriptor)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
