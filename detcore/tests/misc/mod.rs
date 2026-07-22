/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! misc syscall tests

use nix::unistd;

mod vfork;

#[global_allocator]
static ALLOC: test_allocator::Global = test_allocator::Global;

#[derive(Clone, Copy)]
struct HardwareRandomFeatures {
    rdrand: bool,
    rdseed: bool,
}

fn hardware_random_features() -> HardwareRandomFeatures {
    let cpuid = raw_cpuid::CpuId::new();
    HardwareRandomFeatures {
        rdrand: cpuid.get_feature_info().is_some_and(|f| f.has_rdrand()),
        rdseed: cpuid
            .get_extended_feature_info()
            .is_some_and(|f| f.has_rdseed()),
    }
}

fn cpuid_faulting_supported() -> bool {
    const ARCH_SET_CPUID: libc::c_int = 0x1012;

    let child = unsafe { libc::fork() };
    assert!(child >= 0, "failed to fork CPUID capability probe");
    if child == 0 {
        let result = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) };
        unsafe { libc::_exit(i32::from(result != 0)) };
    }

    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

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

#[test]
fn getrandom_intercepted() {
    reverie_ptrace::ret_without_perf!();
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
    let features = hardware_random_features();
    if !features.rdrand {
        eprintln!("host does not expose RDRAND; skipping host feature prerequisite");
        return;
    }

    if !features.rdseed {
        eprintln!("host exposes RDRAND without RDSEED; RDSEED is not required by this host test");
    }
}

#[test]
fn rdrand_rdseed_is_masked() {
    let features = hardware_random_features();
    if !features.rdrand && !features.rdseed {
        eprintln!("host exposes neither RDRAND nor RDSEED; skipping CPUID masking test");
        return;
    }
    if !cpuid_faulting_supported() {
        eprintln!("host does not support CPUID faulting; skipping CPUID masking test");
        return;
    }

    det_test_fn_without_pmu(|| {
        let cpuid = raw_cpuid::CpuId::new();
        let feature = cpuid
            .get_feature_info()
            .expect("virtual CPU should expose basic feature information");
        assert!(!feature.has_rdrand());

        let feature_ext = cpuid
            .get_extended_feature_info()
            .expect("virtual CPU should expose extended feature information");
        assert!(!feature_ext.has_rdseed());
    })
}
