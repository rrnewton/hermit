---
schema: syscall-pages/v1
syscall: pwrite64
nr_x86_64: 18
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/387
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pwrite64&body=Discussion+for+the+%60pwrite64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpwrite64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pwrite64%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`pwrite64(fd, buf, count, offset)` writes up to `count` bytes at an explicit file offset and does not move the descriptor's file offset. Detcore models it. It orders the write through a file-contents resource, completes short writes under deterministic I/O, records and replays the result, and advances the virtual mtime (`detcore/src/syscalls/files.rs:2337-2435` at hermit f4055eb9cae4).

## Linux semantics

`pwrite64` is the positional form of `write`. It needs a seekable file, and the descriptor's file offset stays unchanged whatever the result. The kernel can return a short count, for example when the disk is full, at the `RLIMIT_FSIZE` boundary, or when a signal arrives mid-copy.

The errors that matter here:

- `EBADF`: the fd is not open, or is not open for writing.
- `EFAULT`: `buf` is bad.
- `EINVAL`: `offset` is negative.
- `ESPIPE`: the fd is a pipe, FIFO or socket.
- `EFBIG`: offset plus count is beyond the maximum file size or `RLIMIT_FSIZE`; the `RLIMIT_FSIZE` case also raises `SIGXFSZ`.
- `ENOSPC` and `EDQUOT`.

Writing past end of file extends the file and leaves a hole. A successful write of one or more bytes changes the file's mtime and ctime.

### Flags and options

- F1: positional write to a regular file: the data lands at `offset` and the fd's file offset does not move.
- F2: an `offset` past end of file extends the file and leaves a zero-filled hole.
- F3: a negative `offset` gives `EINVAL`.
- F4: a pipe, FIFO or socket fd gives `ESPIPE`.
- F5: an fd opened with `O_APPEND`: Linux appends at end of file and ignores `offset` (a bug documented in pwrite(2)).
- F6: `count` 0 returns 0 and writes nothing.
- F7: short write: the kernel may return fewer than `count` bytes (`ENOSPC`, `RLIMIT_FSIZE`, a signal mid-copy).
- F8: `EFBIG` and `SIGXFSZ` when the write starts at or beyond `RLIMIT_FSIZE` or the filesystem maximum.
- F9: `EBADF` for a closed fd or one not open for writing.
- F10: `EFAULT` for an unreadable `buf`.
- F11: metadata side effect: a write of one or more bytes updates mtime and ctime.
- F12: a Detcore timer-slack procfs fd (`/proc/<pid>/timerslack_ns`) gives `EINVAL` for a negative offset, otherwise `ESPIPE`.

## Determinism analysis

With the same file contents, offset and data, a positional write to a regular file produces the same bytes and the same return value, so the data path repeats. Three things do not repeat on their own:

- Short counts depend on host disk space, quota and signal timing (F7, F8).
- When several guest threads or processes share the file, the order of this write against other writers and readers depends on host scheduling.
- The mtime and ctime that the kernel writes are host wall-clock time (F11).

The kernel refuses pipes and sockets with `ESPIPE` (F4), so endpoint timing never reaches this call. That gives `conditional`: deterministic for a single writer with ample disk, but not in general.

## Hermit design

The policy is `model`, which is what the code at hermit f4055eb9cae4 already does:

- **Dispatch:** `Syscall::Pwrite64` goes to `handle_pwrite64` (`detcore/src/lib.rs:2400`, with `TODO-HUMAN-REVIEW(#683)` at `detcore/src/lib.rs:2399`).
- **Timer-slack procfs fds are emulated:** `EINVAL` for a negative offset, otherwise `ESPIPE` (`detcore/src/syscalls/files.rs:2342-2348`). This covers F12.
- **Ordering:** the write takes a `Permission::W` resource on the file's contents (`detcore/src/syscalls/files.rs:2350-2370`). If the fd has no resource yet, it first determinizes the inode. The resource is released at the end (`detcore/src/syscalls/files.rs:2433`). This serializes the write against other guest accesses to the same file in the deterministic schedule.
- **Short writes:** under `deterministic_io`, a loop reissues the remainder and advances buffer, length and offset (`detcore/src/syscalls/files.rs:2372-2422`). The guest sees a complete write unless an error stops it.
  - Arithmetic overflow maps to `EIO`, `EFAULT` or `EFBIG`.
  - An error after partial progress returns the partial count through `finish_partial_record_or_replay_write` (`detcore/src/tool_local.rs:413-421`).
  - Without `deterministic_io`, the call is a single `record_or_replay` (`detcore/src/syscalls/files.rs:2423-2426`).
- **Metadata:** with `virtualize_metadata` on and at least one byte written, `touch_file` advances the virtual mtime (`detcore/src/syscalls/files.rs:2428-2431`), which hides the host clock (F11).
- **Pipes and sockets:** the handler does not dispatch on fd type, so pipes and sockets reach the kernel and get its `ESPIPE` (F4). That is correct and needs no special case.

Why not `emulate`: the data has to land in the real file so that later reads, `mmap` and child processes see it, so the kernel must perform the write. Why not `refuse`: positional writes are common (databases, linkers, `tests/c/append_pwrite.c`). What remains is test coverage, not a design change.

## Recommended tests

- T1 (covers F1, F5): a positional write leaves the fd offset unchanged, and an `O_APPEND` fd appends whatever the offset. Existing candidate: `tests/c/append_pwrite.c` (manifest cell `c-programs/append-pwrite`, `tests/e2e/manifests/c-programs.yaml:14856`, verify on ptrace).
- T2 (covers F1, F11): pwrite leaves the offset unchanged, and the virtualized mtime advances identically across runs. Existing candidate: `tests/c/syscall_file_metadata.c` (line 125), driven by `hermit-cli/tests/syscall_file_metadata.rs`.
- T3 (covers F3, F12): on a timer-slack procfs fd, a negative offset gives `EINVAL` and offset 0 gives `ESPIPE`. Existing candidate: `timer_slack_procfs_vector_io_matches_linux` in `detcore/tests/misc/mod.rs:434` (pwrite with offset -1 at lines 569-573).
- T4 (covers F2, F6): write 0 bytes, then write past end of file. Check the return values, the file size, and that the hole reads back as zeros, identically across two runs under `--verify`.
- T5 (covers F3, F4, F9, F10): try a negative offset on a regular file, a pipe end, a socketpair end, a closed fd, an `O_RDONLY` fd and a `NULL` buffer. Each must return the Linux errno, and the errno sequence must match a native run.
- T6 (covers F7, F8): with `SIGXFSZ` ignored, set `RLIMIT_FSIZE` below `offset + count`. Expect a short count, then `EFBIG` for a write that starts at the limit, with identical results across runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/387: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_pwrite64 orders the write through a FileContents W resource, completes short writes, records/replays and virtualizes mtime); linux_determinism stays conditional; 12 flags, T1-T6 proposed; tracking_status stays todo (no coverage tokens recorded).

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pwrite64`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pwrite64&body=Discussion+for+the+%60pwrite64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpwrite64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pwrite64%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: pwrite64" (https://github.com/rrnewton/hermit/issues/387), legacy variant A1.

The 2026-07-24 evidence below describes how the call was routed at that time. At hermit f4055eb9cae4, `pwrite64` has its own typed handler (see Hermit design), and the "temporary-passthrough" wording no longer applies.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=pwrite64` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `pwrite64` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
pwrite64(4, "XYZ", 3, 0)        = 3
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
