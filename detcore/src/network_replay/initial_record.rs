//! Entry boundary for the first authenticated, single-root Record route.
//! This is a supported-operation boundary, not a guest errno emulation. Unknown
//! operations stop the run before submission; the complete mutation and Replay
//! topology obligations remain separate from this initial admission.

use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

pub(crate) fn initial_record_call_supported(call: Syscall) -> bool {
    match call {
        // A positive Socket must join the exact original installation and held
        // profile. Non-TCP allocation is not activated through this route.
        Syscall::Socket(socket) => {
            matches!(socket.family(), libc::AF_INET | libc::AF_INET6)
                && socket.r#type() & !(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
                    == libc::SOCK_STREAM
                && matches!(socket.protocol(), 0 | libc::IPPROTO_TCP)
        }
        Syscall::Fcntl(fcntl) => matches!(
            fcntl.cmd(),
            reverie::syscalls::FcntlCmd::F_GETFL
                | reverie::syscalls::FcntlCmd::F_GETFD
                | reverie::syscalls::FcntlCmd::F_SETFL(_)
        ),
        _ => matches!(
            call.number(),
            Sysno::read | Sysno::pread64 | Sysno::write | Sysno::close
                | Sysno::dup | Sysno::dup2 | Sysno::dup3
                | Sysno::connect | Sysno::bind | Sysno::listen
                | Sysno::accept | Sysno::accept4 | Sysno::shutdown
                | Sysno::getsockname | Sysno::getpeername
                | Sysno::getsockopt | Sysno::setsockopt
                | Sysno::fstat | Sysno::lseek
                | Sysno::brk | Sysno::mmap | Sysno::mprotect | Sysno::munmap
                | Sysno::madvise | Sysno::arch_prctl
                | Sysno::rt_sigaction | Sysno::rt_sigprocmask | Sysno::rt_sigreturn
                | Sysno::sigaltstack | Sysno::set_tid_address | Sysno::set_robust_list
                | Sysno::getpid | Sysno::gettid | Sysno::getppid
                | Sysno::getuid | Sysno::geteuid | Sysno::getgid | Sysno::getegid
                | Sysno::getrandom | Sysno::clock_gettime | Sysno::gettimeofday
                | Sysno::uname | Sysno::sched_getaffinity | Sysno::prlimit64
                // Alarm is virtualized by the deterministic scheduler and has
                // no native descriptor or network-provider side effect.
                | Sysno::alarm
                // Access, newfstatat and pread64 have no descriptor-table
                // effect and are carried by the recorder/replayer's ordinary
                // effect stream. Metadata virtualization still owns the
                // newfstatat result and guest-memory copy.
                // Openat is the one admitted filesystem allocator: its handler
                // joins the native/recorded allocator protocol and requires the
                // exact published descriptor binding before it can return.
                | Sysno::access | Sysno::newfstatat | Sysno::openat
                // Neither touches the descriptor table: strict rseq returns
                // ENOSYS without entering Linux, and readlink reads a path.
                | Sysno::rseq | Sysno::readlink
                | Sysno::exit | Sysno::exit_group
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raw(number: Sysno) -> Syscall {
        Syscall::from_raw(
            number,
            reverie::syscalls::SyscallArgs::new(0, 0, 0, 0, 0, 0),
        )
    }
    #[test]
    fn initial_record_refuses_all_unjoined_descriptor_creators_and_table_changes() {
        for number in [
            Sysno::open,
            Sysno::openat2,
            Sysno::creat,
            Sysno::pipe,
            Sysno::pipe2,
            Sysno::socketpair,
            Sysno::recvmsg,
            Sysno::recvmmsg,
            Sysno::sendmsg,
            Sysno::sendmmsg,
            Sysno::epoll_create,
            Sysno::epoll_create1,
            Sysno::eventfd,
            Sysno::eventfd2,
            Sysno::signalfd,
            Sysno::signalfd4,
            Sysno::timerfd_create,
            Sysno::inotify_init,
            Sysno::inotify_init1,
            Sysno::userfaultfd,
            Sysno::pidfd_open,
            Sysno::pidfd_getfd,
            Sysno::close_range,
            Sysno::clone,
            Sysno::clone3,
            Sysno::fork,
            Sysno::vfork,
            Sysno::execve,
            Sysno::execveat,
            Sysno::unshare,
            Sysno::setns,
            Sysno::ioctl,
            Sysno::bpf,
            Sysno::io_uring_setup,
        ] {
            assert!(!initial_record_call_supported(raw(number)), "{number:?}");
        }
    }
    #[test]
    fn initial_record_admits_recorded_loader_reads_and_exactly_joined_openat() {
        for number in [
            Sysno::access,
            Sysno::newfstatat,
            Sysno::pread64,
            Sysno::openat,
        ] {
            assert!(initial_record_call_supported(raw(number)), "{number:?}");
        }

        // Negative controls: admitting the joined openat operation must not
        // admit sibling descriptor creators that lack its publication join.
        for number in [Sysno::open, Sysno::openat2, Sysno::creat] {
            assert!(!initial_record_call_supported(raw(number)), "{number:?}");
        }
    }
    #[test]
    fn initial_record_admits_exactly_joined_descriptor_aliases() {
        for number in [Sysno::dup, Sysno::dup2, Sysno::dup3] {
            assert!(initial_record_call_supported(raw(number)), "{number:?}");
        }
        for number in [Sysno::pipe, Sysno::pipe2, Sysno::socketpair] {
            assert!(!initial_record_call_supported(raw(number)), "{number:?}");
        }
    }
    #[test]
    fn initial_record_keeps_scalar_tcp_and_original_descriptor_operations() {
        for number in [
            Sysno::read,
            Sysno::write,
            Sysno::close,
            Sysno::connect,
            Sysno::bind,
            Sysno::listen,
            Sysno::accept,
            Sysno::accept4,
            Sysno::getsockopt,
            Sysno::setsockopt,
            Sysno::getsockname,
            Sysno::getpeername,
            Sysno::shutdown,
            Sysno::alarm,
            Sysno::rseq,
            Sysno::readlink,
            Sysno::exit,
            Sysno::exit_group,
        ] {
            assert!(initial_record_call_supported(raw(number)), "{number:?}");
        }
    }
}
