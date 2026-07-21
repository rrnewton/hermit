/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! misc syscall tests

use nix::unistd;

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

fn det_test_fn_sequential_without_pmu<F>(f: F)
where
    F: Fn(),
{
    let config = detcore::Config {
        preemption_timeout: None,
        sequentialize_threads: true,
        ..Default::default()
    };
    detcore_testutils::det_test_fn_with_config(true, f, config, detcore_testutils::expect_success)
}

#[test]
fn dup_shares_status_flags_but_not_cloexec() {
    det_test_fn_sequential_without_pmu(|| {
        let mut sockets = [0; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_NONBLOCK,
                    0,
                    sockets.as_mut_ptr(),
                )
            },
            0
        );

        let duplicate = unsafe { libc::fcntl(sockets[0], libc::F_DUPFD_CLOEXEC, 0) };
        assert!(duplicate >= 0);
        assert_ne!(
            unsafe { libc::fcntl(duplicate, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        assert_eq!(
            unsafe { libc::fcntl(sockets[0], libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_ne!(
            unsafe { libc::fcntl(duplicate, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );

        let mut byte = 0_u8;
        assert_eq!(
            unsafe { libc::read(duplicate, (&mut byte as *mut u8).cast(), 1) },
            -1
        );
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EAGAIN);

        assert_eq!(unsafe { libc::close(duplicate) }, 0);
        assert_eq!(unsafe { libc::close(sockets[0]) }, 0);
        assert_eq!(unsafe { libc::close(sockets[1]) }, 0);
    });
}

#[test]
fn unsupported_futex_keys_fail_explicitly() {
    det_test_fn_sequential_without_pmu(|| {
        let futex = 0_u32;
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_futex, &futex, libc::FUTEX_WAKE, 1) },
            -1
        );
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EOPNOTSUPP);

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &futex,
                    libc::FUTEX_WAKE_BITSET | libc::FUTEX_PRIVATE_FLAG,
                    1,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    0,
                )
            },
            -1
        );
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EINVAL);
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
