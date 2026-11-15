---
schema: syscall-pages/v1
syscall: fallocate
nr_x86_64: 285
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/347
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fallocate&body=Discussion+for+the+%60fallocate%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffallocate.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fallocate%60%2C+or+in+what+the+page+says%3F%0A
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

`fallocate(fd, mode, offset, len)` allocates, deallocates, zeroes or shifts file space over a byte range of an open file. Hermit forwards it to the host kernel unchanged. For a given file on a given filesystem type the result repeats. Which modes succeed, and what `st_blocks` reports afterwards, depend on the host filesystem behind the guest's file.

Manual reference: Linux man-pages fallocate(2).

## Linux semantics

The VFS (`vfs_fallocate`) validates the arguments and the descriptor, then hands the mode to the filesystem's `->fallocate` method. Filesystems differ in which modes they support. For example, tmpfs implements only mode 0, `KEEP_SIZE` and `PUNCH_HOLE|KEEP_SIZE`, while ext4 and xfs implement most modes. A mode the filesystem does not implement fails with `EOPNOTSUPP`. A successful call can change the file size, its allocated blocks (`st_blocks`) and, for the hole, zero, collapse and insert modes, the bytes a later read returns. The call writes no memory in the caller.

### Flags and options

- F1: `mode == 0` (allocate): reserve blocks for `[offset, offset+len)` and extend `st_size` if the range ends past EOF. Newly allocated ranges read back as zeros.
- F2: `FALLOC_FL_KEEP_SIZE` (0x01) with mode 0: allocate, but never change `st_size`, so blocks past EOF are preallocated and `st_blocks` grows
- F3: `FALLOC_FL_PUNCH_HOLE` (0x02): deallocate the range so it reads back as zeros. It must be combined with `KEEP_SIZE`; without it the call fails with `EOPNOTSUPP`.
- F4: `FALLOC_FL_NO_HIDE_STALE` (0x04): reserved. No mainline filesystem implements it, so it fails with `EOPNOTSUPP`.
- F5: `FALLOC_FL_COLLAPSE_RANGE` (0x08): remove the range and shift the following data down, shrinking the file. It must be used alone. `offset` and `len` must be multiples of the filesystem block size, and the range must end before EOF; otherwise `EINVAL`.
- F6: `FALLOC_FL_ZERO_RANGE` (0x10): make the range read as zeros, typically by converting it to unwritten extents. `KEEP_SIZE` controls whether the size may grow.
- F7: `FALLOC_FL_INSERT_RANGE` (0x20): insert a hole at `offset` and shift the following data up, growing the file. It must be used alone, with the same block-alignment rule as F5, and `offset` must be before EOF.
- F8: `FALLOC_FL_UNSHARE_RANGE` (0x40): break shared (reflinked) extents in the range so later writes cannot fail for lack of space. It works only on filesystems with reflink support, such as xfs and btrfs.
- F9: `FALLOC_FL_WRITE_ZEROES` (0x80, newer kernels; Linux 6.17 is believed to be the first): zero the range by writing zeros on the device. It must be used alone, and filesystems and devices without support return `EOPNOTSUPP`. On older kernels the bit is unknown and gives `EOPNOTSUPP`.
- F10: argument validation: `offset < 0` or `len <= 0` gives `EINVAL`. An invalid combination of mode bits gives `EINVAL` or `EOPNOTSUPP`, as F3 to F9 say. Unknown bits give `EOPNOTSUPP`. `offset + len` beyond the filesystem's maximum file size, or overflowing, gives `EFBIG`.
- F11: descriptor and file type: a bad fd, or one not open for writing, gives `EBADF`. A directory gives `EISDIR`. A pipe or FIFO gives `ESPIPE`. Other non-regular files, apart from block devices, give `ENODEV`.
- F12: file state: an append-only inode refuses every mode except F1 and F2, and an immutable inode refuses every mode; both give `EPERM`. A memfd sealed against writes or growth gives `EPERM`. An active swapfile gives `ETXTBSY`.
- F13: host-transient failures: `ENOSPC` or `EDQUOT` when the host filesystem's free space or quota runs out, and `EIO` from failing storage. All three depend on host state shared with other processes.
- F14: host-config failure: `EOPNOTSUPP` for a mode the host filesystem behind the guest's file does not implement (F3 to F9 say which modes; tmpfs implements only mode 0, `KEEP_SIZE` and `PUNCH_HOLE|KEEP_SIZE`).
- F15: `EINTR` when a signal interrupts a long allocation.

## Determinism analysis

`fallocate` repeats when the guest's file lives on the same filesystem type with the same block size and enough free space, and nothing outside the container modifies the file. Those are the assumptions Hermit documents for the whole synchronous extent-operation group. Under them, the return value, the new `st_size` and the bytes read back are functions of the guest's own earlier operations.

### Nondeterminism sources

- Filesystem mode support (F3 to F9, F13). The same program succeeds on ext4 and gets `EOPNOTSUPP` on tmpfs. Within one host configuration the outcome is fixed.
- Block size, which decides whether F5 and F7 accept a range and how much `st_blocks` grows.
- Free space and quotas (`ENOSPC`, `EDQUOT`), which are host state shared with other processes.
- Extent and page-cache state. These affect latency only, and Detcore charges every syscall a fixed virtual cost (`detcore/src/lib.rs:1936`). `fallocate` has no entry in the cost table, so it falls back to `detcore/src/syscall_time.rs:156`.
- `st_blocks` passes through raw. `determinize_stat` (`detcore/src/syscalls/files.rs:2981`) rewrites the timestamps (`detcore/src/syscalls/files.rs:3021`, `detcore/src/syscalls/files.rs:3025`) but leaves `st_blocks` and `st_size` as the host reports them.

### Cross-host variation

- Filesystem type behind the guest's file: which of F3 to F9 succeed and which give `EOPNOTSUPP` (F14).
- Filesystem block size: whether F5 and F7 accept a range, and how much `st_blocks` grows after F1, F2 and F6.
- Kernel version: F9 (`FALLOC_FL_WRITE_ZEROES`) exists only on newer kernels; older ones return `EOPNOTSUPP`.

## Hermit design

Target policy: `passthrough`, changed from `investigate`. The legacy issue asked for a handler (NEEDS-HANDLER). The code at the cited SHA already decides otherwise, and this body agrees:

- `fallocate` is classified `PassThrough` at `detcore/src/syscall_classification.rs:760`, under the group comment at `detcore/src/syscall_classification.rs:756`. That comment states that synchronous extent and pathname operations "are repeatable for guest-owned files in a fixed namespace with adequate space and no external mutation". The block carries `TODO-HUMAN-REVIEW(PR-675)` at `detcore/src/syscall_classification.rs:758`.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `representative_policies_stay_in_their_reviewed_sections` asserts the classification at `detcore/src/syscall_classification.rs:2053`.

Why not the alternatives:

- `model`/`emulate`: Hermit has no model of file extents. Emulating `fallocate` would mean reimplementing per-filesystem mode support and block-size rules, only to give an answer the real filesystem already gives the same way every time under the stated assumptions.
- `refuse`: real programs call `fallocate` routinely (databases, `posix_fallocate`, package managers). Refusing it would break them for no determinism gain.

There is one gap between the passthrough and Detcore's metadata virtualization. A successful `write` calls `touch_file` when `virtualize_metadata` is set (`detcore/src/syscalls/files.rs:2428`), which advances the file's logical mtime. A passthrough `fallocate` never reaches that code. After F1, F3, F5, F6 or F7 changes the size or the contents, the guest's `stat` still shows the old logical mtime, because `determinize_stat` takes mtime from the per-inode logical clock (`detcore/src/syscalls/files.rs:3025`; a file first seen gets the epoch, `detcore/src/tool_global.rs:2524`). Linux would update mtime and ctime. The value is deterministic but not faithful. Whether to fix it is an open question below.

## Recommended tests

- T1 (covers F1, F2, F3, F10): on a fresh `mkstemp` file under `hermit run --verify` on ptrace, use raw `syscall(SYS_fallocate, ...)`. Check that mode 0 extends the file to 8192. Check that `KEEP_SIZE` past EOF keeps `st_size`. Check that `PUNCH_HOLE|KEEP_SIZE` in the middle reads back as zeros with the size unchanged. Check that `PUNCH_HOLE` without `KEEP_SIZE` gives `EOPNOTSUPP`, and that `offset = -1`, `len = 0` and `offset = LLONG_MAX` with `len = 2` give `EINVAL`, `EINVAL` and `EFBIG`.
  Existing partial coverage: `tests/c/fallocate_extents.c:43` (mode 0 to 8192), `tests/c/fallocate_extents.c:55` (punch hole keeps the size), `tests/c/fallocate_extents.c:61` (the hole reads back as zeros) and `tests/c/fallocate_extents.c:77` (`KEEP_SIZE` past EOF), through the glibc wrapper (cell `c-programs/fallocate-extents`, `tests/e2e/manifests/c-programs.yaml:11699`, verify ci on ptrace). Also raw mode 0 at `tests/c/syscall_file_io.c:134`, which accepts `EOPNOTSUPP` (`tests/c/syscall_file_io.c:141`), run by cell `c-programs/syscall-file-io` (`tests/e2e/manifests/c-programs.yaml:9214`) and by the cargo test `deterministic_file_io_syscalls_verify` (`hermit-cli/tests/syscall_file_io.rs:51`, which passes `--backend=ptrace`).
- T2 (covers F4, F5, F6, F7, F8, F9): for each of `NO_HIDE_STALE`, `COLLAPSE_RANGE`, `ZERO_RANGE`, `ZERO_RANGE|KEEP_SIZE`, `INSERT_RANGE`, `UNSHARE_RANGE` and `WRITE_ZEROES`, issue the call with block-aligned and misaligned ranges. Assert the documented contents and size where it succeeds, and the exact errno where the test filesystem lacks the mode. Record the filesystem type in the output, so `--verify` compares like with like.
- T3 (covers F11, F12): assert `EBADF` on fd `-1` and on an `O_RDONLY` descriptor, `EISDIR` on a directory descriptor and `ESPIPE` on a pipe. Seal a `memfd_create(MFD_ALLOW_SEALING)` with `F_SEAL_GROW` and assert that a size-extending `fallocate` gives `EPERM`.
- T4 (covers none): fill a small size-limited tmpfs, if the test harness can mount one inside the container, and assert `ENOSPC` from a mode-0 `fallocate` past the limit. A container-owned filesystem of fixed size makes this `ENOSPC` guest-determined; it does not cover F13, which is the host's own free space.
- U1 (covers F13; host-transient): `ENOSPC`, `EDQUOT` and `EIO` from the host filesystem depend on free space, quotas and storage health shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands them to the guest today. F13 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.
- U2 (covers F14; host-config): the CI host's filesystem decides which modes are missing, so CI cannot produce `EOPNOTSUPP` for every mode. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:760`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/347: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> passthrough, matching the reviewed classification at detcore/src/syscall_classification.rs:760; kept conditional (mode support, block size, free space and raw st_blocks depend on the host filesystem); 13 flag behaviours and 4 tests listed; an open question (passthrough fallocate does not advance the virtual mtime) keeps tracking_status todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type, block size, kernel version); F13 narrowed to host-transient errors (U1); new F14, EOPNOTSUPP from the host filesystem (host-config U2), and F15, EINTR; T4 no longer claims F13; the two open questions on unreachable errors and host filesystems are resolved by the owner decisions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fallocate`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fallocate&body=Discussion+for+the+%60fallocate%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffallocate.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fallocate%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fallocate" (https://github.com/rrnewton/hermit/issues/347), legacy variant A2. It was rank 68 in the batch-2 classification sequence, which excluded the 50 syscalls already seen in batch-1 workloads.

### Open questions

- Should Detcore advance the virtual mtime (`touch_file`) after a successful size- or content-changing `fallocate`, as it does for `write` (`detcore/src/syscalls/files.rs:2428`)? Today the guest sees a stale but deterministic mtime. Fixing it needs a typed post-hook instead of the plain passthrough. `ftruncate` would raise the same question.
- F15: whether a passthrough `fallocate` can return `EINTR` under Hermit depends on when Detcore delivers guest signals relative to the host call. No test exercises it.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fallocate -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(285, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fallocate(-1, FALLOC_FL_KEEP_SIZE|FALLOC_FL_PUNCH_HOLE|FALLOC_FL_NO_HIDE_STALE|FALLOC_FL_COLLAPSE_RANGE|FALLOC_FL_ZERO_RANGE|FALLOC_FL_INSERT_RANGE|FALLOC_FL_UNSHARE_RANGE|0xffffff80, -1, -1) = -1 EBADF (Bad file descriptor)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
