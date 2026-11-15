---
schema: syscall-pages/v1
syscall: ftruncate
nr_x86_64: 77
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1945
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ftruncate&body=Discussion+for+the+%60ftruncate%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fftruncate.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ftruncate%60%2C+or+in+what+the+page+says%3F%0A
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

`ftruncate(fd, length)` sets the size of the regular file open on `fd` to `length` bytes, discarding data past the new end or extending the file with zero bytes. Hermit forwards it to the host kernel unchanged. It relies on the guest's files being private and stable, and Detcore keeps no file state of its own.

## Linux semantics

The kernel looks up `fd`, checks that it is open for writing and refers to a regular file, refuses an append-only inode, and calls `do_truncate` with the open file. Unlike `truncate`, no path is resolved and no lease is broken: opening the file for writing already did that. Shrinking discards the bytes past `length`; pages of a shared mapping that now lie wholly past the end raise `SIGBUS` when touched. Extending makes the new range read as zero bytes, as a hole on filesystems that support sparse files. This is also how POSIX shared memory (`shm_open`) and `memfd_create` objects are given a size before they are mapped.

The call returns `0` or a negative errno. It does not change the descriptor's file offset, and it does not report the new size, timestamps or block count; a guest sees those only through the stat family and later reads.

### Flags and options

- F1: `length` smaller than the current size: data past `length` is discarded. Touching a shared mapping past the new end raises `SIGBUS`.
- F2: `length` larger than the current size: the file is extended, and the new range reads as zero bytes. This includes sizing a fresh `memfd_create` or `shm_open` object.
- F3: `length` equal to the current size: the data is unchanged and `0` is returned
- F4: the file offset of the open file description is not changed. After a shrink it can lie past the end: a `read` there returns `0` and a `write` there leaves a hole. A descriptor opened with `O_APPEND` may still be truncated; only an append-only inode refuses (F10).
- F5: `fd` is not open, or is an `O_PATH` descriptor: `EBADF`
- F6: `fd` is not open for writing, or does not refer to a regular file (a pipe or FIFO, a socket, a directory, a terminal or other device): `EINVAL`
- F7: `length` is negative: `EINVAL`. On hugetlbfs, `length` that is not a multiple of the huge page size: `EINVAL`.
- F8: growing past the caller's `RLIMIT_FSIZE`: `SIGXFSZ` is sent to the caller and the call returns `EFBIG`
- F9: memfd seals: with `F_SEAL_SHRINK` set, a smaller `length` gives `EPERM`; with `F_SEAL_GROW` set, a larger `length` gives `EPERM`. An equal `length` is allowed under both.
- F10: host-config failure: `EPERM` because the inode is append-only or immutable. Setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest lacks, so only files the host flagged give it.
- F11: side effects the call does not return: `mtime` and `ctime` are updated, even when the size is unchanged. Unless the caller has `CAP_FSETID` over the file, `S_ISUID` is cleared, `S_ISGID` is cleared when the group-execute bit is set, and a `security.capability` attribute is removed.
- F12: `fd` is an inherited descriptor such as fd 0, 1 or 2: the result depends on what the launcher attached. A regular file is truncated on the host; a pipe or a terminal gives `EINVAL` (F6).
- F13: host-transient failure: `EIO` from failing host storage while the inode is updated.
- F14: host-config failure: `length` larger than the maximum file size of the host filesystem holding the file gives `EFBIG`. The limit depends on the filesystem type (ext4 with 4 KiB blocks stops at 16 TiB, while tmpfs and XFS accept lengths up to `LLONG_MAX`), so the same `length` gives `EFBIG` on one host and succeeds on another.

## Determinism analysis

`linux_determinism: deterministic`. The result is a function of the open file description (its type, access mode and offset), the file's size and seals, the caller's credentials and resource limits, and the inode's attributes. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so two guest threads that truncate, read, write or map the same file through different or duplicated descriptors get a fixed order. The call repeats exactly when the guest's files start in the same state and nothing outside the container changes them. This matches the truncate tracking issue, which counts filesystem state as an input.

The migrated body carried `conditional` only as a placeholder, because the 2026-08-07 filing stated no class. Its determinism text listed "host filesystem limits, seals, descriptor permissions, sparse-file behavior, and concurrent I/O ordering". Seals, permissions and limits are guest-owned inputs (F6 to F9), sparse-file behaviour is fixed for a given filesystem, and concurrent I/O is ordered by Hermit's scheduler. The remaining external dependencies are listed below.

### Nondeterminism sources

- Changes to the file made outside the container, including writers that are not guest processes. Hermit does not try to make a changing host filesystem deterministic.
- Host attributes and failures: append-only or immutable flags set by the host (F10), and I/O errors (F13).
- The type of an inherited descriptor (F12). This is a launch input, like `argv`, not a property of the call.
- Values the call changes but does not return: the timestamps and block count in F11. They become visible only through `stat` and similar calls, and are handled there.

### Cross-host variation

- Filesystem type behind the file: its maximum file size decides which `length` values give `EFBIG` (F14).
- Inode flags on host files the guest sees: an append-only or immutable flag set on the host gives `EPERM` (F10) on that host and not on another.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `ftruncate` is classified `PassThrough` at `detcore/src/syscall_classification.rs:746`, inside the pass-through block that begins at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:743` gives the reason: "fdatasync/ftruncate have deterministic results for stable guest-owned files".
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` hands the call to the record/replay layer through `record_or_replay_preserving_tool_errors`, which keeps a host errno as the guest's result.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins `ftruncate` as `PassThrough` in the list at `detcore/src/syscall_classification.rs:2058`.

The 2026-08-07 strategy draft proposed applying size changes "through deterministic file-object state with scheduler-serialized ordering" and modelling zero-fill, offsets, seals, limits and error precedence. This body rejects that draft, and target_policy moves from `investigate` to `passthrough`:

- **Hermit has no file model.** The file's contents and size live in the kernel, and every later `read`, `mmap` and `stat` sees them there. A Hermit-side model of zero-fill, seals and error order would have to agree with the kernel on every one of those calls; only the kernel can apply F1 to F11 the way they see them.
- **The ordering half already exists.** Hermit's scheduler serializes guest threads, which is the "scheduler-serialized ordering" the draft asked for.
- **The inputs are already fixed.** Seals come from `fcntl`, limits from `setrlimit`, and the file itself from the container's private filesystem. None of them varies between runs unless something outside the container changes it.

Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore never sees `ftruncate`. `hermit record` has its own subscriptions, in the recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree. One record test shows how record treats F12: `replay_captured_output_ftruncate_failure_aborts_without_panicking` (`hermit-cli/tests/record_replay.rs:1167`) records a guest that truncates its own stdout (`tests/c/ftruncate_ignore_output_error.c:17`), checks that replay truncates a compatible captured stdout, and checks that replay into a sink that rejects the truncate ends as one Hermit tool error rather than a panic.

The open gap is test evidence. `tests_naming` is 0: no test names `SYS_ftruncate`. Four ptrace verify cells call glibc `ftruncate()`, which issues `SYS_ftruncate` on x86_64, and cover F1 and F2.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, call raw `syscall(SYS_ftruncate, ...)` on a file holding `ABCD` with `length` 16, 2, 6 and 6 again. Assert `st_size` after each call, that the retained prefix is intact, and that every extended range reads as zero bytes.
  Existing coverage through glibc `ftruncate()`: grow, shrink and regrow at `tests/c/ftruncate_sparse.c:97`, `tests/c/ftruncate_sparse.c:109` and `tests/c/ftruncate_sparse.c:119`, with size and zero-fill checks (cell `c-programs/ftruncate-sparse`, `tests/e2e/manifests/c-programs.yaml:15261`, verify ci on ptrace); memfd growth with a zero-filled tail at `tests/c/memfd_create.c:68` (cell `c-programs/memfd-create`, `tests/e2e/manifests/c-programs.yaml:12387`); a shrink to half at `tests/c/file_io_roundtrip.c:120` (cell `c-programs/file-io-roundtrip`, `tests/e2e/manifests/c-programs.yaml:17236`); and a grow at `tests/c/syscall_file_metadata.c:123` (cell `c-programs/syscall-file-metadata`, `tests/e2e/manifests/c-programs.yaml:9275`).
  No test covers F3.
- T2 (covers F1): map a file `MAP_SHARED`, shrink it with `ftruncate`, and touch a page past the new end inside a `SIGBUS` handler that `siglongjmp`s out. Assert that the handler runs exactly once and that the run is identical across `--verify` runs.
- T3 (covers F4): write 8 bytes, shrink to 2, and assert that `lseek(fd, 0, SEEK_CUR)` is still 8, that `read` returns `0`, and that a following `write` leaves bytes 2 to 7 reading as zero. Repeat through a `dup` copy and through an `O_APPEND` descriptor, which must truncate successfully.
- T4 (covers F5, F6): assert `EBADF` for fd `-1`, a closed descriptor and an `O_PATH` descriptor. Assert `EINVAL` for an `O_RDONLY` regular-file descriptor, both ends of a pipe, a socketpair end, an `O_DIRECTORY` descriptor and `/dev/null`. Assert the exact errno.
  Existing tests check only that the result is stable across runs: `tests/c/errno_path_identity.c:108` (fd `-1`) and `tests/c/errno_path_identity.c:135` (a pipe).
- T5 (covers F7, F8): assert `EINVAL` for `length = -1`. Do not assert a result for `length = LLONG_MAX`: it gives `EFBIG` or succeeds depending on the host filesystem (F14, recorded as U3). Lower `RLIMIT_FSIZE` to 4096 with `SIGXFSZ` caught, grow a file to 8192, and assert `EFBIG` plus exactly one `SIGXFSZ`. If a hugetlbfs mount is visible, assert `EINVAL` for an unaligned `length`, and record in the test whether ptrace CI has one.
- T6 (covers F9): create a memfd with `MFD_ALLOW_SEALING`. Add `F_SEAL_SHRINK` and assert that shrinking gives `EPERM` while growing and an equal `length` give `0`. On a second memfd, add `F_SEAL_GROW` and assert the reverse.
- T7 (covers none): append-only and immutable flags need `CAP_LINUX_IMMUTABLE` in the initial namespace, and `EIO` needs fault injection, so this test covers neither; F10 is recorded as U1 and F13 as U2.
- T8 (covers F11): create a file with mode `04755`, drop `CAP_FSETID` with `capset`, `ftruncate` it to its current length, and assert that `S_ISUID` is gone. Repeat with `CAP_FSETID` kept and assert that it stays. The guest runs as root in a user namespace, so it holds `CAP_FSETID` by default.
- T9 (covers F12): `ftruncate(STDOUT_FILENO, 0)` with stdout attached to a regular file returns `0` and empties the file; with stdout attached to a pipe it returns `EINVAL`. Assert both, and that each is identical across `--verify` runs.
  Existing record coverage: `replay_captured_output_ftruncate_failure_aborts_without_panicking` (`hermit-cli/tests/record_replay.rs:1167`), described under Hermit design.
- T10 (covers F1, F2, F4): two guest threads share a file through duplicated descriptors and a `MAP_SHARED` mapping. One reads, writes and touches the mapping while the other alternately shrinks and grows the file. Assert that every return value, the `SIGBUS` count and the final contents are identical across `--verify` runs.
- T11 (covers F1, F2, F5): record and replay the T1 sequence, plus fd `-1`, with `hermit record` on ptrace, and assert the same return values and file contents on replay.
- U1 (covers F10; host-config): only the host can set the append-only or immutable flag, so CI produces this `EPERM` only on a host that flagged a file the guest can open for writing. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:746`, `detcore/src/lib.rs:2843`).
- U2 (covers F13; host-transient): `EIO` comes from failing host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands it to the guest today. F13 counts toward done only once Hermit hides this error or reports it as an infrastructure error.
- U3 (covers F14; host-config): the maximum file size belongs to the host filesystem under the guest's files, which the test does not choose. Hermit forwards the kernel's `EFBIG` or success unchanged (`detcore/src/syscall_classification.rs:746`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. ftruncate is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1945: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> deterministic (file state, seals and limits are guest inputs; Hermit serializes concurrent I/O); target_policy investigate -> passthrough (the code forwards at detcore/src/lib.rs:2843, and Hermit has no file model for the 2026-08-07 file-object draft); 12 flag behaviours and 11 tests; no coverage token is recorded, F3 to F12 lack tests and F10 needs host privilege or fault injection, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and its maximum file size, inode flags on host files); F10 narrowed to append-only or immutable EPERM (host-config U1), EIO split out as new F13 (host-transient U2), filesystem-limit EFBIG split from F7 as new F14 (host-config U3); T5 no longer asserts the LLONG_MAX result and T7 now covers none; the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `ftruncate`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+ftruncate&body=Discussion+for+the+%60ftruncate%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fftruncate.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60ftruncate%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: ftruncate" (https://github.com/rrnewton/hermit/issues/1945), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy strategy draft (2026-08-07), rejected above

Determinism analysis: ftruncate mutates file size and allocation and can expose host filesystem limits, seals, descriptor permissions, sparse-file behavior, and concurrent I/O ordering.

Hermit design: Apply size changes through deterministic file-object state with scheduler-serialized ordering. Model zero-fill, offsets, seals, limits, and error precedence; bind any host mutation to the same stable object identity.

Legacy required tests (2026-08-07), now T1 to T11:

- Exercise grow, shrink, zero length, unchanged length, invalid/readonly fds, seals, and limit failures. (T1, T4, T5, T6)
- Verify bytes, holes, stat size, and file offsets after each transition. (T1, T3)
- Race reads, writes, mmap access, and duplicate fds under the scheduler. (T10)
- Compare repeated strict runs and record/replay across supported filesystems/backends. (every T runs under `--verify`; T11 for record/replay; tests run on ptrace)

Legacy acceptance text (2026-08-07): Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

ftruncate is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
