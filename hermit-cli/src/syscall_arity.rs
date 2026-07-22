/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Arity-aware comparison of raw syscalls for desync detection.
//!
//! reverie's `typed_syscall!` macro represents every syscall (typed variants
//! *and* the `Syscall::Other` fallback) as a `raw: SyscallArgs` holding all six
//! argument registers, and derives `PartialEq` over that raw struct. Comparing
//! two `Syscall`s therefore compares all six registers even for syscalls that
//! use fewer. Registers beyond a syscall's ABI arity are caller-leftover garbage
//! that is not part of the syscall's semantics and routinely differs between a
//! record run (native execution) and a replay run (many syscalls emulated), so a
//! naive `==` produces false desync positives on any 2- or 3-argument syscall
//! (statfs, uname, and the rest of the class).
//!
//! [`syscalls_match`] compares two syscalls after zeroing the argument registers
//! at or beyond the syscall's arity, on both operands, closing the whole class.

use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

/// Number of argument registers a syscall's ABI actually defines.
///
/// Keyed on the (arch-independent) syscall name so it compiles on any target
/// without referencing arch-gated `Sysno` variants. Unknown syscalls return `6`
/// so that no register is masked: erring high can only preserve a real desync,
/// whereas erring low could hide a genuine divergence in a used argument.
fn syscall_arity(sysno: Sysno) -> usize {
    match sysno.name() {
        // 0 arguments.
        "restart_syscall" | "fork" | "vfork" | "getpid" | "getuid" | "getgid" | "geteuid"
        | "getegid" | "getppid" | "getpgrp" | "setsid" | "sched_yield" | "pause" | "sync"
        | "gettid" | "munlockall" | "vhangup" | "rt_sigreturn" | "inotify_init" => 0,

        // 1 argument.
        "close"
        | "brk"
        | "dup"
        | "chdir"
        | "fchdir"
        | "rmdir"
        | "unlink"
        | "exit"
        | "exit_group"
        | "uname"
        | "sysinfo"
        | "times"
        | "chroot"
        | "setuid"
        | "setgid"
        | "setfsuid"
        | "setfsgid"
        | "umask"
        | "getpgid"
        | "getsid"
        | "syncfs"
        | "fsync"
        | "fdatasync"
        | "shmdt"
        | "alarm"
        | "time"
        | "set_tid_address"
        | "epoll_create"
        | "epoll_create1"
        | "eventfd"
        | "inotify_init1"
        | "unshare"
        | "personality"
        | "sched_getscheduler"
        | "sched_get_priority_max"
        | "sched_get_priority_min"
        | "mlockall"
        | "io_destroy"
        | "timer_getoverrun"
        | "timer_delete"
        | "swapoff"
        | "userfaultfd"
        | "pkey_free"
        | "memfd_secret" => 1,

        // 2 arguments.
        "stat"
        | "lstat"
        | "fstat"
        | "access"
        | "munmap"
        | "dup2"
        | "nanosleep"
        | "getitimer"
        | "kill"
        | "tkill"
        | "shutdown"
        | "listen"
        | "flock"
        | "truncate"
        | "ftruncate"
        | "rename"
        | "mkdir"
        | "creat"
        | "link"
        | "symlink"
        | "chmod"
        | "fchmod"
        | "utime"
        | "utimes"
        | "statfs"
        | "fstatfs"
        | "ustat"
        | "getrlimit"
        | "setrlimit"
        | "getrusage"
        | "gettimeofday"
        | "settimeofday"
        | "sethostname"
        | "setdomainname"
        | "clock_gettime"
        | "clock_settime"
        | "clock_getres"
        | "clock_adjtime"
        | "getgroups"
        | "setgroups"
        | "setpgid"
        | "setreuid"
        | "setregid"
        | "capget"
        | "capset"
        | "rt_sigpending"
        | "rt_sigsuspend"
        | "sigaltstack"
        | "pipe2"
        | "sched_setparam"
        | "sched_getparam"
        | "sched_rr_get_interval"
        | "mlock"
        | "munlock"
        | "pivot_root"
        | "getpriority"
        | "ioprio_get"
        | "umount2"
        | "swapon"
        | "io_setup"
        | "removexattr"
        | "lremovexattr"
        | "fremovexattr"
        | "timerfd_create"
        | "eventfd2"
        | "signalfd"
        | "setns"
        | "pkey_alloc"
        | "memfd_create"
        | "timer_gettime"
        | "timerfd_gettime"
        | "fanotify_init"
        | "pidfd_open"
        | "clone3"
        | "fsopen"
        | "process_mrelease"
        | "msgget"
        | "io_uring_setup" => 2,

        // 3 arguments.
        "read" | "write" | "open" | "poll" | "lseek" | "ioctl" | "readv" | "writev" | "msync"
        | "mincore" | "madvise" | "shmget" | "shmat" | "shmctl" | "setitimer" | "connect"
        | "accept" | "sendmsg" | "recvmsg" | "bind" | "getpeername" | "getsockname" | "socket"
        | "fcntl" | "getdents" | "getdents64" | "chown" | "fchown" | "lchown" | "mprotect"
        | "execve" | "readlink" | "mknod" | "sysfs" | "setpriority" | "sched_setscheduler"
        | "syslog" | "getresuid" | "getresgid" | "setresuid" | "setresgid" | "modify_ldt"
        | "ioperm" | "init_module" | "finit_module" | "listxattr" | "llistxattr" | "flistxattr"
        | "io_submit" | "io_cancel" | "lookup_dcookie" | "getcpu" | "inotify_add_watch"
        | "sched_setaffinity" | "sched_getaffinity" | "sched_setattr" | "timer_create"
        | "tgkill" | "mkdirat" | "unlinkat" | "symlinkat" | "fchmodat" | "faccessat"
        | "readlinkat" | "rt_sigqueueinfo" | "get_robust_list" | "seccomp" | "getrandom"
        | "bpf" | "membarrier" | "mlock2" | "open_by_handle_at" | "sendmmsg" | "fspick"
        | "fsmount" | "close_range" | "pidfd_getfd" | "quotactl_fd" | "open_tree"
        | "futex_wake" | "msgctl" | "ioprio_set" | "semop" | "semget" => 3,

        // 4 arguments.
        "rt_sigaction"
        | "rt_sigprocmask"
        | "pread64"
        | "pwrite64"
        | "wait4"
        | "sendfile"
        | "ptrace"
        | "semctl"
        | "msgsnd"
        | "rt_sigtimedwait"
        | "getxattr"
        | "lgetxattr"
        | "fgetxattr"
        | "semtimedop"
        | "futex_requeue"
        | "epoll_wait"
        | "epoll_ctl"
        | "fadvise64"
        | "timer_settime"
        | "clock_nanosleep"
        | "quotactl"
        | "reboot"
        | "socketpair"
        | "newfstatat"
        | "renameat"
        | "fchownat"
        | "futimesat"
        | "utimensat"
        | "accept4"
        | "signalfd4"
        | "dup3"
        | "rseq"
        | "rt_tgsigqueueinfo"
        | "prlimit64"
        | "sync_file_range"
        | "tee"
        | "vmsplice"
        | "fallocate"
        | "timerfd_settime"
        | "request_key"
        | "mq_open"
        | "migrate_pages"
        | "sched_getattr"
        | "pkey_mprotect"
        | "io_uring_register"
        | "pidfd_send_signal"
        | "openat"
        | "openat2"
        | "faccessat2"
        | "fchmodat2"
        | "cachestat"
        | "set_mempolicy_home_node" => 4,

        // 5 arguments.
        "select" | "mremap" | "setsockopt" | "getsockopt" | "msgrcv" | "clone" | "waitid"
        | "add_key" | "keyctl" | "setxattr" | "lsetxattr" | "fsetxattr" | "io_getevents"
        | "get_mempolicy" | "mq_timedsend" | "mq_timedreceive" | "kexec_load"
        | "kexec_file_load" | "perf_event_open" | "recvmmsg" | "fanotify_mark"
        | "name_to_handle_at" | "linkat" | "renameat2" | "execveat" | "preadv" | "pwritev"
        | "ppoll" | "remap_file_pages" | "mknodat" | "statx" | "prctl" | "process_madvise"
        | "move_mount" | "fsconfig" | "mount_setattr" | "mount" | "futex_waitv" => 5,

        // 6 arguments.
        "mmap" | "sendto" | "recvfrom" | "futex" | "mbind" | "pselect6" | "epoll_pwait"
        | "epoll_pwait2" | "splice" | "move_pages" | "process_vm_readv" | "process_vm_writev"
        | "copy_file_range" | "preadv2" | "pwritev2" | "io_pgetevents" | "io_uring_enter"
        | "futex_wait" => 6,

        _ => 6,
    }
}

/// Zeroes argument registers at or beyond the syscall's ABI arity so that
/// unused, caller-leftover register contents do not participate in comparison.
fn normalize_args(sysno: Sysno, args: SyscallArgs) -> SyscallArgs {
    match syscall_arity(sysno) {
        0 => SyscallArgs::new(0, 0, 0, 0, 0, 0),
        1 => SyscallArgs::new(args.arg0, 0, 0, 0, 0, 0),
        2 => SyscallArgs::new(args.arg0, args.arg1, 0, 0, 0, 0),
        3 => SyscallArgs::new(args.arg0, args.arg1, args.arg2, 0, 0, 0),
        4 => SyscallArgs::new(args.arg0, args.arg1, args.arg2, args.arg3, 0, 0),
        5 => SyscallArgs::new(args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, 0),
        _ => args,
    }
}

/// Compares a recorded syscall against an observed one for desync detection,
/// ignoring argument registers beyond the syscall's ABI arity.
pub fn syscalls_match(recorded: Syscall, observed: Syscall) -> bool {
    let (recorded_no, recorded_args) = recorded.into_parts();
    let (observed_no, observed_args) = observed.into_parts();

    recorded_no == observed_no
        && normalize_args(recorded_no, recorded_args) == normalize_args(observed_no, observed_args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(sysno: Sysno, args: SyscallArgs) -> Syscall {
        Syscall::from_raw(sysno, args)
    }

    #[test]
    fn ignores_garbage_in_unused_registers() {
        // statfs (arity 2): only arg0/arg1 are defined. Garbage in arg2..arg5
        // must not cause a false desync. This is the reported case, where the
        // unused arg2 held 0xfffffffffffffea0 at record and 0 at replay.
        let recorded = raw(
            Sysno::statfs,
            SyscallArgs::new(0x1000, 0x2000, 0xfffffffffffffea0, 0xdead, 0, 0xbeef),
        );
        let observed = raw(Sysno::statfs, SyscallArgs::new(0x1000, 0x2000, 0, 0, 0, 0));
        assert!(syscalls_match(recorded, observed));
    }

    #[test]
    fn uname_ignores_unused_registers() {
        // uname (arity 1): only arg0 is defined.
        let recorded = raw(Sysno::uname, SyscallArgs::new(0x1000, 0x11, 0x22, 0, 0, 0));
        let observed = raw(Sysno::uname, SyscallArgs::new(0x1000, 0, 0, 0, 0, 0));
        assert!(syscalls_match(recorded, observed));
    }

    #[test]
    fn detects_divergence_in_used_register() {
        // A difference in a *used* argument is still a real desync.
        let recorded = raw(Sysno::statfs, SyscallArgs::new(0x1000, 0x2000, 0, 0, 0, 0));
        let observed = raw(Sysno::statfs, SyscallArgs::new(0x1000, 0x9999, 0, 0, 0, 0));
        assert!(!syscalls_match(recorded, observed));
    }

    #[test]
    fn detects_different_syscall_numbers() {
        let a = raw(Sysno::statfs, SyscallArgs::new(0x1000, 0x2000, 0, 0, 0, 0));
        let b = raw(Sysno::fstatfs, SyscallArgs::new(0x1000, 0x2000, 0, 0, 0, 0));
        assert!(!syscalls_match(a, b));
    }

    #[test]
    fn unknown_syscall_compares_all_registers() {
        // Arity defaults to 6 for unknown syscalls, so every register still
        // participates and a divergence in any is detected.
        assert_eq!(syscall_arity(Sysno::mmap), 6);
    }
}
