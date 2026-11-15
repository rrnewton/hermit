---
schema: syscall-pages/v1
syscall: pread64
nr_x86_64: 17
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pread64&body=Discussion+for+the+%60pread64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpread64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pread64%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`pread64` reads from a file at an explicit offset without moving the file position. Hermit serves `/proc/<tid>/timerslack_ns`, other procfs files and the random devices from its own deterministic state, fills regular-file reads to the full count or end of file, and forwards everything else to the kernel through record/replay.

## Linux semantics

`pread64(fd, buf, count, offset)` copies up to `count` bytes, starting at byte `offset` of the file, into `buf`, and returns the number of bytes copied, 0 at end of file, or `-1` with `errno`. The file position shared by duplicates of the open file description is neither read nor changed, so threads can read the same file concurrently without `lseek`. The data is whatever the file holds at the time of the read; for generated files (procfs, sysfs, devices) the driver produces it on demand.

### Flags and options

- F1: the read starts at `offset` and leaves the file position unchanged.
- F2: a negative `offset` gives `EINVAL`; Linux checks it before it looks up the descriptor.
- F3: a `count` of 0 returns 0 without touching `buf`, but still reports descriptor errors.
- F4: the result can be shorter than `count`: at end of file (0 when `offset` is at or past it), and on filesystems or devices that return what they have.
- F5: a descriptor that cannot seek (pipe, FIFO, socket, terminal) gives `ESPIPE`.
- F6: a directory gives `EISDIR`.
- F7: a descriptor that is not open, not open for reading, or opened with `O_PATH` gives `EBADF`.
- F8: a `buf` that is NULL or unmapped gives `EFAULT`; if the fault comes after some bytes were copied, Linux returns that shorter count instead.
- F9: `count` above `MAX_RW_COUNT` (2 GiB minus one page) is cut to that limit, and an `offset + count` that overflows a signed 64-bit file offset gives `EINVAL`.
- F10: on a descriptor opened with `O_DIRECT`, a `buf`, `offset` or `count` that is not aligned to the device's logical block size gives `EINVAL`.
- F11: procfs and sysfs files are generated when they are read; a positioned read of a `seq_file` regenerates the content and returns the bytes at `offset` of the new text.
- F12: `/proc/<tid>/timerslack_ns` returns the target thread's timer slack in nanoseconds as decimal text.
- F13: `/dev/urandom` and `/dev/random` ignore `offset` and return fresh random bytes on every call.
- F14: a device or special file that can block may give `EINTR` when a signal arrives before any data, or `EAGAIN` when it is non-blocking and has no data.
- F15: a `pread64` concurrent with a `write`, `pwrite` or `ftruncate` of the same range by another thread or process may see the old data, the new data, or a mix.

## Determinism analysis

`linux_determinism` is `conditional`. The result is a function of the file's contents at `offset` when the read happens. It repeats when the program reads the same file with the same contents, and no other writer races with it. The sources that break this:

- Generated files. F11 and F12 report live kernel state (process times, memory use, scheduler counters), which differs between runs.
- Randomness. F13 returns different bytes every time by design.
- Concurrent writers. F15 depends on how the reader and the writers interleave.
- Host and filesystem behaviour. F4 short reads on network or FUSE filesystems and devices, F10 block sizes, and F14 interruptions depend on the host and on signal timing.
- Host files. A file that the guest did not create, such as a system library, differs between hosts and over time.

### Cross-host variation

- Device logical block size and filesystem `O_DIRECT` support behind the guest's file: which `buf`, `offset` and `count` alignments an `O_DIRECT` read accepts and which give `EINVAL` (F10). Hermit forwards these reads to the kernel (`detcore/src/syscalls/files.rs:1936-1942`).
- Contents of host files the guest did not create (system libraries, files under `/etc`): the bytes and counts F1 and F4 return differ between hosts and are stable on one host.

## Hermit design

Target policy: `model`. Detcore answers the reads whose content is host state from its own deterministic state, makes regular-file reads return a repeatable count, and orders concurrent access to the same file. The kernel still reads ordinary file data.

- Dispatch. The typed arm (`detcore/src/lib.rs:2370`) calls `handle_pread64` (`detcore/src/syscalls/files.rs:1881`). `pread64` stays in the subscription list of the opt-in fast path as well (`detcore/src/lib.rs:1124`), and is charged `IO_NS` of logical time (`detcore/src/syscall_time.rs:16`, `detcore/src/syscall_time.rs:79`).
- F12. A descriptor bound to `/proc/<tid>/timerslack_ns` goes to `pread_timer_slack` (`detcore/src/syscalls/files.rs:1886-1890`, defined at `detcore/src/syscalls/files.rs:592`). It returns `EINVAL` for a negative offset, checks read access, returns 0 for a zero count, and copies the decimal text of the thread's logical `timer_slack_ns` from the requested offset (`detcore/src/syscalls/files.rs:600-619`).
- F3. A zero count is injected directly, so the kernel reports descriptor errors (`detcore/src/syscalls/files.rs:1892-1896`).
- F2. Any other negative offset returns `EINVAL` (`detcore/src/syscalls/files.rs:1898`).
- F11. A procfs descriptor takes a sanitized snapshot on first use (`detcore/src/syscalls/files.rs:1899-1908`, `detcore/src/fd.rs:415`), and every positioned read returns bytes of that snapshot without moving its shared cursor (`detcore/src/syscalls/files.rs:1910-1917`, `detcore/src/fd.rs:474-475`), so repeated reads see the same text.
- F15. Before reading, Detcore requests a read permission on the file's resource and releases it afterwards (`detcore/src/syscalls/files.rs:1919-1926`, `detcore/src/syscalls/files.rs:1945`), so the scheduler orders the read against writers in the guest.
- F13. For a random device, Detcore fills the buffer from the canonical stream seeded by `rng_seed`, at a stream position equal to `offset` (`detcore/src/syscalls/files.rs:1929-1935`, `detcore/src/syscalls/misc.rs:478-520`).
- F4. For a regular file with deterministic I/O on (`detcore/src/syscalls/files.rs:1936-1938`), `deterministic_pread64` repeats the read with an advancing offset until the buffer is full or end of file, and returns `EOVERFLOW` if the offset would overflow (`detcore/src/syscalls/files.rs:2100-2141`). `hermit run` turns deterministic I/O on unless `--no-deterministic-io` or `--strace-only` is given (`hermit-cli/src/bin/hermit/run.rs:3668`, `hermit-cli/src/bin/hermit/run.rs:3759`); recording turns it off (`hermit-cli/src/bin/hermit/record_start.rs:191-194`).
- Everything else (F5 to F10, F14, and regular files without deterministic I/O) goes through `record_or_replay` (`detcore/src/syscalls/files.rs:1939-1942`), so the kernel produces the errors. The recorder injects the call and stores the bytes it returned (`hermit-cli/src/recorder/fs.rs:516-534`, reached from `hermit-cli/src/recorder.rs:500`); the replayer writes those bytes back and returns their length (`hermit-cli/src/replayer/fs.rs:794-808`, reached from `hermit-cli/src/replayer.rs:404`).

Why `model` and not the alternatives: `passthrough` would expose live procfs values, real random bytes and host-dependent short reads. `emulate` would mean reimplementing filesystem reads that the kernel already does deterministically for guest-created files. `refuse` would break nearly every program.

Differences from Linux found by reading the code, not measured:

- F13: Linux ignores `offset` on the random devices, but Detcore uses it as the stream position, so two `pread64` calls at the same offset return the same bytes in one run.
- F8: `deterministic_pread64` returns the error when a later fragment fails after earlier bytes were copied (`detcore/src/syscalls/files.rs:2138`), where Linux, and Detcore's own `deterministic_read` (`detcore/src/syscalls/files.rs:2090-2093`), return the bytes already read.
- F2 and F7: for a descriptor that is not open and a negative offset, Detcore's descriptor lookup returns `EBADF` first (`detcore/src/syscalls/files.rs:1886`, `detcore/src/tool_local.rs:2559-2567`, `detcore/src/tool_local.rs:740`), where Linux returns `EINVAL`.
- F4 and F8 under record: when the kernel returns 0 and `buf` is NULL, the recorder stores `EFAULT` as the event (`hermit-cli/src/recorder/fs.rs:525-529`), so replay would report an error that the recorded run did not see.

## Recommended tests

- T1 (covers F1, F4): write a known file, `pread64` from the middle and at end of file, and check the bytes, the counts (including 0 past the end), and that `lseek(fd, 0, SEEK_CUR)` is unchanged.
  Existing partial coverage: `tests/c/simple/pread64_nostdlib.c` line 50 reads 4096 bytes of `/bin/true` at offset 0 and checks only for a positive result (cell `c-programs/pread64-nostdlib`, `tests/e2e/manifests/c-programs.yaml:5603`, verify ci on ptrace). `tests/c/dup_shared_offset.c` (helper at lines 71-81, used at line 134; cell `c-programs/dup-shared-offset`, `tests/e2e/manifests/c-programs.yaml:15923`) and `tests/c/lseek_positioning.c` (helper at lines 75-85, used at line 148; cell `c-programs/lseek-positioning`, `tests/e2e/manifests/c-programs.yaml:15991`) use `pread` to read back whole files, both verify ci on ptrace.
- T2 (covers F2, F3, F7): with raw `syscall(SYS_pread64, ...)`, check `EINVAL` for offset -1 on an open file, `EBADF` for a zero count on a closed descriptor, `EBADF` on an `O_WRONLY` and an `O_PATH` descriptor, and the error for a closed descriptor with offset -1, which pins the precedence question above.
- T3 (covers F5, F6): `ESPIPE` on a pipe and on a socketpair, `EISDIR` on a directory descriptor.
- T4 (covers F8): `EFAULT` for a NULL buffer with a non-zero count; then a buffer whose second page is `PROT_NONE`, which on Linux returns the count of the first page.
- T5 (covers F9, F10): a count above 2 GiB on a small file returns the file size; `offset = INT64_MAX` with count 2 gives `EINVAL`; on a filesystem that supports `O_DIRECT`, an unaligned buffer gives `EINVAL` (skip with a stated reason when `O_DIRECT` is refused).
- T6 (covers F11): `pread64` of `/proc/self/stat` returns the sanitized snapshot, and a second read at the same offset returns the same bytes.
  Existing coverage: `tests/c/procfs_positioned_probe.c` (`check_pread_sanitized` at line 56, `pread` at line 64; cell `c-programs/procfs-positioned-probe`, `tests/e2e/manifests/c-programs.yaml:6208`, verify ci on ptrace). The repeated-read half is still missing.
- T7 (covers F12): set the timer slack with `prctl(PR_SET_TIMERSLACK)`, then `pread64` `/proc/self/task/<tid>/timerslack_ns` at offsets 0 and 2 and check the text.
- T8 (covers F13): two `pread64` calls of `/dev/urandom` at offset 0 in one run, printing both; under `hermit run --verify` the output must match across runs, and the test records whether the two reads are equal, which pins the difference noted above.
- T9 (covers F14): where a file source that can block is available (for example a FUSE mount the test creates), block a `pread64` on it, deliver a signal whose handler was installed without `SA_RESTART`, and check `EINTR`, identically in both runs of `hermit run --verify`; skip with a stated reason when no such source exists.
- T10 (covers F15): one thread `pwrite`s a pattern while another `pread64`s the same range in a loop, printing what it saw, under `hermit run --verify`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (Detcore serves timerslack_ns, procfs snapshots and the random devices itself, fills regular-file reads under deterministic I/O, orders reads with a resource lock, forwards the rest through record/replay); linux_determinism stays conditional (live procfs, randomness, concurrent writers, host files); 15 flag behaviours and 10 tests listed; four code-read differences from Linux noted for tests to confirm; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (device block size and O_DIRECT support for F10, host file contents for F1 and F4); no U items, since no listed error is host-only; no open questions to resolve

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `pread64`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+pread64&body=Discussion+for+the+%60pread64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fpread64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60pread64%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `pread64`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
