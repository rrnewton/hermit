/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! misc syscall tests

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nix::unistd;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

fn det_test_fn_without_pmu<F>(f: F)
where
    F: Fn(),
{
    let config = detcore::Config {
        preemption_timeout: None,
        ..Default::default()
    };
    detcore_testutils::det_test_fn_with_config(true, f, config, detcore_testutils::expect_success)
}

#[test]
fn ppoll_ready_and_timeout() {
    det_test_fn_without_pmu(|| {
        let mut pipefds = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipefds.as_mut_ptr()) }, 0);

        let byte = b"x";
        assert_eq!(
            unsafe { libc::write(pipefds[1], byte.as_ptr().cast(), byte.len()) },
            1
        );
        let mut pollfd = libc::pollfd {
            fd: pipefds[0],
            events: libc::POLLIN,
            revents: 0,
        };
        let mut ready_timeout = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_ppoll,
                    &mut pollfd,
                    1,
                    &mut ready_timeout,
                    std::ptr::null::<libc::sigset_t>(),
                    0,
                )
            },
            1
        );
        assert_ne!(pollfd.revents & libc::POLLIN, 0);
        assert!(ready_timeout.tv_sec >= 0);
        assert!((ready_timeout.tv_sec, ready_timeout.tv_nsec) > (0, 0));
        assert!((ready_timeout.tv_sec, ready_timeout.tv_nsec) < (1, 0));

        let mut expired_timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_ppoll,
                    std::ptr::null_mut::<libc::pollfd>(),
                    0,
                    &mut expired_timeout,
                    std::ptr::null::<libc::sigset_t>(),
                    0,
                )
            },
            0
        );
        assert_eq!((expired_timeout.tv_sec, expired_timeout.tv_nsec), (0, 0));
        assert_eq!(unsafe { libc::close(pipefds[0]) }, 0);
        assert_eq!(unsafe { libc::close(pipefds[1]) }, 0);
    });
}

#[test]
fn select_ready_and_timeout() {
    det_test_fn_without_pmu(|| {
        let mut pipefds = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipefds.as_mut_ptr()) }, 0);

        let writer_fd = pipefds[1];
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            let byte = b"s";
            assert_eq!(
                unsafe { libc::write(writer_fd, byte.as_ptr().cast(), byte.len()) },
                1
            );
        });

        let mut readfds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe { libc::FD_SET(pipefds[0], &mut readfds) };
        let mut ready_timeout = libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_select,
                    pipefds[0] + 1,
                    &mut readfds,
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    &mut ready_timeout,
                )
            },
            1
        );
        assert!(unsafe { libc::FD_ISSET(pipefds[0], &readfds) });
        assert!((ready_timeout.tv_sec, ready_timeout.tv_usec) > (0, 0));
        assert!((ready_timeout.tv_sec, ready_timeout.tv_usec) < (1, 0));
        writer.join().unwrap();

        let mut byte = [0_u8; 1];
        assert_eq!(
            unsafe { libc::read(pipefds[0], byte.as_mut_ptr().cast(), byte.len()) },
            1
        );

        unsafe { libc::FD_SET(pipefds[0], &mut readfds) };
        let mut expired_timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 1_000,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_select,
                    pipefds[0] + 1,
                    &mut readfds,
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    &mut expired_timeout,
                )
            },
            0
        );
        assert!(!unsafe { libc::FD_ISSET(pipefds[0], &readfds) });
        assert_eq!((expired_timeout.tv_sec, expired_timeout.tv_usec), (0, 0));
        assert_eq!(unsafe { libc::close(pipefds[0]) }, 0);
        assert_eq!(unsafe { libc::close(pipefds[1]) }, 0);
    });
}

#[test]
fn select_fd_set_abi_boundaries() {
    det_test_fn_without_pmu(|| {
        let invalid_set = 1_usize as *mut libc::fd_set;
        let mut timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 1_000,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_select,
                    0,
                    invalid_set,
                    invalid_set,
                    invalid_set,
                    &mut timeout,
                )
            },
            0
        );

        timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 1_000,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_select,
                    -1,
                    invalid_set,
                    invalid_set,
                    invalid_set,
                    &mut timeout,
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );

        timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 1_000,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_select,
                    1,
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    &mut timeout,
                )
            },
            0
        );

        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size * 2,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(mapping, libc::MAP_FAILED);
        let protected_page = unsafe { mapping.cast::<u8>().add(page_size) };
        assert_eq!(
            unsafe { libc::mprotect(protected_page.cast(), page_size, libc::PROT_NONE) },
            0
        );
        let one_word_set = unsafe {
            protected_page
                .sub(std::mem::size_of::<libc::c_ulong>())
                .cast::<libc::fd_set>()
        };
        unsafe { one_word_set.cast::<libc::c_ulong>().write(0) };
        timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 1_000,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_select,
                    1,
                    one_word_set,
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    &mut timeout,
                )
            },
            0
        );
        assert_eq!(unsafe { libc::munmap(mapping, page_size * 2) }, 0);
    });
}

#[test]
fn pselect6_ready_and_timeout() {
    #[derive(Clone, Copy)]
    #[repr(C)]
    struct Pselect6SigmaskArg {
        sigmask: *const libc::sigset_t,
        sigsetsize: usize,
    }

    det_test_fn_without_pmu(|| {
        let mut pipefds = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipefds.as_mut_ptr()) }, 0);

        let writer_fd = pipefds[1];
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            let byte = b"p";
            assert_eq!(
                unsafe { libc::write(writer_fd, byte.as_ptr().cast(), byte.len()) },
                1
            );
        });

        let mut readfds: libc::fd_set = unsafe { std::mem::zeroed() };
        unsafe { libc::FD_SET(pipefds[0], &mut readfds) };
        let mut ready_timeout = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        let sigmask_arg = Pselect6SigmaskArg {
            sigmask: std::ptr::null(),
            sigsetsize: std::mem::size_of::<libc::sigset_t>(),
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pselect6,
                    pipefds[0] + 1,
                    &mut readfds,
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    &mut ready_timeout,
                    &sigmask_arg,
                )
            },
            1
        );
        assert!(unsafe { libc::FD_ISSET(pipefds[0], &readfds) });
        assert!((ready_timeout.tv_sec, ready_timeout.tv_nsec) > (0, 0));
        assert!((ready_timeout.tv_sec, ready_timeout.tv_nsec) < (1, 0));
        writer.join().unwrap();

        let mut byte = [0_u8; 1];
        assert_eq!(
            unsafe { libc::read(pipefds[0], byte.as_mut_ptr().cast(), byte.len()) },
            1
        );

        unsafe { libc::FD_SET(pipefds[0], &mut readfds) };
        let mut expired_timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pselect6,
                    pipefds[0] + 1,
                    &mut readfds,
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    &mut expired_timeout,
                    &sigmask_arg,
                )
            },
            0
        );
        assert!(!unsafe { libc::FD_ISSET(pipefds[0], &readfds) });
        assert_eq!((expired_timeout.tv_sec, expired_timeout.tv_nsec), (0, 0));
        assert_eq!(unsafe { libc::close(pipefds[0]) }, 0);
        assert_eq!(unsafe { libc::close(pipefds[1]) }, 0);
    });
}

static PSELECT_SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn pselect_signal_handler(_: libc::c_int) {
    PSELECT_SIGNAL_RECEIVED.store(true, Ordering::SeqCst);
}

#[test]
fn pselect6_temporarily_unmasks_signal() {
    #[derive(Clone, Copy)]
    #[repr(C)]
    struct Pselect6SigmaskArg {
        sigmask: *const libc::sigset_t,
        sigsetsize: usize,
    }

    det_test_fn_without_pmu(|| {
        PSELECT_SIGNAL_RECEIVED.store(false, Ordering::SeqCst);

        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = pselect_signal_handler as *const () as usize;
        assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()) },
            0
        );

        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::sigemptyset(&mut blocked) }, 0);
        assert_eq!(unsafe { libc::sigaddset(&mut blocked, libc::SIGUSR1) }, 0);
        let mut old_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut old_mask) },
            0
        );

        let main_thread = unsafe { libc::pthread_self() };
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            assert_eq!(unsafe { libc::pthread_kill(main_thread, libc::SIGUSR1) }, 0);
        });

        let mut temporary_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::sigemptyset(&mut temporary_mask) }, 0);
        let sigmask_arg = Pselect6SigmaskArg {
            sigmask: &temporary_mask,
            sigsetsize: std::mem::size_of::<libc::c_ulong>(),
        };
        let mut timeout = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pselect6,
                    0,
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    std::ptr::null_mut::<libc::fd_set>(),
                    &mut timeout,
                    &sigmask_arg,
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINTR)
        );
        sender.join().unwrap();
        assert!(PSELECT_SIGNAL_RECEIVED.load(Ordering::SeqCst));

        let mut current_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut current_mask)
            },
            0
        );
        assert_eq!(
            unsafe { libc::sigismember(&current_mask, libc::SIGUSR1) },
            1
        );
        assert_eq!(
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &old_mask, std::ptr::null_mut()) },
            0
        );
    });
}

#[test]
fn vectored_socket_io() {
    det_test_fn_without_pmu(|| {
        let mut sockets = [0; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sockets.as_mut_ptr(),) },
            0
        );

        let first = b"qe";
        let second = b"mu";
        let write_iov = [
            libc::iovec {
                iov_base: first.as_ptr() as *mut libc::c_void,
                iov_len: first.len(),
            },
            libc::iovec {
                iov_base: second.as_ptr() as *mut libc::c_void,
                iov_len: second.len(),
            },
        ];
        assert_eq!(unsafe { libc::writev(sockets[0], std::ptr::null(), 1) }, -1);
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EFAULT);
        assert_eq!(
            unsafe { libc::writev(sockets[0], write_iov.as_ptr(), 2) },
            4
        );

        let mut first_out = [0; 2];
        let mut second_out = [0; 2];
        let mut read_iov = [
            libc::iovec {
                iov_base: first_out.as_mut_ptr().cast(),
                iov_len: first_out.len(),
            },
            libc::iovec {
                iov_base: second_out.as_mut_ptr().cast(),
                iov_len: second_out.len(),
            },
        ];
        assert_eq!(unsafe { libc::readv(sockets[1], std::ptr::null(), 1) }, -1);
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EFAULT);
        assert_eq!(
            unsafe { libc::readv(sockets[1], read_iov.as_mut_ptr(), 2) },
            4
        );
        assert_eq!(&first_out, b"qe");
        assert_eq!(&second_out, b"mu");
        assert_eq!(unsafe { libc::close(sockets[0]) }, 0);
        assert_eq!(unsafe { libc::close(sockets[1]) }, 0);
    });
}

#[test]
fn futex_wait_bitset_realtime_timeout() {
    det_test_fn_without_pmu(|| {
        let futex = 0_u32;
        let mut timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut timeout) },
            0
        );
        timeout.tv_nsec += 1_000_000;
        if timeout.tv_nsec >= 1_000_000_000 {
            timeout.tv_sec += 1;
            timeout.tv_nsec -= 1_000_000_000;
        }

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &futex,
                    libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG | libc::FUTEX_CLOCK_REALTIME,
                    0,
                    &timeout,
                    std::ptr::null::<u32>(),
                    libc::FUTEX_BITSET_MATCH_ANY,
                )
            },
            -1
        );
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::ETIMEDOUT);
    });
}

/// Tests SYS_uname
#[test]
fn getrandom_intercepted() {
    detcore_testutils::det_test_fn(|| {
        let mut got: u64 = 0;
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_getrandom, &mut got as *const u64 as u64, 8, 0) },
            8
        );
        println!("SYS_getrandom 1st result: {}", got);

        let dev_urandom = b"/dev/urandom\0";
        let fd = unsafe { libc::open(dev_urandom[..].as_ptr() as *const _, libc::O_RDONLY, 0o644) };
        assert!(fd >= 0);

        assert_eq!(
            unsafe { libc::syscall(libc::SYS_read, fd, &mut got as *const u64 as u64, 8) },
            8
        );
        println!("/dev/urandom result: {}", got);
        assert!(unistd::close(fd).is_ok());

        let dev_random = b"/dev/random\0";
        let fd = unsafe { libc::open(dev_random[..].as_ptr() as *const _, libc::O_RDONLY, 0o644) };
        assert!(fd >= 0);

        assert_eq!(
            unsafe { libc::syscall(libc::SYS_read, fd, &mut got as *const u64 as u64, 8) },
            8
        );
        println!("/dev/random result: {}", got);
        assert!(unistd::close(fd).is_ok());
    })
}

#[test]
fn has_rdrand_without_detcore() {
    let cpuid = raw_cpuid::CpuId::new();
    let feature = cpuid.get_feature_info();
    assert!(feature.is_some());
    let feature = feature.unwrap();
    assert!(feature.has_rdrand());

    if let Some(feature_ext) = cpuid.get_extended_feature_info() {
        assert!(feature_ext.has_rdseed());
    }
}

#[test]
fn rdrand_rdseed_is_masked() {
    detcore_testutils::det_test_fn(|| {
        let cpuid = raw_cpuid::CpuId::new();
        let feature = cpuid.get_feature_info();
        assert!(feature.is_some());
        let feature = feature.unwrap();
        assert!(!feature.has_rdrand());

        if let Some(feature_ext) = cpuid.get_extended_feature_info() {
            assert!(!feature_ext.has_rdseed());
        }
    })
}
