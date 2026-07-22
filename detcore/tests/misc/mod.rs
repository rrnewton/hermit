/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! misc syscall tests

mod notification_fds;

use std::os::fd::AsRawFd;

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

fn det_test_fn_sequential_without_pmu_or_metadata<F>(f: F)
where
    F: Fn(),
{
    let config = detcore::Config {
        preemption_timeout: None,
        sequentialize_threads: true,
        virtualize_metadata: false,
        ..Default::default()
    };
    detcore_testutils::det_test_fn_with_config(true, f, config, detcore_testutils::expect_success)
}

fn wait_on_one_futex_and_wake_another(
    wait_address: *mut u32,
    wake_address: *mut u32,
) -> (libc::c_long, libc::c_long, nix::errno::Errno) {
    let wait_address = wait_address as usize;
    let waiter = std::thread::spawn(move || {
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        let result = unsafe {
            libc::syscall(
                libc::SYS_futex,
                wait_address as *mut u32,
                libc::FUTEX_WAIT,
                0,
                &timeout,
                std::ptr::null::<u32>(),
                0,
            )
        };
        (result, nix::errno::Errno::last())
    });

    std::thread::sleep(std::time::Duration::from_millis(5));
    let woke = unsafe {
        libc::syscall(
            libc::SYS_futex,
            wake_address,
            libc::FUTEX_WAKE,
            1,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0,
        )
    };
    let (waited, wait_errno) = waiter.join().unwrap();
    (woke, waited, wait_errno)
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
fn bound_port_survives_closing_dup_alias() {
    det_test_fn_sequential_without_pmu(|| {
        fn bind_loopback_ephemeral(fd: libc::c_int) -> libc::c_int {
            let mut address = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: 0,
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes([127, 0, 0, 1]),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                libc::bind(
                    fd,
                    (&mut address as *mut libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }

        let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(socket >= 0);
        let mut first_bound = false;
        for _ in 0..128 {
            if bind_loopback_ephemeral(socket) == 0 {
                first_bound = true;
                break;
            }
            assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EADDRINUSE);
        }
        assert!(first_bound, "no deterministic ephemeral port was available");

        let duplicate = unsafe { libc::dup(socket) };
        assert!(duplicate >= 0);
        assert_eq!(unsafe { libc::close(socket) }, 0);

        let second = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(second >= 0);
        assert_eq!(
            bind_loopback_ephemeral(second),
            0,
            "closing one dup alias must not free its bound port reservation"
        );

        assert_eq!(unsafe { libc::close(duplicate) }, 0);
        assert_eq!(unsafe { libc::close(second) }, 0);
    });
}

#[test]
fn shared_futex_modes_are_supported_and_validate_bitsets() {
    det_test_fn_sequential_without_pmu(|| {
        let futex = 0_u32;
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_futex, &futex, libc::FUTEX_WAKE, 1) },
            0,
            "a shared-mode wake with no waiters should succeed"
        );
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
fn shared_anonymous_futex_wakes_across_processes() {
    det_test_fn_sequential_without_pmu(|| {
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(mapping, libc::MAP_FAILED);
        let futex = mapping.cast::<u32>();
        unsafe { futex.write(0) };

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork should succeed");
        if child == 0 {
            let waited = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    futex,
                    libc::FUTEX_WAIT,
                    0,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    0,
                )
            };
            unsafe { libc::_exit(i32::from(waited != 0)) };
        }

        let mut woke = 0;
        for _ in 0..1024 {
            woke = unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    futex,
                    libc::FUTEX_WAKE,
                    1,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    0,
                )
            };
            if woke == 1 {
                break;
            }
            assert_eq!(unsafe { libc::sched_yield() }, 0);
        }
        assert_eq!(
            woke, 1,
            "parent should wake the child through the shared mapping"
        );

        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        assert_eq!(unsafe { libc::munmap(mapping, 4096) }, 0);
    });
}

#[test]
fn distinct_shared_anonymous_mappings_do_not_alias_with_ignored_fd() {
    det_test_fn_sequential_without_pmu(|| {
        let map = || unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                libc::STDIN_FILENO,
                0,
            )
        };
        let first = map();
        let second = map();
        assert_ne!(first, libc::MAP_FAILED);
        assert_ne!(second, libc::MAP_FAILED);
        assert_ne!(first, second);

        let first_futex = first.cast::<u32>();
        let second_futex = second.cast::<u32>();
        unsafe {
            first_futex.write(0);
            second_futex.write(0);
        }
        let (woke, waited, wait_errno) =
            wait_on_one_futex_and_wake_another(first_futex, second_futex);
        assert_eq!(woke, 0, "independent anonymous mappings must not alias");
        assert_eq!(waited, -1);
        assert_eq!(wait_errno, nix::errno::Errno::ETIMEDOUT);

        assert_eq!(unsafe { libc::munmap(first, 4096) }, 0);
        assert_eq!(unsafe { libc::munmap(second, 4096) }, 0);
    });
}

#[test]
fn independent_opens_share_file_backed_futex_without_metadata_virtualization() {
    det_test_fn_sequential_without_pmu_or_metadata(|| {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(4096).unwrap();
        let reopened = file.reopen().unwrap();

        let map = |fd| unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        let first = map(file.as_raw_fd());
        let second = map(reopened.as_raw_fd());
        assert_ne!(first, libc::MAP_FAILED);
        assert_ne!(second, libc::MAP_FAILED);
        assert_ne!(first, second);

        let first_futex = first.cast::<u32>();
        let second_futex = second.cast::<u32>();
        unsafe { first_futex.write(0) };
        let (woke, waited, _) = wait_on_one_futex_and_wake_another(first_futex, second_futex);
        assert_eq!(woke, 1, "aliases of one inode and offset must share a key");
        assert_eq!(waited, 0);

        assert_eq!(unsafe { libc::munmap(first, 4096) }, 0);
        assert_eq!(unsafe { libc::munmap(second, 4096) }, 0);
    });
}

#[test]
fn dup2_same_fd_preserves_cloexec() {
    det_test_fn_sequential_without_pmu(|| {
        let path = b"/dev/null\0";
        let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(fd >= 0);
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );

        assert_eq!(unsafe { libc::dup2(fd, fd) }, fd);
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0,
            "dup2(fd, fd) must leave descriptor flags unchanged"
        );
        assert_eq!(unsafe { libc::close(fd) }, 0);
    });
}

#[test]
fn failed_exec_preserves_shared_fd_table() {
    det_test_fn_sequential_without_pmu(|| {
        use std::ffi::CString;
        use std::sync::Arc;
        use std::sync::atomic::AtomicI32;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc::sync_channel;

        let path = b"/dev/null\0";
        let original = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY) };
        assert!(original >= 0);

        let shared_fd = Arc::new(AtomicI32::new(-1));
        let worker_fd = Arc::clone(&shared_fd);
        let (exec_failed_tx, exec_failed_rx) = sync_channel(0);
        let (continue_tx, continue_rx) = sync_channel(0);
        let (finished_tx, finished_rx) = sync_channel(0);
        let worker = std::thread::spawn(move || {
            let missing = CString::new("/definitely/missing/hermit-exec").expect("valid path");
            let argv = [missing.as_ptr(), std::ptr::null()];
            let envp: [*const libc::c_char; 1] = [std::ptr::null()];
            assert_eq!(
                unsafe { libc::execve(missing.as_ptr(), argv.as_ptr(), envp.as_ptr()) },
                -1
            );
            assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::ENOENT);
            exec_failed_tx.send(()).expect("notify parent");
            continue_rx.recv().expect("wait for sibling mutation");

            let fd = worker_fd.load(Ordering::SeqCst);
            let mut byte = 0_u8;
            assert_eq!(
                unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) },
                0,
                "failed exec must restore the exact CLONE_FILES table"
            );
            finished_tx.send(()).expect("notify parent of completion");
        });

        exec_failed_rx.recv().expect("worker should fail exec");
        let duplicate = unsafe { libc::fcntl(original, libc::F_DUPFD, 0) };
        assert!(duplicate >= 0);
        shared_fd.store(duplicate, Ordering::SeqCst);
        continue_tx.send(()).expect("release worker");
        finished_rx
            .recv()
            .expect("worker should observe the duplicate");
        drop(worker);

        assert_eq!(unsafe { libc::close(duplicate) }, 0);
        assert_eq!(unsafe { libc::close(original) }, 0);
    });
}

#[test]
fn futex_wait_bitset_timeout_is_absolute_and_removes_waiter() {
    det_test_fn_sequential_without_pmu(|| {
        fn as_nanos(ts: libc::timespec) -> i128 {
            i128::from(ts.tv_sec) * 1_000_000_000 + i128::from(ts.tv_nsec)
        }

        let futex = 0_u32;
        let mut before = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut before) },
            0
        );
        let mut deadline = before;
        deadline.tv_nsec += 5_000_000;
        if deadline.tv_nsec >= 1_000_000_000 {
            deadline.tv_sec += 1;
            deadline.tv_nsec -= 1_000_000_000;
        }

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &futex,
                    libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG,
                    0,
                    &deadline,
                    std::ptr::null::<u32>(),
                    1_u32,
                )
            },
            -1
        );
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::ETIMEDOUT);

        let mut after = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut after) },
            0
        );
        let elapsed = as_nanos(after) - as_nanos(before);
        assert!(
            (5_000_000..1_000_000_000).contains(&elapsed),
            "absolute WAIT_BITSET deadline advanced virtual time by {elapsed}ns"
        );

        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    &futex,
                    libc::FUTEX_WAKE_BITSET | libc::FUTEX_PRIVATE_FLAG,
                    1,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    1_u32,
                )
            },
            0,
            "timed-out waiter must not remain in the futex queue"
        );
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
