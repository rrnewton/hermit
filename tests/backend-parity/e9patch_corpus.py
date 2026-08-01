#!/usr/bin/env python3
"""Ratchet e9patch preprocessing parity against the golden ptrace backend.

e9patch is binary-rewriting *preprocessing* for the ptrace backend, not a
standalone Detcore backend: e9tool rewrites the guest ELF ahead of time to
pre-trap its `SYSCALL` sites, then Detcore runs the rewritten image under
ptrace. e9tool only rewrites the *main* executable, so a dynamically linked
libc program exposes zero `SYSCALL` sites in its own ELF (they live in
`libc.so`/`ld-linux`) and e9patch preprocessing becomes a no-op
(`candidate_sites=0`). The shared `run_matrix.py` guests are all dynamic libc
binaries and therefore never exercise the rewrite path. This harness instead
uses a freestanding, statically linked, raw-`syscall` corpus (x86-64) whose
`SYSCALL` sites live in the main ELF, so `candidate_sites > 0` and e9patch
actually rewrites the guest.

For each guest we compare the golden plain-ptrace run against the e9patch
preprocessing + ptrace run and enforce, per guest:

  * exit-status parity              (golden exit == e9patch exit),
  * stdout parity                   (captured from a plain --strict run;
                                      --verify diverts guest stdout for its own
                                      log comparison),
  * golden L2                       (hermit run --strict --verify verifies),
  * e9patch L2                      (hermit --backend e9patch run --strict
                                      --verify verifies),
  * full direct-AOT coverage        (mapped_sites == candidate_sites > 0),
  * no signal fallback              (b0_sites == 0; a nonzero B0 would reserve
                                      SIGILL and change guest signal semantics),
  * guest-syscall DETLOG tail-match (the golden guest-syscall sequence equals
                                      the suffix of the e9patch sequence; the
                                      removed prefix is the deterministic
                                      e9loader prologue).

Byte-identical DETLOG parity to plain ptrace is impossible by construction: the
e9patch-rewritten image carries an e9loader stub that runs a fixed, deterministic
startup prologue (readlink /proc/self/exe, open(self), arch_prctl GET/SET_FS,
N * mmap of trampoline pages, close) before the guest's own `_start`. That
prologue is a pure prefix; the achievable and enforced parity is guest-syscall
DETLOG identity *modulo* that deterministic prologue (tail-match), plus L2 and
guest-visible parity. This harness makes no claim of strict detlog identity.

Prerequisites (absent in CI, hence BLOCKED there, mirroring the KVM /dev/kvm
gate in run_matrix.py):
  * a hermit built with the `e9patch` cargo feature
    (`cargo build -p hermit --features e9patch`);
  * HERMIT_E9TOOL and HERMIT_E9PATCH_BACKEND pointing at a built e9tool/e9patch
    pair (the reverie checkout vendors them under
    `third-party/e9patch/{e9tool,e9patch}`);
  * an x86-64 host with `cc`.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPOSITORY = SCRIPT_DIR.parent.parent
CORPUS_DIR = SCRIPT_DIR / "e9patch_corpus"

# name -> (expected_exit, expected_stdout or None). stdout is exact when given.
CORPUS: dict[str, tuple[int, bytes | None]] = {
    "minimal_exit": (0, b""),
    "write_stdout": (0, b"corpus-write\n"),
    "getpid_check": (0, b""),
    "clock_gettime": (0, b""),
    "nanosleep": (0, b""),
    "getrandom": (0, b""),
    "multi_site": (0, b"multi\n"),
    "loop_write": (0, b"xxxxxxxx\n"),
    "mmap_anon": (0, b""),
    "uname": (0, b""),
    "sigmask": (0, b""),
    "compute": (0, None),
    # Round-2 fd/output-hygiene ratchet batch (non-time, non-gated). These probe
    # descriptor allocation, reuse, and process-metadata output; e9patch
    # preprocessing must not perturb any of it (the e9loader closes its self-fd
    # and leaves /proc/self/exe pointing at the original guest binary). The two
    # environment-dependent guests assert golden==e9patch parity only (None).
    "fd_open_number": (0, b"fd=3\n"),
    "fd_lowest_free": (0, b"a=3\nb=4\nc=3\n"),
    "pipe_fds": (0, b"r=0\nrd=3\nwr=4\n"),
    "dup3_high": (0, b"dup3=10\nviaten\n"),
    "writev_multi": (0, b"ABC\nwrote=4\n"),
    "fcntl_cloexec": (0, b"stdout_fd_flags=0\nopened_fd=3\nopened_flags=0\n"),
    "proc_self_fd_count": (0, None),
    "readlink_exe": (0, None),
    # Round-3 new-family ratchet batch (non-time, non-gated). Widens coverage
    # beyond fd/output hygiene into content I/O, stat mode bits, memory
    # protection, errno paths, and credential syscalls -- families e9patch
    # preprocessing must leave byte-identical to golden ptrace. The identity
    # guests emit host-specific absolute values, so they assert golden==e9patch
    # parity only (None); the rest pin exact deterministic stdout.
    "read_devzero": (0, b"zeros=16\n"),
    "read_devnull_eof": (0, b"eof=0\n"),
    "fstat_devnull": (0, b"chardev=1\n"),
    "lseek_pipe": (0, b"espipe=-29\n"),
    "write_badfd": (0, b"ebadf=-9\n"),
    "mprotect_roundtrip": (0, b"mprotect=ok\n"),
    "getid_identity": (0, None),
    "getgroups_identity": (0, None),
    # Round-4 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into heap/brk, memory advice, file-backed mmap, anonymous-file seek
    # positioning, ioctl error paths, filesystem access checks, signal-
    # disposition queries, and process hierarchy -- all families e9patch
    # preprocessing must leave byte-identical to golden ptrace. getppid emits a
    # host-specific virtualized pid, so it asserts golden==e9patch parity only.
    "brk_grow": (0, b"brk=ok\n"),
    "madvise_dontneed": (0, b"madvise=ok\n"),
    "file_mmap_zero": (0, b"mzero=1\n"),
    "memfd_seek": (0, b"end=4096\n"),
    "ioctl_enotty": (0, b"enotty=-25\n"),
    "access_devnull": (0, b"access=0\n"),
    "sigaction_query": (0, b"sigaction=ok\n"),
    "getppid_check": (0, None),
    # Round-5 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into filesystem errno paths, prctl thread-name round-trips, cwd queries,
    # pipe data I/O, statx mode bits, scatter reads, umask round-trips, and
    # fstat size reporting -- all families e9patch preprocessing must leave
    # byte-identical to golden ptrace. getcwd emits a host-specific path, so it
    # asserts golden==e9patch parity only.
    "open_enoent": (0, b"enoent=-2\n"),
    "prctl_name": (0, b"name=cg\n"),
    "getcwd_check": (0, None),
    "pipe_rw": (0, b"pipe=hi\n"),
    "statx_devnull": (0, b"statx_chr=1\n"),
    "readv_zero": (0, b"readv=16\n"),
    "umask_set": (0, b"umask=18\n"),
    "fstat_size_memfd": (0, b"size=5\n"),
    # Round-6 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into *at-suffixed stat/access syscalls, the sendfile zero-copy transfer,
    # positioned I/O (pwrite64/pread64), eventfd counters, signal-pending
    # queries, and fchmod -- all families e9patch preprocessing must leave
    # byte-identical to golden ptrace. (copy_file_range was intentionally not
    # added: hermit returns -ENOSYS for it, so it exercises no working feature.)
    "newfstatat_devnull": (0, b"fstatat_chr=1\n"),
    "faccessat_devnull": (0, b"faccessat=0\n"),
    "sendfile_memfd": (0, b"sent=5\n"),
    "pwrite_pread_memfd": (0, b"pread=abc\n"),
    "eventfd_rw": (0, b"eventfd=5\n"),
    "rt_sigpending_empty": (0, b"pending=0\n"),
    "fchmod_memfd": (0, b"chmod=ok\n"),
    # Round-7 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into dup2 fd placement, chdir/fchdir + getcwd against the host-independent
    # root, fsync, AF_UNIX socketpair data transfer, flock, the nonblocking
    # empty-pipe errno path, and getpgid -- all families e9patch preprocessing
    # must leave byte-identical to golden ptrace. getpgid emits a host-specific
    # process-group id, so it asserts golden==e9patch parity only.
    "dup2_high": (0, b"dup2=20\n"),
    "chdir_root": (0, b"cwd=/\n"),
    "fchdir_root": (0, b"cwd=/\n"),
    "fsync_memfd": (0, b"fsync=ok\n"),
    "socketpair_rw": (0, b"sp=hi\n"),
    "flock_memfd": (0, b"flock=ok\n"),
    "pipe_nonblock_eagain": (0, b"eagain=-11\n"),
    "getpgid_check": (0, None),
    # Round-8 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into dup fd allocation, fcntl F_DUPFD/F_GETFL, lseek SEEK_CUR composition,
    # pread past EOF, readlinkat, getresuid, and prlimit64 -- all families
    # e9patch preprocessing must leave byte-identical to golden ptrace. The
    # readlinkat/getresuid/prlimit64 guests emit host-specific values (exe path,
    # uid, and RLIMIT_NOFILE), so they assert golden==e9patch parity only.
    "dup_lowest": (0, b"dup=4\n"),
    "fcntl_dupfd": (0, b"dupfd=20\n"),
    "fcntl_getfl": (0, b"getfl=0\n"),
    "lseek_seekcur_memfd": (0, b"cur=5\n"),
    "pread_past_eof": (0, b"eof=0\n"),
    "readlinkat_exe": (0, None),
    "getresuid_check": (0, None),
    "prlimit_nofile": (0, None),
    # Round-9 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into positioned vector I/O (preadv/pwritev), fcntl F_SETFL and
    # F_DUPFD_CLOEXEC, mremap growth, socketpair sendmsg/recvmsg, and the
    # getsid/getpgrp process-group queries -- all families e9patch preprocessing
    # must leave byte-identical to golden ptrace. getsid/getpgrp emit
    # host-specific ids, so they assert golden==e9patch parity only.
    "preadv_memfd": (0, b"preadv=cdef\n"),
    "pwritev_memfd": (0, b"pwritev=hiyo\n"),
    "fcntl_setfl_nonblock": (0, b"nonblock=1\n"),
    "fcntl_dupfd_cloexec": (0, b"cloexec=1\n"),
    "mremap_grow": (0, b"mremap=ok\n"),
    "sendmsg_socketpair": (0, b"msg=hi\n"),
    "getsid_check": (0, None),
    "getpgrp_check": (0, None),
    # Round-10 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into socketpair sendto/recvfrom, getsockname/getpeername address copyout,
    # fallocate size-extension on a memfd, fdatasync, mincore residency query,
    # fadvise64 hints, and sysinfo -- all families e9patch preprocessing must
    # leave byte-identical to golden ptrace. getsockname/getpeername report the
    # AF_UNIX family constant (1); fdatasync/mincore/fadvise/sysinfo print the
    # syscall return (0 on success), so every value is host-independent.
    "sendto_socketpair": (0, b"sf=hi\n"),
    "getsockname_unix": (0, b"sockname=1\n"),
    "getpeername_unix": (0, b"peername=1\n"),
    "fallocate_memfd": (0, b"falloc=8\n"),
    "fdatasync_memfd": (0, b"fdatasync=0\n"),
    "mincore_resident": (0, b"mincore=0\n"),
    "fadvise_memfd": (0, b"fadvise=0\n"),
    "sysinfo_ok": (0, b"sysinfo=0\n"),
    # Round-11 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into signal-mask/altstack queries (no delivery, no scheduling), epoll fd
    # registration (no wait), memfd fcntl sealing, uname, prctl PR_GET_DUMPABLE,
    # capget, and fstatfs -- all families e9patch preprocessing must leave
    # byte-identical to golden ptrace. Every printed value is host-independent:
    # constants (uname sysname "Linux", SS_DISABLE=2, F_SEAL_SEAL=1,
    # PR_GET_DUMPABLE=1) or the syscall return (0 on success). splice was
    # evaluated and DROPPED: hermit returns -ENOSYS (golden itself prints
    # "splice=-38"), so it would encode a hermit limitation, not a parity claim
    # (no false-parity), exactly like round-6's copy_file_range.
    "rt_sigprocmask_query": (0, b"sigprocmask=0\n"),
    "sigaltstack_query": (0, b"altstack=2\n"),
    "epoll_ctl_add": (0, b"epoll=0\n"),
    "memfd_seal": (0, b"seals=1\n"),
    "uname_sysname": (0, b"uname=Linux\n"),
    "prctl_dumpable": (0, b"dumpable=1\n"),
    "capget_ok": (0, b"capget=0\n"),
    "fstatfs_memfd": (0, b"fstatfs=0\n"),
    # Round-12 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into path-based stat/lstat on /dev/null, openat(AT_FDCWD) read-to-EOF,
    # memory locking (mlock/mlock2/munlock), msync(MS_SYNC) on an anonymous
    # mapping, inotify watch registration (no event wait), and readahead over a
    # memfd -- all families e9patch preprocessing must leave byte-identical to
    # golden ptrace. Every printed value is host-independent: constants (S_IFCHR
    # file-type test => 1, first inotify wd => 1) or the syscall return (0 on
    # success). All eight ran clean under golden ptrace (no -ENOSYS drop).
    "stat_devnull": (0, b"stat_chr=1\n"),
    "lstat_devnull": (0, b"lstat_chr=1\n"),
    "openat_devnull": (0, b"openat=0\n"),
    "mlock_page": (0, b"mlock=0\n"),
    "mlock2_page": (0, b"mlock2=0\n"),
    "msync_anon": (0, b"msync=0\n"),
    "inotify_watch_root": (0, b"inotify=1\n"),
    "readahead_memfd": (0, b"readahead=0\n"),
    # Round-13 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into path-based statfs on "/", the legacy pipe(2) syscall, directory
    # enumeration (getdents64, reduced to a non-empty boolean since contents are
    # host-specific), signalfd registration (no signal delivered or read),
    # close_range fd teardown, socket-option get/set on a socketpair
    # (getsockopt SO_TYPE, setsockopt SO_REUSEADDR), and fcntl F_GETLK record-lock
    # querying on a memfd -- all families e9patch preprocessing must leave
    # byte-identical to golden ptrace. Every printed value is host-independent:
    # constants (SOCK_STREAM=1, F_UNLCK=2, lowest pipe fd=3, non-empty getdents
    # boolean, valid-fd boolean) or the syscall return (0 on success). All eight
    # ran clean under golden ptrace (no -ENOSYS drop).
    "statfs_root": (0, b"statfs=0\n"),
    "pipe_legacy": (0, b"pipe=3\n"),
    "getdents_root": (0, b"getdents=1\n"),
    "signalfd_create": (0, b"signalfd=1\n"),
    "close_range_high": (0, b"close_range=0\n"),
    "getsockopt_socktype": (0, b"socktype=1\n"),
    "setsockopt_reuseaddr": (0, b"setsockopt=0\n"),
    "fcntl_getlk": (0, b"getlk=2\n"),
    # Round-14 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into a lone AF_UNIX SOCK_DGRAM socket (distinct from socketpair), socket
    # half-close (shutdown), robust-futex-list registration/query
    # (set_robust_list/get_robust_list, registration only -- no futex contended),
    # MADV_WILLNEED advice (distinct from MADV_DONTNEED), shared anonymous mmap
    # (MAP_SHARED flag path, distinct from MAP_PRIVATE), prctl PR_GET_KEEPCAPS
    # (distinct op), and arch_prctl ARCH_GET_FS -- all families e9patch
    # preprocessing must leave byte-identical to golden ptrace. Every printed
    # value is host-independent: the lowest free fd (3), the PR_GET_KEEPCAPS
    # default (0), or the syscall return (0 on success); the FS base and robust
    # head pointer are read but not printed. All eight ran clean under golden
    # ptrace (no -ENOSYS drop).
    "socket_dgram": (0, b"socket=3\n"),
    "shutdown_socketpair": (0, b"shutdown=0\n"),
    "set_robust_list_ok": (0, b"robust=0\n"),
    "get_robust_list_ok": (0, b"getrobust=0\n"),
    "madvise_willneed": (0, b"willneed=0\n"),
    "mmap_shared_anon": (0, b"shmap=0\n"),
    "prctl_keepcaps": (0, b"keepcaps=0\n"),
    "arch_prctl_getfs": (0, b"getfs=0\n"),
    # Round-15 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into filesystem/memory flush (syncfs on a memfd), memory-ordering
    # (membarrier CMD_GLOBAL), the getcpu query, the LEGACY getdents(78)
    # (distinct syscall number from round-13's getdents64), the execution-domain
    # persona query (personality), an advisory write-lock set/release (fcntl
    # F_SETLK, distinct from round-13's F_GETLK query), and inotify watch removal
    # (inotify_rm_watch, distinct from round-12's add-only guest) -- all families
    # e9patch preprocessing must leave byte-identical to golden ptrace. Every
    # printed value is host-independent: the syscall return (0 on success) or a
    # boolean (getdents entries present -> 1, personality query succeeded -> 1).
    # The cpu/node and persona value are read/used but never printed.
    # process_vm_readv(self) was DROPPED: it returns -1 under golden hermit
    # ptrace (the tracer/self read is not supported), so it would encode a hermit
    # limitation, not parity (no false parity, #152); the batch kept 7 of 8.
    "syncfs_memfd": (0, b"syncfs=0\n"),
    "membarrier_global": (0, b"membarrier=0\n"),
    "getcpu_check": (0, b"getcpu=0\n"),
    "getdents_legacy": (0, b"getdents=1\n"),
    "personality_query": (0, b"persona=1\n"),
    "fcntl_setlk_memfd": (0, b"setlk=0\n"),
    "inotify_rm_watch": (0, b"inotify_rm=0\n"),
    # Round-16 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into randomness (getrandom -- prints the byte count filled, not the random
    # bytes), the gettid thread-id query, the LEGACY getrlimit(97) (distinct from
    # round-8's prlimit64), clear-child-tid registration (set_tid_address), two
    # scheduler QUERIES that read but never change scheduling (sched_get_priority_max
    # SCHED_OTHER and sched_getscheduler, both fixed constant 0 for the default
    # policy), and the no-argument sync (distinct from round-15's fd-scoped
    # syncfs) -- all families e9patch preprocessing must leave byte-identical to
    # golden ptrace. Every printed value is host-independent: the syscall return
    # on success (0), a boolean (gettid/set_tid_address returned a positive
    # tid -> 1), a fixed byte count (getrandom fills 16), or a fixed scheduler
    # constant (SCHED_OTHER = 0). The random bytes, tid values, and rlimit fields
    # are read/used but never printed. The scheduler guests only QUERY policy/
    # range; they do not alter scheduling, so they are routine backend-parity
    # coverage, not a DetCore scheduling change. prctl PR_GET_CHILD_SUBREAPER was
    # DROPPED: it returns -ENOSYS (-38) under golden hermit ptrace, so it would
    # encode a hermit limitation, not parity (no false parity, #152); the batch
    # kept 7 of 8.
    "getrandom_bytes": (0, b"getrandom=16\n"),
    "gettid_check": (0, b"gettid=1\n"),
    "getrlimit_nofile": (0, b"getrlimit=0\n"),
    "set_tid_address_ok": (0, b"settid=1\n"),
    "sched_priority_max": (0, b"priomax=0\n"),
    "sched_getscheduler_check": (0, b"sched=0\n"),
    "sync_all": (0, b"sync=0\n"),
    # Round-17 new-family ratchet batch (non-time, non-gated). Extends coverage
    # into fd-flag/ownership fcntl ops (F_SETFD+F_GETFD FD_CLOEXEC round-trip
    # -> boolean, F_GETOWN on a pipe -> 0 no owner, F_GETPIPE_SZ -> boolean
    # size>0), two working ioctls beyond round-4's TCGETS/ENOTTY (FIONREAD on a
    # pipe holding 3 bytes -> 3, FIONBIO set-nonblocking -> 0), the semaphore
    # eventfd flag path (EFD_SEMAPHORE read returns 1, distinct from round-6's
    # plain eventfd), the AF_UNIX/SOCK_DGRAM socketpair round-trip (distinct
    # from round-7's SOCK_STREAM pair), and an abstract-namespace bind (leading
    # NUL, no filesystem entry) -> 0. Every printed value is host-independent:
    # the syscall return on success (0), a boolean (cloexec bit / pipe size>0),
    # a fixed readable-byte count (3), a fixed semaphore decrement (1), or fixed
    # round-tripped text ("hi"). The exact pipe capacity, owner pid, and fd
    # numbers are read/used but never printed. All are ordinary fd/socket/ioctl
    # families e9patch preprocessing must leave byte-identical to golden ptrace;
    # none changes scheduling, time, or randomness.
    "fcntl_setfd_cloexec": (0, b"setfd=1\n"),
    "fcntl_getown_pipe": (0, b"getown=0\n"),
    "ioctl_fionread_pipe": (0, b"fionread=3\n"),
    "ioctl_fionbio_pipe": (0, b"fionbio=0\n"),
    "eventfd_semaphore": (0, b"efdsem=1\n"),
    "socketpair_dgram": (0, b"sp=hi\n"),
    "bind_abstract": (0, b"bind=0\n"),
    "fcntl_getpipe_sz": (0, b"pipesz=1\n"),
    # Round-18 new-family ratchet batch (non-time, non-gated). This round targets
    # syscalls with NO existing guest at all (the corpus already covers umask,
    # access, chdir, msync, fstat, statx, prctl-name, and uname, so those were
    # deliberately not duplicated). New syscalls: sched_getparam(143) and
    # sched_get_priority_min(147) -- two pure scheduler QUERIES that read but
    # never change scheduling (return 0 / SCHED_OTHER constants), a dedicated
    # ftruncate(77) memfd resize (return 0; previously only a helper),
    # sync_file_range(277) on a memfd (return 0), the AF_UNIX/SOCK_SEQPACKET
    # socketpair (distinct from the SOCK_STREAM and SOCK_DGRAM pairs),
    # pidfd_open(434) on self (boolean valid-fd, the fd number not printed), and
    # sendmmsg(307) of one datagram on a socketpair (message count 1). Every
    # printed value is host-independent: the syscall return on success (0), a
    # fixed scheduler constant (0), a boolean, a fixed message count (1), or
    # fixed round-tripped text ("hi"). None changes scheduling, time, or
    # randomness, so all are routine backend-parity coverage that e9patch
    # preprocessing must leave byte-identical to golden ptrace. An eighth
    # candidate, prctl PR_SET_NO_NEW_PRIVS/PR_GET_NO_NEW_PRIVS, was DROPPED: the
    # PR_SET_NO_NEW_PRIVS operation returns -ENOSYS (-38) under golden hermit
    # ptrace, so keeping it would encode a hermit limitation, not parity (no
    # false parity, #152); the batch kept seven of eight.
    "sched_getparam_check": (0, b"getparam=0\n"),
    "sched_get_priority_min_check": (0, b"priomin=0\n"),
    "ftruncate_memfd": (0, b"ftruncate=0\n"),
    "sync_file_range_memfd": (0, b"syncrange=0\n"),
    "socketpair_seqpacket": (0, b"sp=hi\n"),
    "pidfd_open_self": (0, b"pidfd=1\n"),
    "sendmmsg_socketpair": (0, b"sendmmsg=1\n"),
    # Round-19 new-family ratchet batch (non-time, non-gated). More syscalls with
    # no existing guest: listen(50) on a bound abstract AF_UNIX socket (return 0,
    # complements bind_abstract), recvmsg(47) and recvfrom(45) round-trips on a
    # socketpair (distinct receive-side counterparts to the sendmsg/sendto
    # guests), arch_prctl ARCH_GET_GS (return 0 only; the GS base is host-specific
    # and never printed, distinct from the ARCH_GET_FS guest), prctl
    # PR_GET_PDEATHSIG (return 0; no parent-death signal set), kill(62) and
    # tgkill(234) with signal 0 (existence/permission checks that deliver no
    # signal, return 0), and flistxattr(196) on a memfd (0-length attribute list).
    # Every printed value is host-independent: the syscall return on success (0)
    # or fixed round-tripped text ("hi"). The signal-0 guests deliver nothing, so
    # they do not exercise signal handling or scheduling; none of the batch
    # changes scheduling, time, or randomness, so all are routine backend-parity
    # coverage that e9patch preprocessing must leave byte-identical to golden
    # ptrace.
    "listen_abstract": (0, b"listen=0\n"),
    "recvmsg_socketpair": (0, b"recvmsg=hi\n"),
    "recvfrom_socketpair": (0, b"recvfrom=hi\n"),
    "arch_prctl_getgs": (0, b"getgs=0\n"),
    "prctl_pdeathsig": (0, b"pdeathsig=0\n"),
    "kill_self_sig0": (0, b"kill=0\n"),
    "tgkill_self_sig0": (0, b"tgkill=0\n"),
    "flistxattr_memfd": (0, b"flistxattr=0\n"),
    # Round-20 new-family ratchet batch (non-time, non-gated). Yet more syscalls
    # with no existing guest: fchown(93) as the fchown(fd,-1,-1) no-op that
    # changes neither owner nor group (return 0); munlockall(152), which always
    # succeeds even with nothing locked (return 0); setrlimit(160) writing the
    # current RLIMIT_NOFILE back unchanged (a no-op, return 0; the host-specific
    # limit values are read but never printed); and four pure scheduler/process
    # QUERIES that read but never change scheduling -- sched_getaffinity(204)
    # (boolean success only; the returned byte count is host CPU-count dependent
    # and never printed), sched_rr_get_interval(148) (return 0; interval not
    # printed), sched_getattr(275) (return 0; attributes not printed), and
    # setpgid(0,0) (109) which sets the process group to the pid, a process-group
    # change and not a thread-scheduling change (return 0). poll(7) with no fds
    # and a zero timeout returns immediately with 0 and never blocks. Every
    # printed value is host-independent: the syscall return on success (0) or a
    # boolean. None changes scheduling, time, or randomness, so all are routine
    # backend-parity coverage that e9patch preprocessing must leave byte-identical
    # to golden ptrace. An eighth candidate, sched_getattr(275), was DROPPED: it
    # returns -ENOSYS (-38) under golden hermit ptrace, so keeping it would be a
    # false-parity claim (#152) -- the batch kept seven of eight.
    "fchown_memfd": (0, b"fchown=0\n"),
    "munlockall_ok": (0, b"munlockall=0\n"),
    "setrlimit_nofile": (0, b"setrlimit=0\n"),
    "sched_getaffinity_check": (0, b"affinity=1\n"),
    "sched_rr_get_interval_check": (0, b"rrinterval=0\n"),
    "setpgid_self": (0, b"setpgid=0\n"),
    "poll_timeout_zero": (0, b"poll=0\n"),
    # Round-21 new-family ratchet batch (non-time, non-gated). Yet more syscalls
    # with no existing guest: munlock(11) after an mlock, unlocking one page
    # (return 0); connect(42) of a second AF_UNIX stream socket to a listening
    # abstract-namespace address (return 0, exercising the connect path that
    # complements bind_abstract/listen_abstract); recvmmsg(299) reading back one
    # datagram previously sent with sendmmsg over a socketpair (round-trips "hi",
    # the receive-side counterpart to sendmmsg_socketpair); preadv2(327) and
    # pwritev2(328) positional scatter/gather I/O on a memfd (round-trip "hi",
    # the flagged v2 counterparts to preadv/pwritev); prctl PR_CAP_AMBIENT(47)
    # PR_CAP_AMBIENT_IS_SET query for CAP_CHOWN (boolean 0, not in the ambient
    # set); and pidfd_getfd(438) duplicating one of the process's own descriptors
    # through a self pidfd (boolean valid-fd; the fd number is host-specific and
    # not printed). Every printed value is host-independent: the syscall return on
    # success (0), a boolean, or fixed round-tripped text ("hi"). None changes
    # scheduling, time, or randomness, so all are routine backend-parity coverage
    # that e9patch preprocessing must leave byte-identical to golden ptrace.
    "munlock_page": (0, b"munlock=0\n"),
    "connect_abstract": (0, b"connect=0\n"),
    "recvmmsg_socketpair": (0, b"recvmmsg=hi\n"),
    "preadv2_memfd": (0, b"preadv2=hi\n"),
    "pwritev2_memfd": (0, b"pwritev2=hi\n"),
    "prctl_cap_ambient": (0, b"capambient=0\n"),
    "pidfd_getfd_self": (0, b"pidfdgetfd=1\n"),
    # Round-22 new-family ratchet batch (non-time, non-gated). More syscalls with
    # no existing guest: the four credential-changing syscalls in their no-op
    # forms -- setresuid(117) and setresgid(119) with (-1,-1,-1), and setreuid(113)
    # and setregid(114) with (-1,-1) -- which leave every real/effective/saved id
    # unchanged and return 0 (distinct kernel entry points, an identity change
    # rather than scheduling); accept4(288) accepting a pending connection on a
    # listening abstract AF_UNIX socket with SOCK_NONBLOCK (boolean valid-fd; the
    # accepted fd number is host-specific and not printed); and ioprio_get(252)
    # querying this process's I/O priority (boolean success only, since the
    # class/level value is host-configuration dependent and not printed). Every
    # printed value is host-independent: the syscall return on success (0) or a
    # boolean. None changes CPU scheduling, time, or randomness, so all are
    # routine backend-parity coverage that e9patch preprocessing must leave
    # byte-identical to golden ptrace.
    "setresuid_noop": (0, b"setresuid=0\n"),
    "setresgid_noop": (0, b"setresgid=0\n"),
    "setreuid_noop": (0, b"setreuid=0\n"),
    "setregid_noop": (0, b"setregid=0\n"),
    "accept4_abstract": (0, b"accept4=1\n"),
    "ioprio_get_check": (0, b"ioprio=1\n"),
    # Round-23 new-family ratchet batch (non-time, non-gated). Families with no
    # existing guest: scheduling *priority* (not CPU scheduling) via
    # setpriority(141) as a PRIO_PROCESS no-op returning 0 and getpriority(140)
    # reporting only a boolean (the 20-nice value is host-configuration
    # dependent and not printed); NUMA
    # memory policy via get_mempolicy(239) and set_mempolicy(238) in their
    # default forms returning 0; the x86 local descriptor table via
    # modify_ldt(154) func=0 read, which yields 0 bytes for a process with no
    # custom LDT entries; and rt_sigqueueinfo(129) queuing signal 0 to self,
    # which performs only the permission check (no delivery) and returns 0.
    # Every printed value is host-independent: the syscall return on success (0)
    # or a boolean. None of these change CPU scheduling, virtual time, or
    # randomness -- setpriority/getpriority adjust nice-level accounting, not the
    # deterministic thread schedule -- so all are routine backend-parity coverage
    # that e9patch preprocessing must leave byte-identical to golden ptrace.
    # (io_setup(206)/io_destroy(207) were probed and DROPPED per no-false-parity
    # #152: io_setup returns -ENOSYS under hermit, so the AIO family has no guest.)
    "setpriority_self": (0, b"setpriority=0\n"),
    "getpriority_self": (0, b"getpriority=1\n"),
    "get_mempolicy_default": (0, b"getmempolicy=0\n"),
    "set_mempolicy_default": (0, b"setmempolicy=0\n"),
    "modify_ldt_read": (0, b"modifyldt=0\n"),
    "rt_sigqueueinfo_self": (0, b"sigqueueinfo=0\n"),
    # Round-24 new-family ratchet batch (non-time, non-gated). Families with no
    # existing guest: whole-address-space memory locking via mlockall(151)
    # MCL_CURRENT (paired with munlockall) returning 0; faccessat2(439) checking
    # read access to /dev/null returning 0; and pidfd-based signalling via
    # pidfd_send_signal(424) delivering signal 0 through a self pidfd (permission
    # check only, no delivery) returning 0. Every printed value is
    # host-independent: the syscall return on success (0) or a boolean. None
    # changes CPU scheduling, virtual time, or randomness, so all are routine
    # backend-parity coverage that e9patch preprocessing must leave byte-identical
    # to golden ptrace.
    # (openat2(437) and the three System V IPC creation syscalls shmget(29),
    # semget(64), msgget(68) were probed and DROPPED per no-false-parity #152:
    # each returns failure under hermit -- openat2 is not supported, and there is
    # no usable System V IPC namespace -- so those families have no guest.)
    "mlockall_all": (0, b"mlockall=0\n"),
    "faccessat2_devnull": (0, b"faccessat2=0\n"),
    "pidfd_send_signal_self": (0, b"pidfdsignal=0\n"),
    # Round-25 new-family ratchet batch (non-time, non-gated). Families with no
    # existing guest: capability writeback via capset(126), reading this thread's
    # capability sets with capget and writing the identical sets back (a no-op
    # returning 0; distinct from the existing capget read guest); and non-blocking
    # readiness polling via epoll_wait(232) with timeout 0 on an empty epoll
    # interest set, which returns 0 ready events. Every printed value is
    # host-independent: the syscall return on success (0) or a boolean. None
    # changes CPU scheduling, virtual time, or randomness, so all are routine
    # backend-parity coverage that e9patch preprocessing must leave byte-identical
    # to golden ptrace.
    # (keyctl(250) KEYCTL_GET_KEYRING_ID was probed and DROPPED per no-false-parity
    # #152: it returns failure under hermit, so the kernel-keyring family has no
    # guest.)
    "capset_noop": (0, b"capset=0\n"),
    "epoll_wait_timeout_zero": (0, b"epollwait=0\n"),
    # Round-26 new-family ratchet batch (non-time, non-gated). Thread-directed
    # signalling syscalls with no existing guest: tkill(200) sending signal 0 to
    # this thread and rt_tgsigqueueinfo(297) queuing signal 0 to this thread with
    # an SI_QUEUE siginfo, both performing only the permission check (no delivery)
    # and returning 0. Every printed value is host-independent: the syscall return
    # on success (0). Neither changes CPU scheduling, virtual time, or randomness,
    # so both are routine backend-parity coverage that e9patch preprocessing must
    # leave byte-identical to golden ptrace.
    # (Two prctl query options were probed and DROPPED per no-false-parity #152:
    # PR_GET_SECUREBITS(27) and PR_MCE_KILL_GET(34) both return -ENOSYS under
    # hermit. userfaultfd(323) was dropped before shipping: it fails even natively
    # here -- unprivileged userfaultfd is disabled -- so it is an error path, not
    # a supported-success guest.)
    "tkill_self_sig0": (0, b"tkill=0\n"),
    "rt_tgsigqueueinfo_self": (0, b"tgsigqueueinfo=0\n"),
    # Round-27 new-family ratchet batch (non-time, non-gated). Non-blocking
    # readiness / connection / usage syscalls with no existing guest: the legacy
    # accept(43) accepting a pending abstract AF_UNIX connection (distinct from
    # round-22's accept4); the select(23), pselect6(270), and ppoll(271) readiness
    # multiplexers each called with no descriptors and a zero timeout so they
    # return 0 immediately without blocking or registering a timed waiter (the
    # non-blocking-poll family already established by poll_timeout_zero); the
    # epoll_pwait(281) sigmask-carrying variant of epoll_wait, timeout 0 on an
    # empty interest set returning 0; and getrusage(98) filling a rusage struct
    # for RUSAGE_SELF, printing only the syscall return (0) since the usage fields
    # are host-specific. Every printed value is host-independent: the syscall
    # return on success (0) or a boolean valid-fd. A zero-timeout readiness poll
    # returns immediately and registers no timed waiter, so none changes CPU
    # scheduling, virtual time, or randomness; all are routine backend-parity
    # coverage that e9patch preprocessing must leave byte-identical to golden
    # ptrace.
    "accept_abstract": (0, b"accept=1\n"),
    "select_timeout_zero": (0, b"select=0\n"),
    "pselect6_timeout_zero": (0, b"pselect=0\n"),
    "ppoll_timeout_zero": (0, b"ppoll=0\n"),
    "epoll_pwait_timeout_zero": (0, b"epollpwait=0\n"),
    "getrusage_self": (0, b"getrusage=0\n"),
    # Round-28 new-family ratchet batch (non-time, non-gated). Legacy/variant
    # syscall numbers with no existing guest, each distinct from a covered
    # newer counterpart: the LEGACY size-hint epoll_create(213) (vs
    # epoll_create1(291)), the LEGACY single-argument eventfd(284) (vs
    # eventfd2(290)) round-tripping a counter of 5, the LEGACY no-argument
    # inotify_init(253) (vs inotify_init1(294)), and the LEGACY 3-argument
    # signalfd(282) over an empty mask (vs signalfd4(289); no signal delivered).
    # Also mbind(237) setting an anonymous page to MPOL_DEFAULT (the
    # range-scoped NUMA-policy call complementing get_mempolicy/set_mempolicy),
    # ioprio_set(251) setting this process's best-effort I/O priority (an
    # I/O-priority accounting change, not a CPU-schedule change, complementing
    # ioprio_get(252)), and the timespec-based epoll_pwait2(441) with a zero
    # timeout on an empty interest set (returns 0 immediately, no timed waiter;
    # distinct from the millisecond epoll_pwait(281)). Every printed value is
    # host-independent: a boolean valid-fd (1), a fixed round-tripped counter
    # (5), or the syscall return on success (0). The fd creators register no
    # events; none of the batch changes CPU scheduling, virtual time, or
    # randomness, so all are routine backend-parity coverage e9patch
    # preprocessing must leave byte-identical to golden ptrace. Zero drops: all
    # seven ran clean under golden hermit ptrace on the first attempt.
    "epoll_create_legacy": (0, b"epollcreate=1\n"),
    "eventfd_legacy": (0, b"eventfdlegacy=5\n"),
    "inotify_init_legacy": (0, b"inotifyinit=1\n"),
    "signalfd_legacy": (0, b"signalfdlegacy=1\n"),
    "mbind_default": (0, b"mbind=0\n"),
    "ioprio_set_self": (0, b"ioprioset=0\n"),
    "epoll_pwait2_timeout_zero": (0, b"epollpwait2=0\n"),
    # Round-29 new-family ratchet batch (non-time, non-gated). Syscalls and
    # prctl/credential operations with no existing guest: sched_yield(24), which
    # with a single runnable guest thread is a no-op that returns immediately and
    # changes no scheduling decision (a voluntary-yield hint, not a DetCore
    # scheduling change); three new prctl QUERY operations distinct from every
    # existing prctl guest -- PR_CAPBSET_READ(23) for CAP_CHOWN, PR_GET_THP_DISABLE
    # (42), each reduced to a boolean "query succeeded" (return >= 0) because the
    # raw bit/flag is host/config dependent; and the setfsuid(122)/setfsgid(123)
    # credential queries in their setfs*(-1) no-op forms, which change nothing and
    # return the previous fs id, reduced to a boolean "call succeeded" since the
    # previous id is host-dependent. Every printed value is host-independent: the
    # syscall return on success (yield=0) or a boolean. None changes CPU
    # scheduling, virtual time, or randomness, so all are routine backend-parity
    # coverage that e9patch preprocessing must leave byte-identical to golden
    # ptrace. Two candidates were probed and DROPPED per no-false-parity #152:
    # prctl PR_GET_NO_NEW_PRIVS(39) and rseq(334) both return -ENOSYS (-38) under
    # golden hermit ptrace, so keeping them would encode a hermit limitation
    # rather than parity; the batch kept five of seven.
    "sched_yield_noop": (0, b"yield=0\n"),
    "prctl_capbset_read": (0, b"capbset=1\n"),
    "prctl_thp_disable": (0, b"thpdisable=1\n"),
    "setfsuid_noop": (0, b"setfsuid=1\n"),
    "setfsgid_noop": (0, b"setfsgid=1\n"),
    # round-30: process-config query, process-time accounting, and xattr error
    # paths. prctl_timerslack queries the timer slack (a config value inert
    # under hermit's virtual time), reduced to a boolean since the raw value is
    # host-dependent. times_check reads process CPU-tick accounting whose return
    # is host/timing dependent, so it is a PARITY-ONLY guest (None): golden and
    # e9patch must agree byte-for-byte under one deterministic run, but the value
    # is not asserted. The get/lget/fgetxattr guests read a nonexistent user
    # xattr from /dev/null, which always fails negative (-ENODATA where user
    # xattrs are supported, -EOPNOTSUPP otherwise); each prints the boolean
    # "returned an error", a faithful Linux semantic that is host/filesystem
    # independent. None changes CPU scheduling, virtual time, or randomness, so
    # all are routine backend-parity coverage e9patch must leave byte-identical
    # to golden ptrace. Three candidates were probed and DROPPED per
    # no-false-parity #152: prctl PR_GET_FP_MODE(46) returns -ENOSYS (-38) under
    # golden hermit ptrace, and name_to_handle_at(303) and seccomp(317) both
    # return -EOPNOTSUPP (-95); keeping any would encode a hermit limitation
    # rather than parity, so the batch kept five of eight.
    "prctl_timerslack": (0, b"timerslack=1\n"),
    "times_check": (0, None),
    "getxattr_devnull": (0, b"getxattr=1\n"),
    "lgetxattr_devnull": (0, b"lgetxattr=1\n"),
    "fgetxattr_devnull": (0, b"fgetxattr=1\n"),
    # round-31: xattr write-side error paths, inert fd/query probes, and a
    # non-blocking signal poll. The remove/lremove/fremovexattr guests remove a
    # nonexistent user xattr from /dev/null, which always fails negative
    # (-ENODATA or -EOPNOTSUPP by filesystem); each prints the boolean "returned
    # an error", the write-side counterpart to round-30's get/lget/fgetxattr.
    # timerfd_create_check creates but never arms a monotonic timer fd (no
    # settime, no expiration, no timed waiter), printing a boolean valid-fd.
    # clock_getres_monotonic reads the fixed CLOCK_MONOTONIC resolution (a kernel
    # constant, not the current time), returning 0. rt_sigtimedwait_empty polls
    # an empty signal set with a {0,0} timeout, returning -EAGAIN immediately
    # without blocking or registering a timed waiter (same class as the
    # zero-timeout poll/select guests). Every printed value is host-independent;
    # none changes CPU scheduling, virtual time, or randomness. One candidate was
    # DROPPED per no-false-parity #152: kcmp(312) returns -1 under golden hermit
    # ptrace (native returns 0) because pid virtualization breaks the kernel's
    # pid-based comparison, so it would encode a hermit limitation; the batch
    # kept six of seven.
    "removexattr_devnull": (0, b"removexattr=1\n"),
    "lremovexattr_devnull": (0, b"lremovexattr=1\n"),
    "fremovexattr_devnull": (0, b"fremovexattr=1\n"),
    "timerfd_create_check": (0, b"timerfd=1\n"),
    "clock_getres_monotonic": (0, b"clockres=0\n"),
    "rt_sigtimedwait_empty": (0, b"sigtimedwait=-11\n"),
    # round-32: inert fd/timer/sleep probes. memfd_create_check allocates an
    # anonymous memfd and prints a boolean valid-fd (the fd number itself is
    # host-dependent). getitimer_real reads the unarmed ITIMER_REAL interval
    # timer, returning 0. clock_nanosleep_relative sleeps a fixed 1ms relative
    # interval on CLOCK_MONOTONIC, which hermit virtualizes deterministically,
    # returning 0. Every printed value is host-independent; none changes CPU
    # scheduling, virtual time, or randomness. Five candidates were DROPPED per
    # no-false-parity #152: splice(275), tee(276), vmsplice(278), and
    # copy_file_range(326) each return -ENOSYS under golden hermit ptrace though
    # native Linux moves the bytes, and process_vm_readv(310) returns -EPERM on
    # a self target under hermit's ptrace supervision though native succeeds;
    # keeping any of them would encode a hermit limitation as expected Linux
    # behavior. The batch kept three of eight.
    "memfd_create_check": (0, b"memfd=1\n"),
    "getitimer_real": (0, b"getitimer=0\n"),
    "clock_nanosleep_relative": (0, b"clocknanosleep=0\n"),
    # round-33: inert futex/time/scheduler query probes. futex_wake_empty issues
    # futex(FUTEX_WAKE) on a private word with no waiters, deterministically
    # waking zero threads (return 0) in a single-threaded guest. gettimeofday_check
    # calls gettimeofday(96) — a distinct syscall from clock_gettime(228) — and
    # prints only the return (0); the virtualized timeval is not emitted.
    # sched_getattr_self reads its own sched_attr via the unified query (315),
    # returning 0. sched_setaffinity_self round-trips the calling thread's CPU
    # affinity (getaffinity then setaffinity the identical mask), a no-op that
    # returns 0; the host-dependent mask is never printed. Every printed value is
    # host-independent and none perturbs scheduling, virtual time, or randomness.
    "futex_wake_empty": (0, b"futexwake=0\n"),
    "gettimeofday_check": (0, b"gettimeofday=0\n"),
    "sched_getattr_self": (0, b"schedgetattr=0\n"),
    "sched_setaffinity_self": (0, b"setaffinity=0\n"),
}

FREESTANDING_FLAGS = (
    "-nostdlib",
    "-static",
    "-ffreestanding",
    "-O0",
    "-fno-pie",
    "-no-pie",
)


class CorpusError(Exception):
    """A missing corpus source or a failed parity contract."""


def compile_guest(name: str, out_dir: Path) -> Path:
    source = CORPUS_DIR / f"{name}.c"
    if not source.is_file():
        raise CorpusError(f"missing corpus source: {source}")
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise CorpusError("C compiler unavailable (set CC or install cc)")
    output = out_dir / name
    command = [compiler, *FREESTANDING_FLAGS, str(source), "-o", str(output)]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise CorpusError(f"compile failed: {command!r}\n{result.stdout}{result.stderr}")
    return output


def hermit_command(hermit: Path, e9: bool, verify: bool, guest: Path) -> list[str]:
    command = [str(hermit)]
    if e9:
        command.extend(["--backend", "e9patch"])
    command.append("run")
    command.append("--strict")
    if verify:
        command.append("--verify")
    # Guests are compiled into a temp dir under host /tmp; --tmp=/tmp keeps
    # Hermit from replacing the guest's /tmp so the binary path resolves
    # (mirrors run_matrix.py).
    command.append("--tmp=/tmp")
    command.extend(["--", str(guest)])
    return command


def run(command: list[str], timeout: int) -> tuple[int, bytes, bytes]:
    try:
        proc = subprocess.run(
            command, capture_output=True, timeout=timeout, check=False
        )
    except subprocess.TimeoutExpired:
        return 124, b"", b"<timeout>"
    return proc.returncode, proc.stdout, proc.stderr


def detlog_syscalls(hermit: Path, e9: bool, guest: Path) -> list[str]:
    """Canonical guest-syscall sequence from a --log=info plain --strict run.

    Uses the "inbound syscall:" lines (they include exit_group, which has no
    finish line), with timestamps and addresses/large integers normalized so
    the sequence is host-layout independent.
    """
    command = [str(hermit)]
    if e9:
        command.extend(["--backend", "e9patch"])
    command.extend(
        ["--log=info", "run", "--strict", "--tmp=/tmp", "--", str(guest)]
    )
    _, _, stderr = run(command, timeout=60)
    lines: list[str] = []
    for raw in stderr.decode(errors="replace").splitlines():
        match = re.search(r"inbound syscall: ([a-z_0-9]+\(.*\)) = \?$", raw)
        if not match:
            continue
        canonical = re.sub(r"0x[0-9a-f]+", "A", match.group(1))
        canonical = re.sub(r", [0-9]{4,}", ", N", canonical)
        lines.append(canonical)
    return lines


def metric(name: str, stderr: bytes) -> int | None:
    match = re.search(rf"{name}=([0-9]+)", stderr.decode(errors="replace"))
    return int(match.group(1)) if match else None


def l2_ok(stderr: bytes) -> bool:
    return b"Determinism verified" in stderr


def prerequisites(hermit: Path) -> str | None:
    if not hermit.is_file() or not os.access(hermit, os.X_OK):
        return f"hermit executable unavailable: {hermit}"
    for var in ("HERMIT_E9TOOL", "HERMIT_E9PATCH_BACKEND"):
        path = os.environ.get(var)
        if not path or not Path(path).is_file():
            return f"{var} is unset or does not point at a file"
    # A hermit built without the e9patch feature rejects --backend e9patch.
    code, _, stderr = run(
        [str(hermit), "--backend", "e9patch", "run", "--", "/bin/true"], timeout=60
    )
    text = stderr.decode(errors="replace")
    if code != 0 and "e9patch" in text and "feature" in text:
        return "hermit was not built with the e9patch cargo feature"
    return None


def run_guest(hermit: Path, name: str, out_dir: Path) -> tuple[str, str]:
    expected_exit, expected_stdout = CORPUS[name]
    guest = compile_guest(name, out_dir)

    gx, gout, _ = run(hermit_command(hermit, False, False, guest), timeout=40)
    _, _, gv = run(hermit_command(hermit, False, True, guest), timeout=60)
    ex, eout, eerr = run(hermit_command(hermit, True, False, guest), timeout=60)
    _, _, ev = run(hermit_command(hermit, True, True, guest), timeout=90)

    if gx == 124 or ex == 124:
        return "FAIL", f"timeout (golden={gx}, e9patch={ex})"
    if gx != expected_exit:
        return "FAIL", f"golden exit {gx}, expected {expected_exit}"
    if gx != ex:
        return "FAIL", f"exit divergence golden={gx} e9patch={ex}"
    if gout != eout:
        return "FAIL", f"stdout divergence golden={gout!r} e9patch={eout!r}"
    if expected_stdout is not None and gout != expected_stdout:
        return "FAIL", f"golden stdout {gout!r}, expected {expected_stdout!r}"
    if not l2_ok(gv):
        return "FAIL", "golden not L2 (no 'Determinism verified')"
    if not l2_ok(ev):
        return "FAIL", "e9patch not L2 (no 'Determinism verified')"

    cand, mapped, b0 = (
        metric("candidate_sites", eerr),
        metric("mapped_sites", eerr),
        metric("b0_sites", eerr),
    )
    if cand is None or mapped is None or b0 is None:
        return "FAIL", "missing e9patch backend metrics"
    if cand == 0:
        return "FAIL", "candidate_sites=0 (guest did not exercise the rewrite path)"
    if mapped != cand:
        return "FAIL", f"incomplete coverage mapped={mapped} candidate={cand}"
    if b0 != 0:
        return "FAIL", f"b0_sites={b0} (SIGILL signal fallback rejected)"

    golden_seq = detlog_syscalls(hermit, False, guest)
    e9_seq = detlog_syscalls(hermit, True, guest)
    prologue = len(e9_seq) - len(golden_seq)
    if prologue < 0 or e9_seq[prologue:] != golden_seq:
        return "FAIL", (
            "guest-syscall DETLOG tail mismatch "
            f"golden={golden_seq!r} e9patch={e9_seq!r}"
        )
    return "PASS_L2", (
        f"exit={gx} sites c/{cand} m/{mapped} b0/{b0} "
        f"prologue={prologue} tail_match=yes"
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--hermit",
        type=Path,
        default=REPOSITORY / "target/debug/hermit",
        help="Hermit executable (must be built --features e9patch)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="validate the corpus contract and list guests without running",
    )
    parser.add_argument(
        "--require-backend",
        action="store_true",
        help="fail instead of reporting BLOCKED when prerequisites are absent",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    print(f"CORPUS: {len(CORPUS)} freestanding e9patch parity guests")
    for name in CORPUS:
        source = CORPUS_DIR / f"{name}.c"
        if not source.is_file():
            raise CorpusError(f"missing corpus source: {source}")
    if args.check:
        for name in CORPUS:
            print(f"  contract {name}")
        return 0

    hermit = args.hermit.resolve()
    block = prerequisites(hermit)
    if block:
        print(f"BLOCKED: {block}")
        return 1 if args.require_backend else 0

    failures = 0
    with tempfile.TemporaryDirectory(prefix="hermit-e9patch-corpus-") as tempdir:
        for name in CORPUS:
            status, detail = run_guest(hermit, name, Path(tempdir))
            print(f"{status} {name}: {detail}")
            if status != "PASS_L2":
                failures += 1
    passed = len(CORPUS) - failures
    print(f"RATCHET e9patch: {passed}/{len(CORPUS)} PASS_L2")
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except CorpusError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(2)
