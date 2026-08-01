# e9patch preprocessing parity corpus

Freestanding, statically linked, raw-`syscall` x86-64 guests used by
`../e9patch_corpus.py` to ratchet e9patch preprocessing parity against the
golden ptrace backend.

These guests are deliberately freestanding (`-nostdlib -static -ffreestanding`)
rather than ordinary libc programs. e9tool rewrites only the *main* executable,
so a dynamically linked libc binary exposes zero `SYSCALL` sites in its own ELF
(they live in `libc.so`) and e9patch preprocessing is a no-op
(`candidate_sites=0`). A freestanding guest emits its `syscall` instructions in
the main ELF, so e9patch actually rewrites it (`candidate_sites > 0`). Every
guest ends in `exit_group` (231); a bare `exit` (60) would exit only the calling
thread and hang the run.

| Guest | Exercises |
| --- | --- |
| `minimal_exit` | single site: `exit_group` only |
| `write_stdout` | `write(1, ...)` then exit |
| `getpid_check` | virtualized `getpid` |
| `clock_gettime` | `clock_gettime(CLOCK_MONOTONIC)` |
| `nanosleep` | `nanosleep` |
| `getrandom` | determinized `getrandom` stream |
| `multi_site` | three distinct `noinline` syscall sites (write/getpid/exit) |
| `loop_write` | one site invoked eight times in a loop |
| `mmap_anon` | anonymous `mmap`, touch, `munmap` |
| `uname` | `uname` |
| `sigmask` | `gettid` + `rt_sigprocmask` |
| `compute` | CPU-bound loop (RCB preemption) then exit |
| `fd_open_number` | first guest-opened fd is the lowest free number (3) |
| `fd_lowest_free` | closed fd number is reused by the next `open` |
| `pipe_fds` | `pipe2` allocates the two lowest free descriptors |
| `dup3_high` | `dup3` honors a caller-chosen high fd (10) |
| `writev_multi` | three-iovec gathered `write` output |
| `fcntl_cloexec` | `F_GETFD` reports no stray `FD_CLOEXEC` |
| `proc_self_fd_count` | `/proc/self/fd` count parity (no leaked loader fd) |
| `readlink_exe` | `/proc/self/exe` resolves to the original guest, not the e9 temp |
| `read_devzero` | content I/O: `read(/dev/zero, 16)` yields 16 zero bytes |
| `read_devnull_eof` | content I/O: `read(/dev/null)` returns 0 (EOF) |
| `fstat_devnull` | stat mode bits: `fstat(/dev/null)` reports `S_IFCHR` |
| `lseek_pipe` | seek error path: `lseek` on a pipe fails `ESPIPE` (-29) |
| `write_badfd` | fd error path: `write` to a bad fd fails `EBADF` (-9) |
| `mprotect_roundtrip` | memory protection: `mmap`/`mprotect` RW→RO→RW/`munmap` |
| `getid_identity` | credentials: `getuid`/`geteuid`/`getgid`/`getegid` |
| `getgroups_identity` | credentials: `getgroups` supplementary-group count |
| `brk_grow` | heap/brk: query the break then grow it by a page |
| `madvise_dontneed` | memory advice: `MADV_DONTNEED` zero-fills an anon page |
| `file_mmap_zero` | file-backed mmap: `MAP_PRIVATE` of `/dev/zero` reads 0 |
| `memfd_seek` | anon-file seek: `memfd_create`/`ftruncate`/`SEEK_END` = 4096 |
| `ioctl_enotty` | ioctl error path: `TCGETS` on `/dev/null` fails `ENOTTY` |
| `access_devnull` | fs access: `access(/dev/null, R_OK)` succeeds |
| `sigaction_query` | signal disposition: `rt_sigaction` NULL-act query |
| `getppid_check` | process hierarchy: virtualized `getppid` |
| `open_enoent` | fs errno: `open` of a missing path fails `ENOENT` |
| `prctl_name` | prctl name: `PR_SET_NAME`/`PR_GET_NAME` round-trip |
| `getcwd_check` | cwd: `getcwd` (parity-only, host-specific path) |
| `pipe_rw` | pipe data I/O: two bytes written and read back |
| `statx_devnull` | statx mode bits: `statx(/dev/null)` reports `S_IFCHR` |
| `readv_zero` | scatter read: `readv` of `/dev/zero` into two iovecs |
| `umask_set` | umask: `umask` round-trips the set value (022) |
| `fstat_size_memfd` | stat size: `fstat` reports a memfd's `st_size` |
| `newfstatat_devnull` | fstatat: `newfstatat(/dev/null)` reports `S_IFCHR` |
| `faccessat_devnull` | faccessat: `faccessat(/dev/null, R_OK)` succeeds |
| `sendfile_memfd` | sendfile: zero-copy five bytes between two memfds |
| `pwrite_pread_memfd` | positioned I/O: `pwrite64`/`pread64` at offset 10 |
| `eventfd_rw` | eventfd: counter written and read back |
| `rt_sigpending_empty` | signal pending: `rt_sigpending` empty set |
| `fchmod_memfd` | fchmod: memfd mode set to `0644` and read back |
| `dup2_high` | `dup2` honors a caller-chosen high fd (20) |
| `chdir_root` | `chdir("/")` then `getcwd` = `/` (host-independent) |
| `fchdir_root` | `fchdir` into a root dir fd then `getcwd` = `/` |
| `fsync_memfd` | `fsync` of a memfd returns 0 |
| `socketpair_rw` | `AF_UNIX` `socketpair` two-byte round-trip |
| `flock_memfd` | `flock` `LOCK_EX` then `LOCK_UN` both succeed |
| `pipe_nonblock_eagain` | nonblocking read of an empty pipe fails `EAGAIN` (-11) |
| `getpgid_check` | `getpgid(0)` (parity-only, host-specific pgid) |
| `dup_lowest` | `dup` lands the copy on the lowest free fd |
| `fcntl_dupfd` | `fcntl` `F_DUPFD` honors a minimum fd of 20 |
| `fcntl_getfl` | `fcntl` `F_GETFL` reports the `O_RDONLY` access mode |
| `lseek_seekcur_memfd` | `SEEK_SET` then `SEEK_CUR` compose to offset 5 |
| `pread_past_eof` | `pread64` past EOF returns 0 |
| `readlinkat_exe` | `readlinkat` `/proc/self/exe` (parity-only, host path) |
| `getresuid_check` | `getresuid` real uid (parity-only, host uid) |
| `prlimit_nofile` | `prlimit64` `RLIMIT_NOFILE` (parity-only, host limit) |
| `preadv_memfd` | `preadv` positioned vector read at offset 2 |
| `pwritev_memfd` | `pwritev` positioned vector write then `pread` back |
| `fcntl_setfl_nonblock` | `fcntl` `F_SETFL` sets `O_NONBLOCK` |
| `fcntl_dupfd_cloexec` | `fcntl` `F_DUPFD_CLOEXEC` sets `FD_CLOEXEC` on the copy |
| `mremap_grow` | `mremap` `MREMAP_MAYMOVE` grows an anon mapping |
| `sendmsg_socketpair` | `sendmsg`/`recvmsg` over an `AF_UNIX` socketpair |
| `getsid_check` | `getsid(0)` (parity-only, host-specific sid) |
| `getpgrp_check` | `getpgrp` (parity-only, host-specific pgrp) |

The last eight guests are the **round-2 fd/output-hygiene ratchet batch**
(non-time, non-gated): they establish that e9patch preprocessing perturbs no
descriptor allocation or process-metadata output. `proc_self_fd_count` and
`readlink_exe` emit environment-dependent values, so the driver asserts
golden==e9patch parity for them (`expected_stdout=None`) rather than a fixed
string; the other six pin exact stdout. All remain freestanding
(`candidate_sites > 0`) so e9patch actually rewrites them.

The final eight guests are the **round-3 new-family ratchet batch** (non-time,
non-gated): they widen coverage beyond fd/output hygiene into content I/O
(`read_devzero`, `read_devnull_eof`), stat mode bits (`fstat_devnull`), memory
protection (`mprotect_roundtrip`), errno paths (`lseek_pipe` `ESPIPE`,
`write_badfd` `EBADF`), and credential syscalls (`getid_identity`,
`getgroups_identity`). These establish that e9patch preprocessing leaves
`read`/`fstat`/`mmap`/`mprotect`/`lseek`/`write` and the credential syscalls
byte-identical to golden ptrace. `getid_identity` and `getgroups_identity` emit
host-specific absolute uid/gid/group-count values, so the driver asserts
golden==e9patch parity only (`expected_stdout=None`); the other six pin exact
deterministic stdout. All remain freestanding (`candidate_sites > 0`).

The final eight guests are the **round-4 new-family ratchet batch** (non-time,
non-gated): heap/brk (`brk_grow`), memory advice (`madvise_dontneed`),
file-backed mmap (`file_mmap_zero`), anonymous-file seek positioning
(`memfd_seek`, via `memfd_create`/`ftruncate`/`SEEK_END`, since a device's
`lseek` is a no-op), ioctl error paths (`ioctl_enotty` `TCGETS`→`ENOTTY`),
filesystem access checks (`access_devnull`), signal-disposition queries
(`sigaction_query`, a NULL-act `rt_sigaction` read with no delivery or
scheduling), and process hierarchy (`getppid_check`). These establish that
e9patch preprocessing leaves `brk`/`madvise`/file-backed `mmap`/`memfd_create`/
`ftruncate`/`lseek`/`ioctl`/`access`/`rt_sigaction`/`getppid` byte-identical to
golden ptrace. `getppid_check` emits a host-specific virtualized parent pid, so
the driver asserts golden==e9patch parity only (`expected_stdout=None`); the
other seven pin exact deterministic stdout. All remain freestanding
(`candidate_sites > 0`).

The final eight guests are the **round-5 new-family ratchet batch** (non-time,
non-gated): filesystem errno paths (`open_enoent` `ENOENT`), prctl thread-name
round-trips (`prctl_name`), cwd queries (`getcwd_check`), pipe data I/O
(`pipe_rw`), statx mode bits (`statx_devnull`), scatter reads (`readv_zero`),
umask round-trips (`umask_set`), and fstat size reporting (`fstat_size_memfd`).
These establish that e9patch preprocessing leaves `open`(error)/`prctl`/
`getcwd`/pipe `read`+`write`/`statx`/`readv`/`umask`/`fstat`(size)
byte-identical to golden ptrace. `getcwd_check` emits a host-specific working
directory, so the driver asserts golden==e9patch parity only
(`expected_stdout=None`); the other seven pin exact deterministic stdout. All
remain freestanding (`candidate_sites > 0`).

The final seven guests are the **round-6 new-family ratchet batch** (non-time,
non-gated): `*at`-suffixed stat/access syscalls (`newfstatat_devnull`,
`faccessat_devnull`), the `sendfile` zero-copy transfer (`sendfile_memfd`),
positioned I/O (`pwrite_pread_memfd`, `pwrite64`/`pread64` at a fixed offset),
eventfd counters (`eventfd_rw`), signal-pending queries (`rt_sigpending_empty`),
and `fchmod` (`fchmod_memfd`). These establish that e9patch preprocessing leaves
`newfstatat`/`faccessat`/`sendfile`/`pwrite64`/`pread64`/`eventfd2`/
`rt_sigpending`/`fchmod` byte-identical to golden ptrace, all pinning exact
deterministic stdout. `copy_file_range` was intentionally *not* added: hermit
returns `-ENOSYS` for it, so it exercises no working feature and would only
encode a hermit limitation. All remain freestanding (`candidate_sites > 0`).

The final eight guests are the **round-7 new-family ratchet batch** (non-time,
non-gated): `dup2` fd placement (`dup2_high`), working-directory syscalls tested
against the host-independent root (`chdir_root`, `fchdir_root`, both reading
back `getcwd` = `/` rather than a host-specific path), `fsync` (`fsync_memfd`),
`AF_UNIX` `socketpair` data transfer (`socketpair_rw`), `flock` (`flock_memfd`),
the nonblocking empty-pipe errno path (`pipe_nonblock_eagain` `EAGAIN`), and
`getpgid` (`getpgid_check`). These establish that e9patch preprocessing leaves
`dup2`/`chdir`/`fchdir`/`getcwd`/`fsync`/`socketpair`/`flock`/nonblocking
`read`/`getpgid` byte-identical to golden ptrace. `getpgid_check` emits a
host-specific process-group id, so the driver asserts golden==e9patch parity
only (`expected_stdout=None`); the other seven pin exact deterministic stdout.
All remain freestanding (`candidate_sites > 0`).

The final eight guests are the **round-8 new-family ratchet batch** (non-time,
non-gated): `dup` fd allocation (`dup_lowest`), `fcntl` `F_DUPFD`/`F_GETFL`
(`fcntl_dupfd`, `fcntl_getfl`), `lseek` `SEEK_CUR` composition
(`lseek_seekcur_memfd`), `pread64` past EOF (`pread_past_eof`), `readlinkat`
(`readlinkat_exe`), `getresuid` (`getresuid_check`), and `prlimit64`
(`prlimit_nofile`). These establish that e9patch preprocessing leaves
`dup`/`fcntl`/`lseek`/`pread64`/`readlinkat`/`getresuid`/`prlimit64`
byte-identical to golden ptrace. `readlinkat_exe`, `getresuid_check`, and
`prlimit_nofile` emit host-specific values (the exe path, the real uid, and the
`RLIMIT_NOFILE` soft limit), so the driver asserts golden==e9patch parity only
(`expected_stdout=None`); the other five pin exact deterministic stdout. All
remain freestanding (`candidate_sites > 0`).

The final eight guests are the **round-9 new-family ratchet batch** (non-time,
non-gated): positioned vector I/O (`preadv_memfd`, `pwritev_memfd`), `fcntl`
`F_SETFL` (`fcntl_setfl_nonblock`) and `F_DUPFD_CLOEXEC` (`fcntl_dupfd_cloexec`),
`mremap` growth (`mremap_grow`), `sendmsg`/`recvmsg` over a socketpair
(`sendmsg_socketpair`), and the `getsid`/`getpgrp` process-group queries
(`getsid_check`, `getpgrp_check`). These establish that e9patch preprocessing
leaves `preadv`/`pwritev`/`fcntl`/`mremap`/`sendmsg`/`recvmsg`/`getsid`/`getpgrp`
byte-identical to golden ptrace. `getsid_check` and `getpgrp_check` emit
host-specific session/process-group ids, so the driver asserts golden==e9patch
parity only (`expected_stdout=None`); the other six pin exact deterministic
stdout. All remain freestanding (`candidate_sites > 0`).

Regenerate identical sources with the parent workspace generator at
`experiments/e9patch_ptrace_corpus_parity_20260731/src/gen_corpus.sh`.
