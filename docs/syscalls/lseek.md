---
schema: syscall-pages/v1
syscall: lseek
nr_x86_64: 8
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lseek&body=Discussion+for+the+%60lseek%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flseek.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lseek%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`lseek(fd, offset, whence)` moves the file position of an open file description and returns the new position. Detcore handles it in `handle_lseek` (`detcore/src/syscalls/files.rs:1950`): it computes the result itself for the descriptors it virtualizes (virtual random devices, inherited container output, procfs snapshots and `/proc/<pid>/timerslack_ns`), and sends every other seek to the kernel through `record_or_replay`, so record/replay keeps the offset even when the replay descriptor is only a placeholder.

Manual reference: Linux man-pages lseek(2).

## Linux semantics

The file position belongs to the open file description, so descriptors copied by `dup`, `fork` or `SCM_RIGHTS` share it. `lseek` does not block and does not touch file data. Seeking past the end of a regular file is allowed; a later write there leaves a hole.

What a seek means depends on the file's `llseek` method:

- Regular files support all five `whence` values. `SEEK_DATA` and `SEEK_HOLE` report extents at the granularity the filesystem tracks, or treat the whole file as data when the filesystem has no hole support.
- Directories accept `SEEK_SET` to 0 (rewind) and otherwise use filesystem-specific cookies, the same values that `getdents` reports in `d_off` and that `telldir` returns. On ext4 with hashed directories these are hash values, not ordinal positions.
- Pipes, FIFOs, sockets and most ttys return `ESPIPE`.
- Some character devices (`/dev/null`, `/dev/zero`, `/dev/urandom`) accept any seek and return 0.
- procfs `seq_file`s accept only `SEEK_SET` and `SEEK_CUR`; some `/proc/net` files return `ESPIPE`.

Errors: `EBADF` for a descriptor that is not open or was opened with `O_PATH`; `EINVAL` for an unknown `whence` or a resulting offset that is negative or beyond the filesystem's maximum; `ENXIO` for `SEEK_DATA`/`SEEK_HOLE` at or past the end of file; `ESPIPE` as above. `EOVERFLOW` applies only to 32-bit callers.

### Flags and options

- F1: `SEEK_SET`: the position becomes `offset`
- F2: `SEEK_CUR`: the position becomes the current position plus `offset`; `lseek(fd, 0, SEEK_CUR)` reads the position without moving it
- F3: `SEEK_END`: the position becomes the file size plus `offset`
- F4: `SEEK_DATA`: the next offset at or after `offset` that holds data; `ENXIO` at or past the end of file
- F5: `SEEK_HOLE`: the next hole at or after `offset`, where end of file counts as a hole; `ENXIO` at or past the end of file
- F6: unknown `whence` value: `EINVAL`
- F7: resulting offset negative or above the filesystem limit: `EINVAL`; an offset past end of file is allowed, and a later write creates a hole
- F8: unseekable files (pipe, FIFO, socket, tty): `ESPIPE`
- F9: descriptor not open, or opened with `O_PATH`: `EBADF`
- F10: directory descriptor: `SEEK_SET` to 0 rewinds; other positions are filesystem cookies shared with `getdents` `d_off` and `telldir`
- F11: procfs files: `seq_file` semantics (only `SEEK_SET` and `SEEK_CUR`, otherwise `EINVAL`), and `ESPIPE` from some `/proc/net` files
- F12: character devices with a no-op `llseek` (`/dev/null`, `/dev/zero`, `/dev/urandom`, `/dev/random`): every valid `whence` returns 0
- F13: shared position: `dup`, `fork` and `SCM_RIGHTS` copies move one position, so a seek through one copy changes where the other reads or writes

## Determinism analysis

`linux_determinism: conditional`. For a file the program owns, the result is a pure function of the arguments, the current position and the file size, so a seek repeats exactly. It does not repeat when the answer comes from host state: directory cookies (hash values chosen by the filesystem), `SEEK_DATA`/`SEEK_HOLE` extent granularity (a filesystem property), the size of a file another process is writing, or the position of a descriptor the program shares with another process whose reads and writes interleave by host scheduling.

### Nondeterminism sources

- Host scheduling of processes that share one file position (F13)
- File size changes by processes outside the guest, visible through `SEEK_END`
- Filesystem-specific directory cookies (F10) and hole granularity (F4, F5)
- The outer runner's choice of what fd 1 and 2 point to: a log file would expose an offset that includes the runner's own writes

## Hermit design

Target policy: `model`. The kernel owns the position of ordinary files; Detcore owns the position of every descriptor it virtualizes. At hermit f4055eb9cae4, the dispatch arm `detcore/src/lib.rs:2371` calls `handle_lseek` (`detcore/src/syscalls/files.rs:1950`), which checks in this order:

- **Virtual random devices** (`detcore/src/syscalls/files.rs:1965`, F12): `random_device_lseek_result` (`detcore/src/syscalls/files.rs:319`) gives `EBADF` for `O_PATH`, 0 for the five valid `whence` values and `EINVAL` otherwise, matching Linux's `noop_llseek` (unit test `random_device_lseek_matches_linux_noop_llseek`, `detcore/src/syscalls/files.rs:4945`).
- **Inherited container stdout and stderr** (`detcore/src/syscalls/files.rs:1968`, F8): always `ESPIPE`, even if the outer runner redirected them to a regular file. The doc comment on `is_inherited_container_output` (`detcore/src/syscalls/files.rs:351`) gives the reason: that file also holds Hermit's own diagnostics, so its offset would make tool logging guest-visible. A file the guest itself `dup2`s onto fd 1 keeps normal seek semantics. This is a deliberate difference from Linux when the runner gives a seekable file.
- **`timerslack_ns` on an `O_PATH` descriptor** (`detcore/src/syscalls/files.rs:1971`, F9): `EBADF`.
- **Every non-procfs descriptor** (`detcore/src/syscalls/files.rs:1988`; F1 to F10, F13): the call goes to `record_or_replay`, which in run mode injects it unchanged. The review comment above it (`detcore/src/syscalls/files.rs:1976`) records why: under replay the descriptor is a placeholder whose kernel position never advances, and a live `SEEK_CUR` (glibc's `__tzfile_read` rewinding `/etc/localtime`) returned 0 instead of the recorded offset. The recorder logs the plain return value (`hermit-cli/src/recorder.rs:544`). The replayer (`handle_optional_fd_position`, `hermit-cli/src/replayer.rs:1875`) re-applies the seek to files present in the replay root and asserts the offset matches, restores `SEEK_DATA`/`SEEK_HOLE` results by an explicit `SEEK_SET`, and otherwise returns the recorded value.
- **`/proc/<pid>/timerslack_ns`** (`detcore/src/syscalls/files.rs:1991`, F11): the position is kept in the virtual open file description with `seq_lseek` rules: only `SEEK_SET` and `SEEK_CUR`, otherwise `EINVAL`; moving to a new positive offset re-checks the target task.
- **procfs descriptor before its first snapshot** (`detcore/src/syscalls/files.rs:2016`): the seek is injected live and the result stored as the virtual offset.
- **procfs snapshot descriptor** (`detcore/src/syscalls/files.rs:2026`): `SEEK_SET`, `SEEK_CUR` and `SEEK_END` are computed against the snapshot length; `SEEK_DATA`/`SEEK_HOLE` give `ENXIO` outside the snapshot, and `SEEK_HOLE` returns its length; other `whence` values give `EINVAL`, a negative result `EINVAL` and an overflowing one `EOVERFLOW`.

Why `model`: `passthrough` would expose the outer runner's log offset and break replay of every placeholder descriptor (the `__tzfile_read` failure above); `emulate` would require Detcore to know every file's size and extent map; `refuse` is not an option for a call every stdio library makes.

What the model does not cover: Detcore sorts each `getdents`/`getdents64` buffer by name (`detcore/src/syscalls/files.rs:4582`, `detcore/src/syscalls/files.rs:4622`) but writes back each entry's host `d_off` unchanged (`detcore/src/dirents.rs:101`). A directory's `lseek(fd, 0, SEEK_CUR)`, `telldir` and `seekdir` therefore see host filesystem cookies (F10). This is read from the code, not measured; see Open questions.

## Recommended tests

- T1 (covers F1, F2, F3, F7): on a regular file the guest creates: `SEEK_END` reports the size, `SEEK_SET` then read returns the right bytes, `SEEK_CUR` adds a signed delta, `SEEK_END` with a negative delta reaches the last byte, a seek past the end followed by a write leaves a zero-filled hole, and a seek to a negative offset gives `EINVAL`. Run under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/lseek_positioning.c` (cell `c-programs/lseek-positioning`, `tests/e2e/manifests/c-programs.yaml:15991`, verify ci on ptrace). It does not test the negative-offset `EINVAL`.
- T2 (covers F4, F5): on a sparse file the guest creates, `SEEK_DATA` and `SEEK_HOLE` from offset 0 and from inside the hole, plus `ENXIO` at end of file; print the offsets and require both `--verify` runs to agree. Also record and replay it.
  Existing partial candidate: `tests/c/record_replay_file_state.c:157`, which calls `SEEK_DATA` then `SEEK_HOLE` in a record/replay fixture.
- T3 (covers F6, F9): `whence = 5` gives `EINVAL`; a closed descriptor gives `EBADF`; an `O_PATH` descriptor gives `EBADF` for every `whence`.
- T4 (covers F8): `lseek` on a pipe, a socketpair end and the inherited stdout/stderr gives `ESPIPE` (including `dup` copies), while a regular file `dup2`'d onto fd 1 reports offset 0.
  Existing candidate: `inherited_container_output_does_not_expose_capture_offset` (`hermit-cli/tests/cli.rs:2764`) with fixture `tests/c/stdio_lseek_identity.c`, which covers the stdout/stderr part; pipes and sockets still need a case.
- T5 (covers F10): read a directory of 20 guest-created files with `getdents64` in small buffers, print every `d_off` and `lseek(fd, 0, SEEK_CUR)`, then `seekdir` to a mid-listing `telldir` value and print the entries that follow. Require identical output across `--verify` runs and no skipped or repeated names.
- T6 (covers F11): on `/proc/self/timerslack_ns`, `SEEK_END` gives `EINVAL` and `SEEK_SET`/`SEEK_CUR` succeed; on a snapshotted `/proc/self/status`, `SEEK_END` reports the snapshot length and `SEEK_DATA` past it gives `ENXIO`.
- T7 (covers F12, F9): every `whence` on `/dev/urandom` returns 0, an invalid one gives `EINVAL`, and an `O_PATH` descriptor gives `EBADF`.
  Existing candidates: unit test `random_device_lseek_matches_linux_noop_llseek` (`detcore/src/syscalls/files.rs:4945`) and `run_kvm_random_device_lseek_matches_linux` (`hermit-cli/tests/cli.rs:4531`), which also runs a ptrace control but returns early when `/dev/kvm` is absent.
- T8 (covers F13, F2): a parent and a forked child share one descriptor and alternate `read` and `lseek(fd, 0, SEEK_CUR)`; every printed offset must match across `--verify` runs. Record and replay a guest that reads a regular file and rewinds with `lseek(fd, -N, SEEK_CUR)`.
  Existing candidate for the replay half: `record_regular_file_lseek_seek_cur` (`hermit-cli/tests/record_replay.rs:1403`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (Detcore computes seeks on virtual random devices, inherited stdout/stderr, timerslack_ns and procfs snapshots; other seeks go through record_or_replay); linux_determinism stays conditional; 13 flag behaviours and 8 tests listed; open question on host directory d_off cookies; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lseek`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lseek&body=Discussion+for+the+%60lseek%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flseek.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lseek%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `lseek`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Directory positions (F10): Detcore sorts each `getdents` buffer by name but keeps each entry's host `d_off` (`detcore/src/dirents.rs:101`). The guest therefore sees host filesystem cookies through `lseek(fd, 0, SEEK_CUR)` and `telldir`. After the sort, an entry's `d_off` no longer marks the position just after that entry in the order the guest sees, so a `seekdir` to a `telldir` value taken in the middle of a buffer could skip or repeat names. Both points are read from the code and not measured; T5 measures them. Whether Detcore should hand out its own ordinal cookies is a design question for the owner.
- Inherited stdout and stderr always give `ESPIPE` (`detcore/src/syscalls/files.rs:1968`), even when Linux would allow a seek because the outer runner provided a regular file. The code comment calls this deliberate; it is recorded here as an accepted difference from Linux.
