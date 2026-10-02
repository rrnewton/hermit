//! Entry boundary for the first authenticated, single-root Record route.
//! This is a supported-operation boundary, not a guest errno emulation. Unknown
//! operations stop the run before submission; the complete mutation and Replay
//! topology obligations remain separate from this initial admission.

use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

pub(crate) fn initial_record_call_supported(call: Syscall) -> bool {
    match call {
        Syscall::Socketpair(pair) => pair.family() == libc::AF_UNIX
            && pair.r#type() & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) == libc::SOCK_STREAM
            && pair.protocol() == 0,
        // A positive Socket must join the exact original installation and held
        // profile. The one non-TCP creator below is allocation-only: its
        // published OFD rejects communication before native submission.
        Syscall::Socket(socket) => {
            (matches!(socket.family(), libc::AF_INET | libc::AF_INET6)
                && socket.r#type() & !(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
                    == libc::SOCK_STREAM
                && matches!(socket.protocol(), 0 | libc::IPPROTO_TCP))
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-3464): Keep the actual original Socket
                // installation/Close, with no UDP communication permission.
                // https://github.com/rrnewton/hermit/pull/3464
                || super::original_installation::is_udp6_capability_probe(
                    socket.family(), socket.r#type(), socket.protocol(),
                )
        }
        Syscall::Fcntl(fcntl) => matches!(
            fcntl.cmd(),
            reverie::syscalls::FcntlCmd::F_GETFL
                | reverie::syscalls::FcntlCmd::F_GETFD
                | reverie::syscalls::FcntlCmd::F_SETFL(_)
        ),
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-3464): These two output-only terminal queries
        // retain handle_ioctl's ordinary syscall/output/error path. Neither
        // changes descriptor flags or consumes/transmits network data. Do not
        // admit arbitrary ioctl commands, including descriptor/control changes.
        // https://github.com/rrnewton/hermit/pull/3464
        Syscall::Ioctl(ioctl) => matches!(
            ioctl.request(),
            reverie::syscalls::ioctl::Request::TCGETS(_)
                | reverie::syscalls::ioctl::Request::TIOCGWINSZ(_)
        ),
        _ => matches!(
            call.number(),
            Sysno::read | Sysno::pread64 | Sysno::write | Sysno::close
                | Sysno::dup | Sysno::dup2 | Sysno::dup3
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-3464): Pipe uses one original physical
                // table transaction, checked native FIFO endpoints and a
                // complete two-result publication. Socketpair is not implied.
                // https://github.com/rrnewton/hermit/pull/3464
                | Sysno::pipe | Sysno::pipe2
                | Sysno::connect | Sysno::bind | Sysno::listen
                | Sysno::accept | Sysno::accept4 | Sysno::shutdown
                | Sysno::getsockname | Sysno::getpeername
                | Sysno::getsockopt | Sysno::setsockopt
                | Sysno::fstat | Sysno::lseek
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-3464): Keep filesystem queries on the
                // ordinary Detcore handlers, including statfs canonicalization
                // and Linux errors. Neither call changes the descriptor table
                // or consumes network data, so no installation join is needed.
                // https://github.com/rrnewton/hermit/pull/3464
                | Sysno::statfs | Sysno::fstatfs
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-3464): Preserve handle_getdents64's
                // strict metadata sorting/inode virtualization, guest copy,
                // directory offset, EOF and Linux errors. This reads an
                // existing directory; it does not install descriptors or
                // consume/transmit a network stream.
                // https://github.com/rrnewton/hermit/pull/3464
                | Sysno::getdents64
                | Sysno::brk | Sysno::mmap | Sysno::mprotect | Sysno::munmap
                | Sysno::madvise | Sysno::arch_prctl
                | Sysno::rt_sigaction | Sysno::rt_sigprocmask | Sysno::rt_sigreturn
                | Sysno::sigaltstack | Sysno::set_tid_address | Sysno::set_robust_list
                | Sysno::getpid | Sysno::gettid | Sysno::getppid
                | Sysno::getuid | Sysno::geteuid | Sysno::getgid | Sysno::getegid
                | Sysno::getrandom | Sysno::clock_gettime | Sysno::gettimeofday
                // AUTONOMOUS-BOT-IMPLEMENTED
                // TODO-HUMAN-REVIEW(PR-3464): time uses the existing logical
                // clock and tloc copy, never a native time result. The Tool
                // entry gate still refuses it when time virtualization is off.
                // https://github.com/rrnewton/hermit/pull/3464
                | Sysno::time
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
                // Clone-family calls use the retained native-birth owner: the
                // provider observes the exact kernel child/table inheritance,
                // and the scheduler consumes that held-generation projection
                // before either parent or child can publish descriptor state.
                | Sysno::clone | Sysno::clone3 | Sysno::fork | Sysno::vfork
                // These scalar synchronization and stream operations already
                // have exact Detcore handlers. Futex and poll join the
                // scheduler's modeled blocking state; recvfrom joins the
                // authenticated network physical-effect protocol. Keep sendto
                // and vectored/message variants refused until their distinct
                // transmission, guest-memory, and descriptor-transfer joins
                // are complete.
                | Sysno::futex | Sysno::poll | Sysno::recvfrom
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
    fn initial_record_admits_only_local_stream_socketpair_shape() {
        let pair = |domain, kind, protocol| Syscall::from(
            reverie::syscalls::Socketpair::new().with_family(domain)
                .with_type(kind).with_protocol(protocol));
        for flags in [0, libc::SOCK_NONBLOCK, libc::SOCK_CLOEXEC,
            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC] {
            assert!(initial_record_call_supported(pair(libc::AF_UNIX,
                libc::SOCK_STREAM | flags, 0)));
        }
        for (domain, kind, protocol) in [
            (libc::AF_INET, libc::SOCK_STREAM, 0),
            (libc::AF_INET6, libc::SOCK_STREAM, 0),
            (libc::AF_UNIX, libc::SOCK_DGRAM, 0),
            (libc::AF_UNIX, libc::SOCK_SEQPACKET, 0),
            (libc::AF_UNIX, libc::SOCK_STREAM, libc::IPPROTO_TCP),
            (libc::AF_UNIX, libc::SOCK_STREAM | 0x4000_0000, 0),
        ] {
            assert!(!initial_record_call_supported(pair(domain, kind, protocol)));
        }
        assert!(!initial_record_call_supported(raw(Sysno::socketpair)));
    }

    #[test]
    fn initial_record_admits_only_exact_udp6_capability_probe() {
        let socket = |domain, kind, protocol| Syscall::from(
            reverie::syscalls::Socket::new().with_family(domain).with_type(kind).with_protocol(protocol)
        );
        assert!(initial_record_call_supported(socket(libc::AF_INET6, libc::SOCK_DGRAM, 0)));
        for (domain, kind, protocol) in [
            (libc::AF_INET, libc::SOCK_DGRAM, 0),
            (libc::AF_INET6, libc::SOCK_DGRAM, libc::IPPROTO_UDP),
            (libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0),
            (libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK, 0),
            (libc::AF_UNIX, libc::SOCK_STREAM, 0),
            (libc::AF_INET6, libc::SOCK_RAW, 0),
        ] {
            assert!(!initial_record_call_supported(socket(domain, kind, protocol)));
        }
    }
    #[test]
    fn initial_record_refuses_all_unjoined_descriptor_creators_and_table_changes() {
        for number in [
            Sysno::open,
            Sysno::openat2,
            Sysno::creat,
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
    fn initial_record_admits_determinized_filesystem_queries() {
        // curl probes filesystem geometry during initialization. These calls
        // keep the ordinary deterministic handlers and cannot create, replace,
        // or close a descriptor, nor consume or transmit a network stream.
        for number in [Sysno::statfs, Sysno::fstatfs] {
            assert!(initial_record_call_supported(raw(number)), "{number:?}");
        }
        // Filesystem observation does not authorize mount/namespace changes.
        for number in [Sysno::mount, Sysno::umount2, Sysno::unshare, Sysno::setns] {
            assert!(!initial_record_call_supported(raw(number)), "{number:?}");
        }
    }
    #[test]
    fn initial_record_admits_existing_directory_read_without_other_fd_creators() {
        assert!(initial_record_call_supported(raw(Sysno::getdents64)));
        // Classification leaves bad descriptors/pointers and buffer sizes to
        // the unchanged ordinary handler rather than manufacturing a result.
        assert!(initial_record_call_supported(Syscall::from_raw(
            Sysno::getdents64,
            reverie::syscalls::SyscallArgs::new(usize::MAX, 0, 0, 0, 0, 0),
        )));
        for number in [
            Sysno::getdents,
            Sysno::openat2,
            Sysno::creat,
            Sysno::eventfd2,
        ] {
            assert!(!initial_record_call_supported(raw(number)), "{number:?}");
        }
    }
    #[test]
    fn initial_record_admits_only_exact_terminal_query_ioctls() {
        let ioctl = |request, fd, address| {
            Syscall::from_raw(
                Sysno::ioctl,
                reverie::syscalls::SyscallArgs::new(fd, request, address, 0, 0, 0),
            )
        };
        for request in [libc::TCGETS, libc::TIOCGWINSZ] {
            for (fd, address) in [(0, 4096), (1, 8192), (usize::MAX, 0)] {
                assert!(initial_record_call_supported(ioctl(
                    request as usize,
                    fd,
                    address,
                )));
            }
        }
        // Neighboring setters, FD flag/allocator operations, other observations
        // and unknown commands remain refused even with a valid-looking pointer.
        for request in [
            0,
            libc::TCSETS,
            libc::TIOCSWINSZ,
            libc::FIONBIO,
            libc::FIOCLEX,
            libc::FIONCLEX,
            libc::FIONREAD,
            0x5441, // TIOCGPTPEER installs a new descriptor.
            0xffff_ffff,
        ] {
            assert!(
                !initial_record_call_supported(ioctl(request as usize, 1, 4096)),
                "ioctl request {request:#x}",
            );
        }
    }
    #[test]
    fn initial_record_admits_exactly_joined_descriptor_aliases() {
        for number in [Sysno::dup, Sysno::dup2, Sysno::dup3] {
            assert!(initial_record_call_supported(raw(number)), "{number:?}");
        }
        assert!(!initial_record_call_supported(raw(Sysno::socketpair)));
    }
    #[test]
    fn initial_record_admits_only_joined_pipe_pair_creators() {
        for number in [Sysno::pipe, Sysno::pipe2] {
            assert!(initial_record_call_supported(raw(number)), "{number:?}");
        }
        for number in [Sysno::socketpair, Sysno::eventfd2, Sysno::recvmsg] {
            assert!(!initial_record_call_supported(raw(number)), "{number:?}");
        }
    }
    #[test]
    fn initial_record_admits_exactly_joined_clone_family() {
        for number in [Sysno::clone, Sysno::clone3, Sysno::fork, Sysno::vfork] {
            assert!(initial_record_call_supported(raw(number)), "{number:?}");
        }

        // These neighboring task/table mutations have no native-effect join.
        for number in [Sysno::execve, Sysno::execveat, Sysno::unshare, Sysno::setns] {
            assert!(!initial_record_call_supported(raw(number)), "{number:?}");
        }
    }
    #[test]
    fn initial_record_admits_modeled_thread_and_scalar_stream_effects() {
        for number in [Sysno::futex, Sysno::poll, Sysno::recvfrom] {
            assert!(initial_record_call_supported(raw(number)), "{number:?}");
        }

        // Message/vector operations have different guest-memory and descriptor
        // transfer contracts; the scalar admission must not authorize them.
        for number in [
            Sysno::sendto,
            Sysno::sendmsg,
            Sysno::recvmsg,
            Sysno::sendmmsg,
            Sysno::recvmmsg,
        ] {
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
