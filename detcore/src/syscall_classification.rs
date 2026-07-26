/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::syscalls::Sysno;

const EXPECTED_X86_64_SYSNO_COUNT: usize = 373;

// `Sysno` is externally `#[non_exhaustive]`. These assertions make additions,
// removals, or a changed table endpoint fail at compile time instead of silently
// reaching the required final arm.
const _: () = {
    assert!(Sysno::count() == EXPECTED_X86_64_SYSNO_COUNT);
    assert!(Sysno::last().id() == 461);
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
/// Detcore's execution policy for a named Linux syscall.
pub(crate) enum SyscallClassification {
    /// Detcore models the syscall or applies an explicit deterministic refusal policy.
    Determinized,
    /// The syscall is intentionally forwarded under documented container assumptions.
    PassThrough,
    /// The syscall has been audited but its deterministic handler is not yet
    /// implemented. It retains the fail-closed-or-forward policy (panic under
    /// `panic_on_unsupported_syscalls`, otherwise forward) until the tracked
    /// determinization work lands. Each member has a `Syscall classification: <name>`
    /// issue on rrnewton/hermit and is owned by the syscall-implementation backlog.
    Unimplemented,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#275): Review syscall policy categories and fail-closed boundaries.
/// Classifies every syscall in the pinned x86_64 `Sysno` table.
pub(crate) const fn classify_syscall(sysno: Sysno) -> SyscallClassification {
    match sysno {
        // ===== DETERMINIZED SYSCALLS =====
        // These have a Detcore handler, deterministic replacement, or explicit refusal policy.
        Sysno::accept
        | Sysno::accept4
        | Sysno::alarm
        | Sysno::arch_prctl
        | Sysno::bind
        | Sysno::clock_getres
        | Sysno::clock_gettime
        | Sysno::clock_nanosleep
        | Sysno::clone
        | Sysno::clone3
        | Sysno::close
        | Sysno::connect
        | Sysno::creat
        | Sysno::dup
        | Sysno::dup2
        | Sysno::dup3
        | Sysno::epoll_create
        | Sysno::epoll_create1
        | Sysno::epoll_ctl
        | Sysno::epoll_ctl_old
        | Sysno::epoll_pwait
        | Sysno::epoll_wait
        | Sysno::epoll_wait_old
        | Sysno::eventfd
        | Sysno::eventfd2
        | Sysno::execve
        | Sysno::execveat
        | Sysno::exit
        | Sysno::exit_group
        | Sysno::fadvise64
        | Sysno::fcntl
        | Sysno::fork
        | Sysno::fstat
        | Sysno::fstatfs
        | Sysno::futex
        | Sysno::futimesat
        | Sysno::getcpu
        | Sysno::getdents
        | Sysno::getdents64
        | Sysno::getrandom
        | Sysno::getrusage
        | Sysno::gettimeofday
        | Sysno::inotify_add_watch
        | Sysno::inotify_init
        | Sysno::inotify_init1
        | Sysno::inotify_rm_watch
        | Sysno::io_uring_enter
        | Sysno::io_uring_register
        | Sysno::io_uring_setup
        | Sysno::ioctl
        | Sysno::lstat
        | Sysno::madvise
        | Sysno::membarrier
        | Sysno::memfd_create
        | Sysno::mmap
        | Sysno::mremap
        | Sysno::munmap
        | Sysno::nanosleep
        | Sysno::newfstatat
        | Sysno::open
        | Sysno::openat
        | Sysno::pause
        | Sysno::pipe
        | Sysno::pipe2
        | Sysno::poll
        | Sysno::ppoll
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#686): Review scratch fd sets and scheduler polling.
        | Sysno::pselect6
        | Sysno::prlimit64
        | Sysno::pread64
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#683): Confirm positional-write ordering and replay semantics.
        | Sysno::pwrite64
        | Sysno::read
        | Sysno::recvfrom
        | Sysno::recvmsg
        | Sysno::rseq
        | Sysno::rt_sigaction
        | Sysno::rt_sigprocmask
        | Sysno::rt_sigtimedwait
        | Sysno::rt_sigsuspend
        | Sysno::sched_getaffinity
        | Sysno::sched_setaffinity
        | Sysno::sched_yield
        | Sysno::sendmmsg
        | Sysno::sendmsg
        | Sysno::sendto
        | Sysno::setsid
        | Sysno::signalfd
        | Sysno::signalfd4
        | Sysno::socket
        | Sysno::socketpair
        | Sysno::stat
        | Sysno::statfs
        | Sysno::statx
        | Sysno::sysinfo
        | Sysno::time
        | Sysno::timer_create
        | Sysno::timer_delete
        | Sysno::timer_getoverrun
        | Sysno::timer_gettime
        | Sysno::timer_settime
        | Sysno::timerfd_create
        | Sysno::timerfd_gettime
        | Sysno::timerfd_settime
        | Sysno::uname
        | Sysno::userfaultfd
        | Sysno::utime
        | Sysno::utimensat
        | Sysno::utimes
        | Sysno::vfork
        | Sysno::wait4
        | Sysno::waitid
        | Sysno::write
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::clock_settime
        | Sysno::getpeername
        | Sysno::getsockname
        | Sysno::getsockopt
        | Sysno::getpriority
        | Sysno::getrlimit
        | Sysno::kill
        | Sysno::listen
        | Sysno::prctl
        | Sysno::rt_sigpending
        | Sysno::setitimer
        | Sysno::setpriority
        | Sysno::process_madvise
        | Sysno::setrlimit
        | Sysno::setsockopt
        | Sysno::tgkill
        // TODO-HUMAN-REVIEW(#547)
        | Sysno::writev
        // ----- Deterministic refusal (ENOSYS) -----
        // These syscalls are obsolete, removed, or never implemented on modern
        // x86_64 Linux. Refusing them with a deterministic ENOSYS via
        // `is_deterministically_refused` is a Determinized outcome ("explicit
        // deterministic refusal policy"). Every entry was verified to already
        // return ENOSYS on the reference host; several (e.g. sysfs, uselib,
        // lookup_dcookie) are kernel-config/version dependent, so deterministic
        // refusal makes their result host-independent rather than a passthrough
        // that varies per host. `ustat` is intentionally NOT here: it is still
        // implemented, so it stays in the fail-closed backlog.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#715): Review deterministic ENOSYS refusal for obsolete syscalls.
        | Sysno::_sysctl
        | Sysno::afs_syscall
        | Sysno::create_module
        | Sysno::get_kernel_syms
        | Sysno::getpmsg
        | Sysno::lookup_dcookie
        | Sysno::nfsservctl
        | Sysno::putpmsg
        | Sysno::query_module
        | Sysno::security
        | Sysno::sysfs
        | Sysno::tuxcall
        | Sysno::uselib
        | Sysno::vserver => SyscallClassification::Determinized,

        // ===== BEGIN PASS-THRU SYSCALLS =====
        // These existing and triaged passthroughs are conditionally repeatable under
        // Hermit's fixed-container, stable-filesystem, and serialization assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#503): Confirm the stable-state boundary for these promotions.
        Sysno::access
        | Sysno::brk
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::chown
        | Sysno::getcwd
        | Sysno::getegid
        | Sysno::geteuid
        | Sysno::getgid
        | Sysno::getpid
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::getpgid
        | Sysno::getpgrp
        | Sysno::getppid
        | Sysno::getsid
        | Sysno::gettid
        | Sysno::getuid
        | Sysno::lseek
        | Sysno::mprotect
        | Sysno::readlink
        | Sysno::set_robust_list
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        | Sysno::setpgid
        | Sysno::set_tid_address
        | Sysno::sigaltstack
        // capget/capset/getgroups observe or update kernel credential state that starts
        // from the fixed container identity on each run.
        | Sysno::capget
        | Sysno::capset
        | Sysno::getgroups
        // chdir/fchdir/faccessat2/umask are deterministic process-state transitions or
        // checks given a fixed namespace, credential set, and filesystem image.
        | Sysno::chdir
        | Sysno::faccessat2
        | Sysno::fchdir
        | Sysno::umask
        // chmod/fchmodat/linkat/mkdir/mkdirat/renameat2/rmdir/symlinkat/unlink/unlinkat
        // repeat given stable guest-visible filesystem state with no external mutation.
        | Sysno::chmod
        | Sysno::fchmodat
        | Sysno::linkat
        | Sysno::mkdir
        | Sysno::mkdirat
        | Sysno::renameat2
        | Sysno::rmdir
        | Sysno::symlinkat
        | Sysno::unlink
        | Sysno::unlinkat
        // getxattr/lgetxattr/removexattr/setxattr are deterministic for stable objects
        // and do not introduce asynchronous state or new kernel objects.
        | Sysno::getxattr
        | Sysno::lgetxattr
        | Sysno::removexattr
        | Sysno::setxattr
        // fdatasync/ftruncate have deterministic results for stable guest-owned files;
        // physical flush latency is outside guest logical time.
        | Sysno::fdatasync
        | Sysno::ftruncate
        // Fixed credentials, process-local unlocks, and guest-owned filesystem
        // flushes are repeatable under the fixed-container model.
        // TODO-HUMAN-REVIEW(PR-654): Verify deterministic passthrough assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fsync
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::getresgid
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::getresuid
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::munlock
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::munlockall
        // These synchronous extent and pathname operations are repeatable for guest-owned
        // files in a fixed namespace with adequate space and no external mutation.
        // TODO-HUMAN-REVIEW(PR-675): Verify stable-filesystem passthrough assumptions.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fallocate
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::readlinkat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::rename
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::renameat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::truncate
        // Stable guest-owned metadata and synchronous writeback operations are
        // repeatable in Hermit's fixed mount namespace and filesystem image.
        // TODO-HUMAN-REVIEW(#683): Confirm the metadata/writeback passthrough boundary.
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::faccessat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fchmod
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fchmodat2
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fchown
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fchownat
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fgetxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::flistxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fremovexattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::fsetxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lchown
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::link
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::listxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::llistxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lremovexattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::lsetxattr
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::msync
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::readahead
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::symlink
        // AUTONOMOUS-BOT-IMPLEMENTED
        | Sysno::sync_file_range
        // Ptrace executes rt_sigreturn directly; DBI has dedicated injected-sigreturn
        // handling, while KVM deterministically reports its current lack of signal support.
        | Sysno::rt_sigreturn
        // ----- Audited safe passthroughs (non-blocking, deterministic) -----
        // These syscalls are non-blocking and produce results that are repeatable
        // under Hermit's fixed-container identity, stable filesystem, and serialized
        // single-CPU model. Forwarding them is deterministic and lets them run under
        // --strict instead of hitting the fail-closed backlog. `is_extra_passthrough`
        // forwards them by Sysno (the pinned Reverie revision exposes several as raw
        // calls without a typed `Syscall` variant).
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#715): Review audited non-blocking deterministic passthroughs.
        | Sysno::close_range
        | Sysno::get_robust_list
        | Sysno::get_thread_area
        | Sysno::ioprio_get
        | Sysno::ioprio_set
        | Sysno::mlock
        | Sysno::mlock2
        | Sysno::mlockall
        | Sysno::remap_file_pages
        | Sysno::set_thread_area
        | Sysno::setfsgid
        | Sysno::setfsuid
        | Sysno::setgid
        | Sysno::setgroups
        | Sysno::setregid
        | Sysno::setresgid
        | Sysno::setresuid
        | Sysno::setreuid
        | Sysno::setuid
        | Sysno::shutdown
        | Sysno::sync
        | Sysno::syncfs => SyscallClassification::PassThrough,
        // ===== END PASS-THRU SYSCALLS =====

        // ===== UNIMPLEMENTED (AUDITED; DETERMINIZATION PENDING) =====
        // These syscalls have been audited but do not yet have a deterministic
        // handler. Most are potentially blocking or expose host/scheduler/timer
        // state, so they stay fail-closed under --strict (panic) and forward
        // otherwise, exactly as before. Each has a `Syscall classification: <name>`
        // issue on rrnewton/hermit and is owned by the syscall-implementation
        // backlog (impl-syscall-implement-batch2). Do not promote one to PassThrough
        // without confirming it is non-blocking and deterministic; do not silently
        // ENOSYS one that real programs depend on.
        Sysno::acct
        | Sysno::add_key
        | Sysno::adjtimex
        | Sysno::bpf
        | Sysno::cachestat
        | Sysno::chroot
        | Sysno::clock_adjtime
        | Sysno::copy_file_range
        | Sysno::delete_module
        | Sysno::epoll_pwait2
        | Sysno::fanotify_init
        | Sysno::fanotify_mark
        | Sysno::finit_module
        | Sysno::flock
        | Sysno::fsconfig
        | Sysno::fsmount
        | Sysno::fsopen
        | Sysno::fspick
        | Sysno::futex_requeue
        | Sysno::futex_wait
        | Sysno::futex_waitv
        | Sysno::futex_wake
        | Sysno::get_mempolicy
        | Sysno::getitimer
        | Sysno::init_module
        | Sysno::io_cancel
        | Sysno::io_destroy
        | Sysno::io_getevents
        | Sysno::io_pgetevents
        | Sysno::io_setup
        | Sysno::io_submit
        | Sysno::ioperm
        | Sysno::iopl
        | Sysno::kcmp
        | Sysno::kexec_file_load
        | Sysno::kexec_load
        | Sysno::keyctl
        | Sysno::landlock_add_rule
        | Sysno::landlock_create_ruleset
        | Sysno::landlock_restrict_self
        | Sysno::listmount
        | Sysno::lsm_get_self_attr
        | Sysno::lsm_list_modules
        | Sysno::lsm_set_self_attr
        | Sysno::map_shadow_stack
        | Sysno::mbind
        | Sysno::memfd_secret
        | Sysno::migrate_pages
        | Sysno::mincore
        | Sysno::mknod
        | Sysno::mknodat
        | Sysno::modify_ldt
        | Sysno::mount
        | Sysno::mount_setattr
        | Sysno::move_mount
        | Sysno::move_pages
        | Sysno::mq_getsetattr
        | Sysno::mq_notify
        | Sysno::mq_open
        | Sysno::mq_timedreceive
        | Sysno::mq_timedsend
        | Sysno::mq_unlink
        | Sysno::msgctl
        | Sysno::msgget
        | Sysno::msgrcv
        | Sysno::msgsnd
        | Sysno::name_to_handle_at
        | Sysno::open_by_handle_at
        | Sysno::open_tree
        | Sysno::openat2
        | Sysno::perf_event_open
        | Sysno::personality
        | Sysno::pidfd_getfd
        | Sysno::pidfd_open
        | Sysno::pidfd_send_signal
        | Sysno::pivot_root
        | Sysno::pkey_alloc
        | Sysno::pkey_free
        | Sysno::pkey_mprotect
        | Sysno::preadv
        | Sysno::preadv2
        | Sysno::process_mrelease
        | Sysno::process_vm_readv
        | Sysno::process_vm_writev
        | Sysno::ptrace
        | Sysno::pwritev
        | Sysno::pwritev2
        | Sysno::quotactl
        | Sysno::quotactl_fd
        | Sysno::readv
        | Sysno::reboot
        | Sysno::recvmmsg
        | Sysno::request_key
        | Sysno::restart_syscall
        | Sysno::rt_sigqueueinfo
        | Sysno::rt_tgsigqueueinfo
        | Sysno::sched_get_priority_max
        | Sysno::sched_get_priority_min
        | Sysno::sched_getattr
        | Sysno::sched_getparam
        | Sysno::sched_getscheduler
        | Sysno::sched_rr_get_interval
        | Sysno::sched_setattr
        | Sysno::sched_setparam
        | Sysno::sched_setscheduler
        | Sysno::seccomp
        | Sysno::select
        | Sysno::semctl
        | Sysno::semget
        | Sysno::semop
        | Sysno::semtimedop
        | Sysno::sendfile
        | Sysno::set_mempolicy
        | Sysno::set_mempolicy_home_node
        | Sysno::setdomainname
        | Sysno::sethostname
        | Sysno::setns
        | Sysno::settimeofday
        | Sysno::shmat
        | Sysno::shmctl
        | Sysno::shmdt
        | Sysno::shmget
        | Sysno::splice
        | Sysno::statmount
        | Sysno::swapoff
        | Sysno::swapon
        | Sysno::syslog
        | Sysno::tee
        | Sysno::times
        | Sysno::tkill
        | Sysno::umount2
        | Sysno::unshare
        // Still implemented on modern kernels (returns EINVAL for bad args, not
        // ENOSYS), so it is kept fail-closed rather than deterministically refused.
        | Sysno::ustat
        | Sysno::vhangup
        | Sysno::vmsplice => SyscallClassification::Unimplemented,
        // ===== END UNCLASSIFIED =====

        // `Sysno` is `#[non_exhaustive]` outside its crate. The const ABI guards above
        // make changes to the pinned table a compile error; this arm only satisfies the
        // external-enum language requirement and deliberately fails closed.
        _unexpected => panic!("unclassified Sysno outside pinned ABI"),
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#715): Review the deterministic ENOSYS refusal set.
/// Subset of [`SyscallClassification::Determinized`] that Detcore refuses with a
/// deterministic `ENOSYS`. These syscalls are obsolete, removed, or never
/// implemented on modern x86_64 Linux, so the kernel itself returns `ENOSYS`;
/// refusing them deterministically is a no-op relative to a real kernel and
/// removes them from the fail-closed backlog without any behavior regression.
/// Keep this list in sync with the "Deterministic refusal" block of
/// [`classify_syscall`]; a unit test enforces that every member classifies as
/// `Determinized`.
pub(crate) const fn is_deterministically_refused(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::_sysctl
            | Sysno::afs_syscall
            | Sysno::create_module
            | Sysno::get_kernel_syms
            | Sysno::getpmsg
            | Sysno::lookup_dcookie
            | Sysno::nfsservctl
            | Sysno::putpmsg
            | Sysno::query_module
            | Sysno::security
            | Sysno::sysfs
            | Sysno::tuxcall
            | Sysno::uselib
            | Sysno::vserver
    )
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#715): Review the audited safe-passthrough set.
/// Subset of [`SyscallClassification::PassThrough`] that is forwarded by `Sysno`
/// rather than through the typed `Syscall` dispatch table, because the pinned
/// Reverie revision exposes several of these only as raw calls. Every member is
/// non-blocking and deterministic under Hermit's fixed-container identity, stable
/// filesystem, and serialized single-CPU model. Keep this list in sync with the
/// "Audited safe passthroughs" block of [`classify_syscall`]; a unit test
/// enforces that every member classifies as `PassThrough`.
pub(crate) const fn is_extra_passthrough(sysno: Sysno) -> bool {
    matches!(
        sysno,
        Sysno::close_range
            | Sysno::get_robust_list
            | Sysno::get_thread_area
            | Sysno::ioprio_get
            | Sysno::ioprio_set
            | Sysno::mlock
            | Sysno::mlock2
            | Sysno::mlockall
            | Sysno::remap_file_pages
            | Sysno::set_thread_area
            | Sysno::setfsgid
            | Sysno::setfsuid
            | Sysno::setgid
            | Sysno::setgroups
            | Sysno::setregid
            | Sysno::setresgid
            | Sysno::setresuid
            | Sysno::setreuid
            | Sysno::setuid
            | Sysno::shutdown
            | Sysno::sync
            | Sysno::syncfs
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pinned_sysno_has_an_explicit_classification() {
        let mut counts = [0usize; 3];
        // syscalls 0.6.18 `Sysno::iter()` omits `last()` due its strict loop bound.
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            match classify_syscall(sysno) {
                SyscallClassification::Determinized => counts[0] += 1,
                SyscallClassification::PassThrough => counts[1] += 1,
                SyscallClassification::Unimplemented => counts[2] += 1,
            }
        }

        // 128 handled + 14 deterministic-ENOSYS-refusal = 142 Determinized;
        // 74 existing + 22 audited safe passthroughs = 96 PassThrough;
        // 135 audited determinization backlog = Unimplemented.
        assert_eq!(counts, [142, 96, 135]);
        assert_eq!(counts.iter().sum::<usize>(), EXPECTED_X86_64_SYSNO_COUNT);
    }

    #[test]
    fn refusal_and_extra_passthrough_helpers_match_the_table() {
        // Every deterministically-refused syscall must classify as Determinized so
        // the dispatch guard in lib.rs is only ever reached for a Determinized call.
        // Every extra passthrough must classify as PassThrough for the same reason.
        // The two helper sets must also be disjoint.
        for sysno in Sysno::iter().chain(std::iter::once(Sysno::last())) {
            if is_deterministically_refused(sysno) {
                assert_eq!(
                    classify_syscall(sysno),
                    SyscallClassification::Determinized,
                    "{sysno:?} is refused but not Determinized",
                );
                assert!(
                    !is_extra_passthrough(sysno),
                    "{sysno:?} is both refused and extra-passthrough",
                );
            }
            if is_extra_passthrough(sysno) {
                assert_eq!(
                    classify_syscall(sysno),
                    SyscallClassification::PassThrough,
                    "{sysno:?} is extra-passthrough but not PassThrough",
                );
            }
        }
    }

    #[test]
    fn representative_policies_stay_in_their_reviewed_sections() {
        assert_eq!(
            classify_syscall(Sysno::futex),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::nanosleep),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::lseek),
            SyscallClassification::PassThrough
        );
        assert_eq!(
            classify_syscall(Sysno::ppoll),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::arch_prctl),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::prlimit64),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::pwrite64),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::madvise),
            SyscallClassification::Determinized
        );
        assert_eq!(
            classify_syscall(Sysno::writev),
            SyscallClassification::Determinized
        );
        for sysno in [
            Sysno::clock_settime,
            Sysno::getpeername,
            Sysno::getsockname,
            Sysno::getsockopt,
            Sysno::getpriority,
            Sysno::getrlimit,
            Sysno::kill,
            Sysno::listen,
            Sysno::prctl,
            Sysno::rt_sigpending,
            Sysno::setitimer,
            Sysno::setpriority,
            Sysno::process_madvise,
            Sysno::setrlimit,
            Sysno::setsockopt,
            Sysno::tgkill,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
        }
        for sysno in [
            Sysno::capget,
            Sysno::capset,
            Sysno::chown,
            Sysno::chdir,
            Sysno::chmod,
            Sysno::faccessat,
            Sysno::faccessat2,
            Sysno::fchdir,
            Sysno::fchmod,
            Sysno::fchmodat,
            Sysno::fchmodat2,
            Sysno::fchown,
            Sysno::fchownat,
            Sysno::fdatasync,
            Sysno::fallocate,
            Sysno::fgetxattr,
            Sysno::flistxattr,
            Sysno::fremovexattr,
            Sysno::fsetxattr,
            Sysno::ftruncate,
            Sysno::fsync,
            Sysno::getresgid,
            Sysno::getresuid,
            Sysno::munlock,
            Sysno::munlockall,
            Sysno::readlinkat,
            Sysno::rename,
            Sysno::renameat,
            Sysno::getgroups,
            Sysno::getppid,
            Sysno::getxattr,
            Sysno::lchown,
            Sysno::getpgid,
            Sysno::getpgrp,
            Sysno::getsid,
            Sysno::setpgid,
            Sysno::lgetxattr,
            Sysno::link,
            Sysno::linkat,
            Sysno::listxattr,
            Sysno::llistxattr,
            Sysno::lremovexattr,
            Sysno::lsetxattr,
            Sysno::mkdir,
            Sysno::mkdirat,
            Sysno::msync,
            Sysno::removexattr,
            Sysno::readahead,
            Sysno::renameat2,
            Sysno::readlinkat,
            Sysno::rmdir,
            Sysno::rt_sigreturn,
            Sysno::setxattr,
            Sysno::symlink,
            Sysno::symlinkat,
            Sysno::sync_file_range,
            Sysno::truncate,
            Sysno::umask,
            Sysno::unlink,
            Sysno::unlinkat,
        ] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::PassThrough);
        }
        // Audited determinization backlog stays fail-closed under --strict.
        for sysno in [Sysno::add_key, Sysno::keyctl, Sysno::request_key] {
            assert_eq!(
                classify_syscall(sysno),
                SyscallClassification::Unimplemented
            );
        }
        // Obsolete/removed syscalls are Determinized via deterministic ENOSYS refusal.
        for sysno in [Sysno::_sysctl, Sysno::uselib, Sysno::vserver] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::Determinized);
            assert!(is_deterministically_refused(sysno));
        }
        // Audited safe, non-blocking, deterministic passthroughs.
        for sysno in [Sysno::setuid, Sysno::close_range, Sysno::sync] {
            assert_eq!(classify_syscall(sysno), SyscallClassification::PassThrough);
            assert!(is_extra_passthrough(sysno));
        }
    }
}
