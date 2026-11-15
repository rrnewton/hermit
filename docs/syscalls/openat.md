---
schema: syscall-pages/v1
syscall: openat
nr_x86_64: 257
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+openat&body=Discussion+for+the+%60openat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopenat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60openat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 12
---

## Summary

`openat(dirfd, pathname, flags, mode)` resolves a path relative to a directory descriptor (or the current directory for `AT_FDCWD`) and returns the lowest free file descriptor for a new open file description. It is the call glibc's `open()` and `fopen()` actually issue on x86-64, so it is the most common way programs open files. Detcore handles it with a typed handler (`detcore/src/lib.rs:2362`, `handle_openat` at `detcore/src/syscalls/files.rs:979` at hermit f4055eb9cae4) that lets the kernel open the file, then classifies the new descriptor (random device, Detcore-managed pipe, Detcore-classified procfs file) and records it in Detcore's descriptor table.

Manual reference: Linux man-pages openat(2).

## Linux semantics

The kernel resolves `pathname` starting from `dirfd` when the path is relative, from the current directory when `dirfd` is `AT_FDCWD`, and from the process root when the path is absolute (then `dirfd` is ignored). It follows symlinks except where flags forbid it, checks permissions, and creates the file if asked. It allocates a new open file description, with offset 0 and status flags taken from `flags`, and returns the lowest-numbered descriptor not in use.

The result depends on filesystem state (which paths exist, permissions, symlinks), on the descriptor table (which number is lowest), on what `dirfd` names, and on the process umask and credentials. Opening a FIFO waits for a peer. Opening some devices has side effects, for example a terminal can become the controlling terminal.

### Flags and options

- F1: access mode `O_RDONLY`, `O_WRONLY` or `O_RDWR`: decides which later reads and writes succeed. The value 3 is not portable; Linux accepts it as a special mode for some devices.
- F2: `O_CREAT` with `mode`: creates a missing regular file with permission bits `mode & ~umask`, owned by the caller's filesystem uid and gid, or by the directory's gid under a set-group-ID directory. Without `O_CREAT` or `O_TMPFILE`, `mode` is ignored.
- F3: `O_EXCL` with `O_CREAT`: fails with `EEXIST` if the path exists, including a dangling symlink, so creation is atomic.
- F4: `O_TRUNC`: truncates an existing regular file opened for writing to length 0, which updates mtime and ctime. It is ignored on FIFOs and terminals.
- F5: `O_APPEND`: every later write goes to end of file.
- F6: `O_NONBLOCK` (also `O_NDELAY`): an open of a FIFO does not wait for a peer; read-only succeeds, and write-only fails with `ENXIO` when there is no reader. Later I/O on the description does not wait.
- F7: `O_CLOEXEC`: sets `FD_CLOEXEC` on the new descriptor atomically, so it closes on `execve`.
- F8: `O_DIRECTORY`: fails with `ENOTDIR` unless the path is a directory.
- F9: `O_NOFOLLOW`: fails with `ELOOP` if the final component is a symlink (with `O_PATH`, it opens the symlink itself).
- F10: `O_PATH`: returns a descriptor that only locates the object. The file is not opened for I/O, most other flags are ignored, and read and write give `EBADF`. Such a descriptor is a valid `dirfd` for later `*at` calls.
- F11: `O_TMPFILE` (with `O_RDWR` or `O_WRONLY`): `pathname` names a directory, and the call creates an unnamed regular file in it. Gives `EOPNOTSUPP` if the filesystem does not support it.
- F12: `O_NOCTTY`: opening a terminal does not make it the controlling terminal.
- F13: `O_SYNC` and `O_DSYNC`: later writes return only after the data (and for `O_SYNC` the metadata) reach stable storage.
- F14: `O_DIRECT`: later I/O bypasses the page cache and needs aligned buffers. Gives `EINVAL` where the filesystem does not support it.
- F15: `O_NOATIME`: reads do not update atime. Only the file owner or a process with `CAP_FOWNER` may set it, otherwise `EPERM`.
- F16: `O_LARGEFILE`, `O_ASYNC` and unknown bits: `O_LARGEFILE` is implied on x86-64. `O_ASYNC` has no effect at open time. Unknown flag bits are ignored, not rejected (`openat2` is the variant that rejects them).
- F17: descriptor number: the result is the lowest descriptor not in use, at most `RLIMIT_NOFILE - 1`.
- F18: special paths: `/dev/random` and `/dev/urandom` give random-device descriptors. `/proc/...` and `/sys/...` paths name generated files. `/dev/stdin`, `/dev/fd/N` and `/proc/self/fd/N` reopen an existing open object, and for a pipe that creates a new open file description on the same pipe.
- F19: `dirfd`: `AT_FDCWD` resolves a relative path through the current directory; a directory descriptor (including an `O_PATH` one) resolves it from that directory; an absolute `pathname` ignores `dirfd`. A relative path with a `dirfd` that is not open gives `EBADF`, and one that is not a directory gives `ENOTDIR`.
- F20: errors: `ENOENT` (missing component or empty path), `EEXIST`, `EACCES`, `EPERM`, `ELOOP`, `ENAMETOOLONG`, `ENOTDIR`, `EISDIR` (directory opened for writing), `EROFS`, `ETXTBSY` (writing a running executable), `EFAULT` (bad `pathname` pointer), `EMFILE` and `ENFILE` (descriptor limits), `ENXIO`, `EINVAL` (bad flag combination such as `O_TMPFILE` without write access), `EOVERFLOW`, `EINTR` (interrupted wait on a FIFO or device), `ENOSPC` and `EDQUOT` when creating.

## Determinism analysis

`linux_determinism: conditional`. Given the same filesystem state, descriptor table, current directory, umask and credentials, `openat` returns the same descriptor number and opens the same object, because the lowest-free-descriptor rule is deterministic (F17). It can vary in these ways:

- Filesystem state outside the guest's control: which host paths exist, their permissions and contents, and files other processes create or remove between runs (F2, F3, F20).
- Identity of the object: inode and device numbers, and the creation time of new files, come from the host and differ between runs. They leak through later `fstat`, `/proc/self/fd` and `/proc/self/fdinfo`.
- Generated content behind special paths: random devices and procfs or sysfs files (F18).
- Waits: a FIFO open waits for a peer that another process supplies, and a signal can interrupt the wait (F6, F20 `EINTR`).
- Concurrency: threads that share a descriptor table race for the lowest free number, and an `O_EXCL` creation races with other creators.

## Hermit design

Target policy: `model`. The kernel resolves paths and creates files. Detcore controls the descriptor's classification, its scheduling, and the identity the guest later sees. At hermit f4055eb9cae4:

- The dispatch sends `Syscall::Openat` to `handle_openat` (`detcore/src/lib.rs:2362`); `open` and `creat` are converted to `Openat` and use the same handler (`detcore/src/lib.rs:2363-2364`). `openat` is in the Determinized list (`detcore/src/syscall_classification.rs:139`).
- A null `pathname` fails with `EFAULT` from Detcore before the kernel runs (`detcore/src/syscalls/files.rs:984`). Detcore reads the path from guest memory and works out what it names: an absolute path as given, an `AT_FDCWD`-relative path resolved through the guest's root and current directory, and a `dirfd`-relative path joined to the path Detcore recorded for `dirfd` (`detcore/src/syscalls/files.rs:993-1002`).
- It requests a path resource from the scheduler, then runs the open, unchanged, through `record_or_replay` (`detcore/src/syscalls/files.rs:1004-1008`). Errors pass back to the guest unchanged (`detcore/src/syscalls/files.rs:1129-1133`), so F19 and F20 errors come from the kernel.
- On success it classifies the descriptor:
  - The literal paths `/dev/random` and `/dev/urandom` give `FdType::Rng`, so later reads return Detcore's seeded bytes (`detcore/src/syscalls/files.rs:1013-1019`). The container bind-mounts the host devices only so the open finds an inode; the comment at `hermit-cli/src/bin/hermit/container.rs:516-522` says their determinism rests on this classification.
  - A reopen of a pipe this process already holds as a Detcore-managed pipe (`/dev/stdin`, `/proc/self/fd/N`, `/dev/fd/63`) becomes `FdType::Pipe` with a hidden physical `O_NONBLOCK`, so a read that waits for a writer waits in the scheduler (`detcore/src/syscalls/files.rs:1020-1049`; https://github.com/rrnewton/hermit/issues/1850). This does not apply to `O_PATH` opens or in record and replay modes.
  - Procfs and sysfs files are classified from the observed path first, and from the descriptor's resolved path only if that gives nothing (`detcore/src/syscalls/files.rs:1068-1072`). Thread-identity files bind the opening thread (`detcore/src/syscalls/files.rs:1073-1087`), and `timerslack_ns` files bind the target task (`detcore/src/syscalls/files.rs:1088-1119`). Later reads serve these from Detcore state.
- `add_fd` records the descriptor, its flags and type (`detcore/src/syscalls/files.rs:916`). With metadata virtualization on, it first takes the file's real `fstat` so later stat calls can map the inode to a virtual one. If that fails, it closes the new descriptor and returns the error (`detcore/src/syscalls/files.rs:923-945`).

Why `model`: `passthrough` would leave procfs content, random devices and pipe reopens to the host, and the last of these deadlocked the scheduler before https://github.com/rrnewton/hermit/issues/1850. `emulate` would mean reimplementing path resolution and permission checks over a real filesystem. `refuse` is impossible: almost every program opens files through this call.

Gaps against `model`, by reading the code:

- The random-device check compares the literal `pathname` (`detcore/src/syscalls/files.rs:1013-1019`), not `observed_path`. `openat(dirfd_of_dev, "urandom")`, `openat(AT_FDCWD, "urandom")` after `chdir("/dev")`, `/dev/./urandom` and a symlink to `/dev/urandom` all give `FdType::Regular`, so later reads return host random bytes. Already filed as https://github.com/rrnewton/hermit/issues/3158 (open; it reports the dotted, `..` and symlink spellings under `--strict`). Measured 2026-10-03 by syscall-tracking (not a test): two `hermit run` runs of a probe through bin/safehermit, with a hermit binary built from a2b1deecaa21ca60c2b981c07e319fd97ee6cb44, whose check at files.rs:1013-1014 is the same as at f4055eb9 and at main 25c3a860. The literal `/dev/urandom` read `2972bb044d96df28` both times; `/dev/./urandom`, `openat(dirfd_of_dev, "urandom")` and `chdir("/dev"); open("urandom")` each read different bytes in the two runs. Probe and outputs: ignored/syscall-tracking/evidence/urandom-20261003/.
- No FIFO handling: a blocking FIFO open (F6 without `O_NONBLOCK`) waits inside the kernel, not in the scheduler, so a peer opened by another guest thread cannot be scheduled deterministically and may deadlock. No test at f4055eb9cae4 opens a FIFO under Hermit.
- The comment at `detcore/src/syscalls/files.rs:1129` marks error results as not audited for nondeterminism. Errors that depend on host state (`ENOSPC`, `EACCES` on host-owned paths) pass through.
- Inode and device numbers of newly opened files are virtualized only when later read through the stat family; the newfstatat and fstat bodies cover that.

## Recommended tests

- T1 (covers F1, F10, F16): open one file read-only, write-only, read-write and with `O_PATH`, plus `O_LARGEFILE | O_ASYNC` and an unknown flag bit. Check which reads and writes succeed, what `fcntl(F_GETFL)` reports, and that results are identical across `hermit run --verify` on ptrace.
  Existing partial candidate: `tests/c/openat_flags.c` (cell `c-programs/openat-flags`, verify ci on ptrace) checks `EBADF` for the wrong access mode. `tests/c/simple/pread64_nostdlib.c:45` (cell `c-programs/pread64-nostdlib`, verify ci on ptrace) issues a raw `SYS_openat` with `O_RDONLY`.
- T2 (covers F2, F3, F4, F5): create a file with `O_CREAT` under two umask values and check the mode. Check that `O_CREAT | O_EXCL` on an existing path and on a dangling symlink gives `EEXIST`, that `O_TRUNC` empties the file and changes mtime, and that `O_APPEND` writes land at end of file.
  Existing partial candidates: `tests/c/openat_flags.c` (`EEXIST`, `O_TRUNC`) and `tests/c/umask_mode.c` (cell `c-programs/umask-mode`, verify ci on ptrace).
- T3 (covers F6, F12): `mkfifo`, then open it with `O_RDONLY | O_NONBLOCK` (succeeds) and `O_WRONLY | O_NONBLOCK` with no reader (`ENXIO`). Then open it blocking from two guest threads, one reader and one writer, and require both opens to complete with identical output across verify runs. Open a terminal with `O_NOCTTY` when one is available, and skip otherwise.
- T4 (covers F7): open with and without `O_CLOEXEC`, then `execve` a helper that reports which descriptors are still open.
  Existing partial candidate: `tests/c/record_replay_fd_close.c` (cell `c-programs/record-replay-fd-close`, verify ci on ptrace) checks close-on-exec across `execve`.
- T5 (covers F8, F9, F11): `O_DIRECTORY` on a regular file gives `ENOTDIR`, `O_NOFOLLOW` on a symlink gives `ELOOP`, `O_NOFOLLOW | O_PATH` on a symlink succeeds, and `O_TMPFILE | O_RDWR` on a directory gives an unnamed file whose later `fstat` is identical across runs.
  Existing partial candidate: `tests/c/o_tmpfile_anon.c` (cell `c-programs/o-tmpfile-anon`, verify ci on ptrace).
- T6 (covers F13, F14, F15): open with `O_SYNC`, `O_DSYNC`, `O_DIRECT` (accept `EINVAL` where the filesystem refuses it, and require the same result in both runs), and `O_NOATIME` on a file the guest owns.
- T7 (covers F17): close descriptors 3 and 5, then open twice and check the results are 3 then 5. With `RLIMIT_NOFILE` lowered, check `EMFILE`.
  Existing candidate for the open itself on an awkward stack: `detcore/tests/misc/tight_stack_openat.rs`, test `openat_succeeds_without_writable_stack_below_rsp` (target `tests_misc`), regression for https://github.com/rrnewton/hermit/issues/3328, compares the descriptor and metadata with an ordinary open.
- T8 (covers F18): open `/dev/urandom` directly, through `openat(dirfd_of_dev, "urandom")`, after `chdir("/dev")` as `"urandom"`, as `/dev/./urandom`, and through a symlink, and read 16 bytes from each. The bytes must be identical across verify runs; the non-literal spellings test the gap named above. Open `/proc/self/status` and `/dev/stdin` on a pipe and read them.
  Existing partial candidate: `tests/c/random_sources.c` (cell `c-programs/random-sources`, verify ci on ptrace) opens the literal `/dev/urandom` and `/dev/random`.
- T9 (covers F19): open a relative name through `AT_FDCWD`, through a directory descriptor, through an `O_PATH | O_DIRECTORY` descriptor, and with an absolute path and a bogus `dirfd`. A relative name with a closed `dirfd` gives `EBADF`, and with a regular-file `dirfd` gives `ENOTDIR`.
- T10 (covers F20): a null path (`EFAULT`), a missing path (`ENOENT`), an empty path (`ENOENT`), a path longer than `PATH_MAX` (`ENAMETOOLONG`), a directory opened for writing (`EISDIR`), a file without permission as a non-root virtual user (`EACCES`), and `O_TMPFILE` read-only (`EINVAL`).
  Existing partial candidate: `tests/c/errno_path_identity.c` (cell `system-utils/errno-path-identity`, verify ci on ptrace).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_openat lets the kernel open, then classifies random devices, Detcore pipe reopens and procfs files and tracks the descriptor); linux_determinism stays conditional (repeats given the same filesystem state, cwd and descriptor table); 20 flag behaviours and 10 tests listed; gaps noted: the random-device check compares the literal path, blocking FIFO opens wait in the kernel, errors unaudited; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `openat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+openat&body=Discussion+for+the+%60openat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fopenat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60openat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `openat`: the 2026-07-24 classification campaign filed issues only for syscalls that Detcore did not yet handle, and Detcore already handled this one.

The test file `tests/compat/lsof_mount_redirect.c` names `openat` only because it wraps the libc `openat` through `LD_PRELOAD` (`tests/compat/lsof_mount_redirect.c:201`); it does not test Hermit's handling of the syscall.
