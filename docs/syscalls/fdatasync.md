---
schema: syscall-pages/v1
syscall: fdatasync
nr_x86_64: 75
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/1944
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fdatasync&body=Discussion+for+the+%60fdatasync%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffdatasync.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fdatasync%60%2C+or+in+what+the+page+says%3F%0A
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

`fdatasync(fd)` writes the file's modified data, and only the metadata needed to read that data back, to stable storage and waits for completion. Hermit forwards it to the host kernel unchanged. The guest sees only `0` or an errno; the flush time is not guest-visible.

## Linux semantics

`fdatasync` is `fsync` with the `datasync` flag set: the VFS calls the filesystem's `fsync` operation over the whole file (`vfs_fsync_range` with `datasync = 1`). Data pages are written back and waited for. Inode metadata is written only if it is needed to retrieve the data, such as a changed file size or newly allocated blocks; a change to only the timestamps is not flushed. The call returns `0` or a negative errno and changes nothing a guest can read back within the run.

Error reporting is the same as `fsync`: a writeback failure is recorded on the file's mapping (`errseq`, since Linux 4.13), and each open file description that was open when it was recorded sees it once, at its next `fsync` or `fdatasync`. Descriptors made by `dup` share one open file description, so they share that one report. On local filesystems the wait is uninterruptible.

### Flags and options

- F1: regular file opened for writing with dirty data: the data, and any size or allocation metadata it needs, are flushed and `0` is returned
- F2: only metadata that is not needed to read the data has changed, such as `mtime` after an in-place overwrite that is already written back: Linux may skip the inode write, and `0` is returned
- F3: regular file opened read-only, or a directory descriptor: allowed, and returns `0` on filesystems that implement it. Linux does not require write access.
- F4: `fd` is not open, or is an `O_PATH` descriptor: `EBADF`
- F5: `fd` refers to an object with no sync operation: a pipe or FIFO, a socket, an eventfd, or a character device such as a terminal or `/dev/null`: `EINVAL`. POSIX also allows `EROFS` here.
- F6: a writeback error was recorded since this open file description last checked: `EIO`, reported once per open file description, including through its `dup` copies. A repeated call then returns `0`.
- F7: the filesystem needs space at writeback time and has none, for example NFS or a filesystem with delayed allocation: `ENOSPC` or `EDQUOT`

## Determinism analysis

`linux_determinism: conditional`. With healthy local storage, the result is a function of the descriptor alone: its type and whether it is open (F1 to F5). F2 changes only what reaches the disk, not what the guest sees. Under Hermit, threads run one at a time, so the order of this call against the guest's own `write`, `ftruncate`, `dup` and `close` calls is fixed.

The condition is the host's storage. F6 and F7 depend on whether a writeback failed, and on when the kernel's background writeback ran relative to this call, which is host timing. The call repeats exactly when the storage under the guest's files does not fail during the run.

The 2026-08-07 analysis (kept under Additional information) said that "a host completion result is not a deterministic guest durability event". That is true only for F6 and F7, which are host failures; for a healthy filesystem the completion result is `0` every time.

### Nondeterminism sources

- Device or filesystem failure (F6), and running out of space or quota at writeback (F7). Which call reports the error depends on host writeback timing.
- Changes to the file made outside the container.
- The type of an inherited descriptor: fd 0, 1 or 2 gives `0` for a regular file and `EINVAL` for a pipe or a terminal (F5). This is a launch input, not a property of the call.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `fdatasync` is classified `PassThrough` at `detcore/src/syscall_classification.rs:745`, inside the pass-through block that begins at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:743` gives the reason: "fdatasync/ftruncate have deterministic results for stable guest-owned files; physical flush latency is outside guest logical time."
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` hands the call to the record/replay layer through `record_or_replay_preserving_tool_errors`, which keeps a host errno as the guest's result.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins `fdatasync` as `PassThrough` in the list at `detcore/src/syscall_classification.rs:2052`.

The 2026-08-07 strategy draft proposed "a deterministic logical durability barrier for modeled files", with host flushing as a hidden implementation detail. This body rejects that draft, and target_policy moves from `investigate` to `passthrough`:

- **Hermit has no file model.** The kernel's page cache is the file state, and there is no "modeled file" for a logical barrier to apply to. Building one is a filesystem model, far beyond this syscall.
- **The guest-visible part is already deterministic.** For a healthy filesystem the result is fixed by the descriptor type. What the draft wanted to hide, flush latency, is wall-clock time, and Hermit's guest time does not follow it.
- **Hiding failures would be wrong.** The draft's alternative, "record and validate externally sourced failure outcomes", is what `hermit record` exists for. In run mode, a real `EIO` (F6) is a host failure the guest should see, not a value to rewrite.

Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore never sees `fdatasync`. `hermit record` has its own subscriptions, in the recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record treats `fdatasync`.

The open gap is test evidence. `tests_naming` is 0: no test names `SYS_fdatasync`. One ptrace verify cell calls glibc `fdatasync()`, which issues `SYS_fdatasync` on x86_64, and checks F1 and F4.

## Recommended tests

- T1 (covers F1, F4): under `hermit run --verify` on ptrace, write to a fresh file and assert raw `syscall(SYS_fdatasync, fd)` returns `0`; assert it returns `EBADF` for fd `-1`.
  Existing coverage through glibc `fdatasync()`: `tests/c/fsync_durability.c:52` and `tests/c/fsync_durability.c:67` (cell `c-programs/fsync-durability`, `tests/e2e/manifests/c-programs.yaml:11947`, verify ci on ptrace).
- T2 (covers F2, F3): overwrite bytes in place without changing the size, call `fdatasync` twice, and assert `0` both times. Also assert `0` for an `O_RDONLY` regular-file descriptor and an `O_DIRECTORY` descriptor.
- T3 (covers F4, F5): assert `EBADF` for an `O_PATH` descriptor, and `EINVAL` for both ends of a pipe, a socketpair end, an eventfd and `/dev/null`. Assert the exact errno, not only its stability across runs.
- T4 (covers none): with a fault-injecting block device (dm-error or dm-flakey) or a filesystem that allocates at writeback, assert that `EIO` is reported once per open file description, through either of two `dup` copies, and that the next call returns `0`; and that `ENOSPC` surfaces. A device or filesystem the test owns would make these errors guest-determined; the test does not cover F6 or F7, which are the host storage's own failures, recorded as U1.
- T5 (covers F1, F6): two guest threads interleave `write`, `ftruncate`, `dup` and `close` on one file around `fdatasync` calls. Assert that every return value and the final file contents are identical across `--verify` runs.
- T6 (covers F1, F4): record and replay the T1 sequence with `hermit record` on ptrace and assert the same return values on replay.
- T7 (covers F1): read `CLOCK_MONOTONIC` before and after `fdatasync` of a file with several megabytes of dirty data. Assert that the delta is identical across `--verify` runs, so that host flush latency cannot reach guest time or scheduling.
- U1 (covers F6, F7; host-transient): `EIO` from a failed writeback and `ENOSPC` or `EDQUOT` at writeback depend on host storage health, free space and quota shared with other processes, and on host writeback timing. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F6 and F7 count toward done only once Hermit hides these errors or reports them as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. fdatasync is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1944: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional (fixed by the descriptor type unless host storage fails); target_policy investigate -> passthrough (Hermit has no file model for the 2026-08-07 logical-barrier draft, and flush latency is not guest time); 7 flag behaviours and 7 tests; no coverage token is recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): F6 (EIO) and F7 (ENOSPC, EDQUOT) recorded as host-transient (U1); T4 now covers none, since a test-owned faulty device does not produce the host's own failures; the open question on unreachable writeback errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fdatasync`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fdatasync&body=Discussion+for+the+%60fdatasync%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffdatasync.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fdatasync%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: fdatasync" (https://github.com/rrnewton/hermit/issues/1944), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy strategy draft (2026-08-07), rejected above

Determinism analysis: fdatasync exposes host writeback timing, filesystem and device errors, descriptor type, and storage state. A host completion result is not a deterministic guest durability event.

Hermit design: Define a deterministic logical durability barrier for modeled files, with host flushing performed as an implementation detail whose incidental timing is hidden. Surface only modeled errors, or record and validate externally sourced failure outcomes.

Legacy required tests (2026-08-07), now T1 to T7:

- Cover regular files, read-only and invalid fds, pipes/directories as applicable, and injected writeback errors. (T1 to T4)
- Interleave writes, truncation, duplication, and close around the barrier under scheduler control. (T5)
- Verify stable return values and errno across strict repeats and record/replay. (every T runs under `--verify`; T6 for record/replay)
- Prove a delayed host flush cannot alter guest scheduling or virtual time. (T7)

Legacy acceptance text (2026-08-07): Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

fdatasync is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
