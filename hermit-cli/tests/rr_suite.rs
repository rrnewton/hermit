/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Runs rr's focused syscall edge-case test programs under `hermit run`.
//!
//! This ports the fbsource `RR_TEST_TARGETS` set (see
//! `hermetic_infra/common/wrap_test_suite.bzl`), which wraps the upstream
//! [rr](https://github.com/rr-debugger/rr) `src/test/*.c` programs and runs each
//! under Hermit in strict (deterministic) mode, asserting the expected exit
//! code. The rr sources come from the pinned `third-party/rr` git submodule;
//! initialize it with:
//!
//! ```text
//! git submodule update --init third-party/rr
//! ```
//!
//! Each test compiles its rr `.c` program (rr's test harness needs a couple of
//! generated syscall-enum headers, produced here by rr's `generate_syscalls.py`)
//! and runs it as:
//!
//! ```text
//! hermit run --base-env=minimal --preemption-timeout=80000000 -- <program> [args]
//! ```
//!
//! The programs are ptrace-heavy and rely on PMU branch counters plus working
//! user/mount namespaces, so like the other Hermit integration suites these are
//! `#[ignore]`d by default and exercised explicitly (e.g. from `validate.sh`):
//!
//! ```text
//! cargo test -p hermit --test rr_suite -- --ignored
//! ```
//!
//! All 219 programs in the exported target set are tracked here. Passing tests
//! assert their native exit code; known failures run as xfails that assert a
//! specific failure shape. An xfail that unexpectedly passes fails the test so
//! it cannot remain hidden after the underlying bug is fixed. See
//! `docs/rr-test-suite.md` for issue links and triage details.

use std::fs;
use std::io::Read;
use std::io::Seek;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static GENERATED_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Per-test wall-clock cap (argument to `timeout(1)`).
const RR_TEST_TIMEOUT: &str = "120s";

/// Known hangs should fail quickly without slowing the full suite by minutes.
const RR_XFAIL_TIMEOUT: &str = "10s";

const RR_TRACKED_TESTS: usize = 219;
const RR_PASSING_TESTS: usize = 214;
const RR_XFAIL_TESTS: usize = 5;

#[derive(Clone, Copy)]
enum ExpectedFailure {
    Signal {
        signal: i32,
        output_contains: &'static str,
    },
    Timeout,
    IntermittentTimeout {
        attempts: usize,
    },
}

struct RrExecution {
    output: Output,
    rendered: String,
}

/// rr's test harness includes these generated headers (guarded by target arch).
const GENERATED_HEADERS: [&str; 3] = [
    "SyscallEnumsForTestsX64.generated",
    "SyscallEnumsForTestsX86.generated",
    "SyscallEnumsForTestsGeneric.generated",
];

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

fn rr_root() -> PathBuf {
    repository().join("third-party/rr")
}

/// Generates rr's syscall-enum headers once and returns the directory holding
/// them (added to the include path when compiling each test program).
fn generated_dir() -> &'static Path {
    GENERATED_DIR.get_or_init(|| {
        let rr = rr_root();
        assert!(
            rr.join("src/test/util.h").is_file(),
            "rr submodule not initialized at {}; run: git submodule update --init third-party/rr",
            rr.display()
        );
        let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rr-generated");
        fs::create_dir_all(&out).expect("failed to create rr generated-header directory");
        let generator = rr.join("src/generate_syscalls.py");
        for header in GENERATED_HEADERS {
            let mut command = Command::new("python3");
            command.arg(&generator).arg(out.join(header));
            command_output(command, &format!("generate rr header {header}"));
        }
        out
    })
}

/// Compiles the rr `src/test/<basename>.c` program (matching rr's own
/// `RR_TEST_FLAGS`) and returns the resulting binary path, reusing it if present.
fn compile_test_source(basename: &str, source: &str) -> PathBuf {
    let rr = rr_root();
    let generated = generated_dir();
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rr-workloads");
    fs::create_dir_all(&build_root).expect("failed to create rr workload directory");
    let binary = build_root.join(basename);
    if !binary.is_file() {
        let mut command = Command::new("cc");
        command
            .args([
                "-D_FILE_OFFSET_BITS=64",
                "-pthread",
                "-std=gnu11",
                "-g3",
                "-O0",
                "-Wno-error",
            ])
            .arg("-I")
            .arg(rr.join("src/test"))
            .arg("-I")
            .arg(rr.join("src/preload"))
            .arg("-I")
            .arg(rr.join("include"))
            .arg("-I")
            .arg(generated)
            .arg(rr.join("src/test").join(source))
            .arg("-o")
            .arg(&binary)
            .args(["-ldl", "-lrt"]);
        command_output(command, &format!("compile rr test {basename}"));
    }
    binary
}

fn fresh_scratch_dir(basename: &str) -> tempfile::TempDir {
    // Use target/ rather than /tmp because Hermit isolates the guest's /tmp.
    let scratch_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rr-scratch");
    fs::create_dir_all(&scratch_root).expect("failed to create rr scratch root");
    tempfile::Builder::new()
        .prefix(&format!("{basename}-"))
        .tempdir_in(&scratch_root)
        .expect("failed to create fresh rr scratch directory")
}

#[test]
fn rr_scratch_directories_are_fresh_and_cleaned() {
    let first = fresh_scratch_dir("scratch-regression");
    let first_path = first.path().to_owned();
    fs::write(first.path().join("dummy.txt"), "stale").expect("failed to dirty first scratch");

    let second = fresh_scratch_dir("scratch-regression");
    let second_path = second.path().to_owned();
    assert_ne!(first_path, second_path);
    assert!(!second.path().join("dummy.txt").exists());

    first.close().expect("failed to clean first rr scratch");
    second.close().expect("failed to clean second rr scratch");
    assert!(!first_path.exists());
    assert!(!second_path.exists());
}

fn execute_rr_test(basename: &str, source: &str, args: &[&str], timeout: &str) -> RrExecution {
    let binary = compile_test_source(basename, source);
    let scratch = fresh_scratch_dir(basename);
    let capture_dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let mut stdout =
        tempfile::tempfile_in(capture_dir).expect("failed to create rr stdout capture file");
    let mut stderr =
        tempfile::tempfile_in(capture_dir).expect("failed to create rr stderr capture file");

    let _guard = hermit_run_lock();
    // Keep timeout and every descendant in a dedicated process group. Capture
    // through regular files so an orphan cannot keep Command::output pipes
    // open, then kill any group members that survive timeout's own cleanup.
    let mut command = Command::new("timeout");
    command
        .current_dir(scratch.path())
        .args([
            "--foreground",
            "--kill-after=1s",
            timeout,
            env!("CARGO_BIN_EXE_hermit"),
        ])
        .args([
            "run",
            "--base-env=minimal",
            "--preemption-timeout=80000000",
            "--",
        ])
        .arg(&binary)
        .args(args)
        .process_group(0)
        .stdout(Stdio::from(
            stdout.try_clone().expect("failed to clone rr stdout file"),
        ))
        .stderr(Stdio::from(
            stderr.try_clone().expect("failed to clone rr stderr file"),
        ));
    let rendered = format!("{command:?}");
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {basename}: {rendered}: {error}"));
    let process_group = i32::try_from(child.id()).expect("rr child PID must fit in i32");
    let status = child
        .wait()
        .unwrap_or_else(|error| panic!("failed to wait for {basename}: {rendered}: {error}"));

    // SAFETY: process_group is the positive PID assigned to the child above;
    // negating it targets only that child's dedicated process group.
    let cleanup = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    if cleanup != 0 {
        let error = std::io::Error::last_os_error();
        assert_eq!(
            error.raw_os_error(),
            Some(libc::ESRCH),
            "failed to clean rr process group {process_group}: {error}"
        );
    }

    stdout.rewind().expect("failed to rewind rr stdout");
    stderr.rewind().expect("failed to rewind rr stderr");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout
        .read_to_end(&mut stdout_bytes)
        .expect("failed to read rr stdout");
    stderr
        .read_to_end(&mut stderr_bytes)
        .expect("failed to read rr stderr");

    scratch
        .close()
        .expect("failed to clean rr scratch directory");
    RrExecution {
        output: Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        },
        rendered,
    }
}

fn execute_standard_rr_test(basename: &str, args: &[&str], timeout: &str) -> RrExecution {
    execute_rr_test(basename, &format!("{basename}.c"), args, timeout)
}

fn execution_details(execution: &RrExecution) -> String {
    format!(
        "{}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        execution.rendered,
        execution.output.status,
        String::from_utf8_lossy(&execution.output.stdout),
        String::from_utf8_lossy(&execution.output.stderr),
    )
}

/// Compiles and runs one rr test program under Hermit, asserting `expected_exit`.
fn run_rr_test(basename: &str, expected_exit: i32, args: &[&str]) {
    let execution = execute_standard_rr_test(basename, args, RR_TEST_TIMEOUT);
    assert_eq!(
        execution.output.status.code(),
        Some(expected_exit),
        "rr test {basename} exited unexpectedly (124 == timeout): {}",
        execution_details(&execution),
    );
}

fn run_rr_source_test(basename: &str, source: &str, expected_exit: i32, args: &[&str]) {
    let execution = execute_rr_test(basename, source, args, RR_TEST_TIMEOUT);
    assert_eq!(
        execution.output.status.code(),
        Some(expected_exit),
        "rr test {basename} exited unexpectedly (124 == timeout): {}",
        execution_details(&execution),
    );
}

fn run_rr_xfail(
    basename: &str,
    args: &[&str],
    expected: ExpectedFailure,
    reason: &str,
    issue: &str,
) {
    let assert_timeout = |execution: &RrExecution| {
        assert_eq!(
            execution.output.status.code(),
            Some(124),
            "rr xfail {basename} changed failure shape or XPASSed; {reason}; {issue}: {}",
            execution_details(execution),
        );
    };

    match expected {
        ExpectedFailure::Signal {
            signal,
            output_contains,
        } => {
            let execution = execute_standard_rr_test(basename, args, RR_XFAIL_TIMEOUT);
            assert_eq!(
                execution.output.status.signal(),
                Some(signal),
                "rr xfail {basename} changed failure shape or XPASSed; {reason}; {issue}: {}",
                execution_details(&execution),
            );
            let stdout = String::from_utf8_lossy(&execution.output.stdout);
            let stderr = String::from_utf8_lossy(&execution.output.stderr);
            assert!(
                stdout.contains(output_contains) || stderr.contains(output_contains),
                "rr xfail {basename} did not contain expected diagnostic {output_contains:?}; {reason}; {issue}: {}",
                execution_details(&execution),
            );
        }
        ExpectedFailure::Timeout => {
            let execution = execute_standard_rr_test(basename, args, RR_XFAIL_TIMEOUT);
            assert_timeout(&execution);
        }
        ExpectedFailure::IntermittentTimeout { attempts } => {
            let mut timeout_seen = false;
            for attempt in 1..=attempts {
                let execution = execute_standard_rr_test(basename, args, RR_XFAIL_TIMEOUT);
                match execution.output.status.code() {
                    Some(0) => {}
                    Some(124) => timeout_seen = true,
                    _ => panic!(
                        "rr xfail {basename} produced an untracked failure on attempt {attempt}; {reason}; {issue}: {}",
                        execution_details(&execution),
                    ),
                }
            }
            assert!(
                timeout_seen,
                "rr xfail {basename} XPASSed in all {attempts} attempts; remove the xfail and resolve {issue}"
            );
        }
    }

    eprintln!("XFAIL {basename}: {reason}; {issue}");
}

macro_rules! rr_test {
    ($name:ident, $base:literal, $exit:literal, $args:expr) => {
        #[test]
        #[ignore = "ptrace-heavy rr program; requires PMU branch counters and working mount namespaces"]
        fn $name() {
            run_rr_test($base, $exit, $args);
        }
    };
}

macro_rules! rr_source_test {
    ($name:ident, $base:literal, $source:literal, $exit:literal, $args:expr) => {
        #[test]
        #[ignore = "ptrace-heavy rr program; requires PMU branch counters and working mount namespaces"]
        fn $name() {
            run_rr_source_test($base, $source, $exit, $args);
        }
    };
}

macro_rules! rr_xfail {
    ($name:ident, $base:literal, $args:expr, $expected:expr, $reason:literal, $issue:literal) => {
        #[test]
        #[ignore = "known Hermit bug; executes in hardware CI and must fail in the documented way"]
        fn $name() {
            run_rr_xfail($base, $args, $expected, $reason, $issue);
        }
    };
}

#[test]
fn all_exported_rr_tests_are_classified() {
    let source = include_str!("rr_suite.rs");
    let macro_count = |name: &str| source.matches(&format!("{name}!(")).count();
    let passing = macro_count("rr_test") + macro_count("rr_source_test");
    let xfails = macro_count("rr_xfail");

    assert_eq!(passing, RR_PASSING_TESTS);
    assert_eq!(xfails, RR_XFAIL_TESTS);
    assert_eq!(passing + xfails, RR_TRACKED_TESTS);
}

rr_source_test!(
    rr_arch_prctl,
    "arch_prctl_x86",
    "x86/arch_prctl_x86.c",
    0,
    &[]
);
rr_test!(
    rr_args,
    "args",
    0,
    &["-no", "--force-syscall-buffer=foo", "-c", "1000", "hello"]
);
rr_test!(rr_brk, "brk", 0, &[]);
rr_test!(rr_brk2, "brk2", 0, &[]);
rr_test!(rr_exit_group, "exit_group", 0, &[]);
rr_test!(rr_exit_race, "exit_race", 0, &[]);
rr_test!(rr_fadvise, "fadvise", 0, &[]);
rr_test!(rr_fatal_init_signal, "fatal_init_signal", 0, &[]);
rr_test!(rr_fcntl_dupfd, "fcntl_dupfd", 0, &[]);
rr_test!(rr_fcntl_misc, "fcntl_misc", 0, &[]);
rr_test!(rr_fcntl_rw_hints, "fcntl_rw_hints", 0, &[]);
rr_test!(rr_fcntl_seals, "fcntl_seals", 0, &[]);
rr_test!(rr_fcntl_sig, "fcntl_sig", 0, &[]);
rr_test!(rr_fd_cleanup, "fd_cleanup", 0, &[]);
rr_test!(rr_fd_limit, "fd_limit", 0, &[]);
rr_test!(rr_fds_clean, "fds_clean", 0, &[]);
rr_test!(rr_fork_brk, "fork_brk", 0, &[]);
rr_test!(rr_fork_child_crash, "fork_child_crash", 0, &[]);
rr_test!(rr_fork_many, "fork_many", 0, &[]);
rr_test!(rr_fork_stress, "fork_stress", 0, &[]);
rr_test!(rr_fork_syscalls, "fork_syscalls", 0, &[]);
rr_test!(rr_function_calls, "function_calls", 0, &[]);
rr_test!(rr_getcpu, "getcpu", 0, &[]);
rr_test!(rr_getgroups, "getgroups", 0, &[]);
rr_test!(rr_getpwnam, "getpwnam", 0, &[]);
rr_test!(rr_getrandom, "getrandom", 0, &[]);
rr_test!(rr_getsid, "getsid", 0, &[]);
rr_test!(rr_gettimeofday, "gettimeofday", 0, &[]);
rr_test!(rr_hello, "hello", 0, &[]);
rr_test!(rr_intr_ppoll, "intr_ppoll", 0, &[]);
rr_test!(rr_invalid_exec, "invalid_exec", 0, &[]);
rr_test!(rr_invalid_fcntl, "invalid_fcntl", 0, &[]);
rr_test!(rr_invalid_ioctl, "invalid_ioctl", 0, &[]);
rr_test!(rr_io, "io", 0, &[]);
rr_test!(rr_ioctl, "ioctl", 0, &[]);
rr_test!(rr_ioctl_blk, "ioctl_blk", 0, &[]);
rr_test!(rr_ioctl_fb, "ioctl_fb", 0, &[]);
rr_test!(rr_ioctl_fs, "ioctl_fs", 0, &[]);
rr_test!(rr_ioctl_sg, "ioctl_sg", 0, &[]);
rr_test!(rr_ioctl_tty, "ioctl_tty", 0, &[]);
rr_test!(rr_ioctl_vt, "ioctl_vt", 0, &[]);
rr_test!(rr_ioprio, "ioprio", 0, &[]);
rr_test!(rr_join_threads, "join_threads", 0, &[]);
rr_test!(rr_keyctl, "keyctl", 0, &[]);
rr_test!(rr_kill_newborn, "kill_newborn", 0, &[]);
rr_test!(rr_large_hole, "large_hole", 0, &[]);
rr_test!(rr_large_write_deadlock, "large_write_deadlock", 0, &[]);
rr_test!(rr_legacy_ugid, "legacy_ugid", 0, &[]);
rr_test!(rr_link, "link", 0, &[]);
rr_test!(rr_madvise, "madvise", 0, &[]);
rr_test!(rr_madvise_wipeonfork, "madvise_wipeonfork", 0, &[]);
rr_test!(rr_map_fixed, "map_fixed", 0, &[]);
rr_test!(rr_map_shared_syscall, "map_shared_syscall", 0, &[]);
rr_test!(rr_membarrier, "membarrier", 0, &[]);
rr_test!(rr_memfd_create, "memfd_create", 0, &[]);
rr_test!(rr_memfd_create_shared, "memfd_create_shared", 0, &[]);
rr_test!(
    rr_memfd_create_shared_huge,
    "memfd_create_shared_huge",
    0,
    &[]
);
rr_test!(rr_mincore, "mincore", 0, &[]);
rr_test!(rr_mknod, "mknod", 0, &[]);
rr_test!(rr_mlock, "mlock", 0, &[]);
rr_test!(
    rr_mmap_adjacent_to_rr_usage,
    "mmap_adjacent_to_rr_usage",
    0,
    &[]
);
rr_test!(rr_mmap_private, "mmap_private", 0, &[]);
rr_test!(
    rr_mmap_private_grow_under_map,
    "mmap_private_grow_under_map",
    0,
    &[]
);
rr_test!(rr_mmap_recycle, "mmap_recycle", 0, &[]);
rr_test!(rr_mmap_ro, "mmap_ro", 0, &[]);
rr_test!(rr_mmap_self_maps_shared, "mmap_self_maps_shared", 0, &[]);
rr_test!(rr_mmap_short_file, "mmap_short_file", 0, &[]);
rr_test!(rr_mmap_shared_dev_zero, "mmap_shared_dev_zero", 0, &[]);
rr_test!(
    rr_mmap_shared_grow_under_map,
    "mmap_shared_grow_under_map",
    0,
    &[]
);
rr_test!(rr_mmap_shared_multiple, "mmap_shared_multiple", 0, &[]);
rr_test!(rr_mmap_shared_write, "mmap_shared_write", 0, &[]);
rr_test!(rr_mmap_shared_write_fork, "mmap_shared_write_fork", 0, &[]);
rr_test!(rr_mmap_write_complex, "mmap_write_complex", 0, &[]);
rr_test!(rr_mmap_zero_size_fd, "mmap_zero_size_fd", 0, &[]);
rr_test!(rr_mprotect, "mprotect", 0, &[]);
rr_test!(rr_mprotect_heterogenous, "mprotect_heterogenous", 0, &[]);
rr_test!(rr_mprotect_none, "mprotect_none", 0, &[]);
rr_test!(rr_mprotect_stack, "mprotect_stack", 0, &[]);
rr_test!(rr_mremap, "mremap", 0, &[]);
rr_test!(rr_mremap_after_coalesce, "mremap_after_coalesce", 0, &[]);
rr_test!(rr_mremap_grow, "mremap_grow", 0, &[]);
rr_test!(rr_mremap_grow_shared, "mremap_grow_shared", 0, &[]);
rr_test!(rr_mremap_non_page_size, "mremap_non_page_size", 0, &[]);
rr_test!(rr_mremap_overwrite, "mremap_overwrite", 0, &[]);
rr_test!(
    rr_mremap_private_grow_under_map,
    "mremap_private_grow_under_map",
    0,
    &[]
);
rr_test!(rr_mremap_shrink, "mremap_shrink", 0, &[]);
rr_test!(rr_msg_trunc, "msg_trunc", 0, &[]);
rr_test!(rr_msync, "msync", 0, &[]);
rr_test!(rr_mtio, "mtio", 0, &[]);
rr_test!(
    rr_multiple_pending_signals,
    "multiple_pending_signals",
    0,
    &[]
);
rr_xfail!(
    rr_multiple_pending_signals_sequential,
    "multiple_pending_signals_sequential",
    &[],
    ExpectedFailure::IntermittentTimeout { attempts: 3 },
    "intermittent hang in sequential delivery of multiple pending signals",
    "https://github.com/rrnewton/hermit/issues/116"
);
rr_test!(rr_munmap_discontinuous, "munmap_discontinuous", 0, &[]);
rr_test!(rr_munmap_segv, "munmap_segv", 0, &[]);
rr_test!(rr_netlink_mmap_disable, "netlink_mmap_disable", 0, &[]);
rr_test!(rr_no_mask_timeslice, "no_mask_timeslice", 0, &[]);
rr_test!(rr_numa, "numa", 0, &[]);
rr_test!(rr_pause, "pause", 1, &[]);
rr_test!(rr_personality, "personality", 0, &[]);
rr_test!(rr_poll_sig_race, "poll_sig_race", 0, &[]);
rr_test!(rr_ppoll, "ppoll", 0, &[]);
rr_test!(rr_prctl_name, "prctl_name", 0, &[]);
rr_test!(rr_prctl_short_name, "prctl_short_name", 0, &[]);
rr_test!(rr_prctl_speculation_ctrl, "prctl_speculation_ctrl", 0, &[]);
rr_test!(rr_privileged_net_ioctl, "privileged_net_ioctl", 0, &[]);
rr_test!(rr_proc_fds, "proc_fds", 0, &[]);
rr_test!(rr_protect_rr_fds, "protect_rr_fds", 0, &[]);
rr_test!(rr_prw, "prw", 0, &[]);
rr_test!(
    rr_pthread_condvar_locking,
    "pthread_condvar_locking",
    0,
    &[]
);
rr_test!(
    rr_pthread_mutex_timedlock,
    "pthread_mutex_timedlock",
    0,
    &[]
);
rr_test!(rr_pthread_rwlocks, "pthread_rwlocks", 0, &[]);
rr_test!(rr_quotactl, "quotactl", 0, &[]);
rr_test!(rr_read_large, "read_large", 0, &[]);
rr_test!(rr_read_nothing, "read_nothing", 0, &[]);
rr_test!(rr_read_oversize, "read_oversize", 0, &[]);
rr_test!(rr_readdir, "readdir", 0, &[]);
rr_test!(rr_readlink, "readlink", 0, &[]);
rr_test!(rr_readlinkat, "readlinkat", 0, &[]);
rr_test!(rr_readv, "readv", 0, &[]);
rr_test!(rr_recvfrom, "recvfrom", 0, &[]);
rr_test!(rr_rename, "rename", 0, &[]);
rr_test!(rr_rlimit, "rlimit", 0, &[]);
rr_xfail!(
    rr_rusage,
    "rusage",
    &[],
    ExpectedFailure::Signal {
        signal: libc::SIGABRT,
        output_contains: "rusage.c:10: !(r->ru_maxrss > 0)",
    },
    "getrusage returns ru_maxrss == 0",
    "https://github.com/rrnewton/hermit/issues/114"
);
rr_test!(rr_sched_attr, "sched_attr", 0, &[]);
rr_test!(rr_sched_setaffinity, "sched_setaffinity", 0, &[]);
rr_test!(rr_sched_setparam, "sched_setparam", 0, &[]);
rr_test!(
    rr_sched_yield_to_lower_priority,
    "sched_yield_to_lower_priority",
    0,
    &[]
);
rr_test!(rr_scratch_read, "scratch_read", 0, &[]);
rr_test!(rr_seccomp_clone_fail, "seccomp_clone_fail", 0, &[]);
rr_test!(rr_seccomp_cloning, "seccomp_cloning", 0, &[]);
rr_test!(rr_seccomp_desched, "seccomp_desched", 0, &[]);
rr_test!(rr_seccomp_kill_exit, "seccomp_kill_exit", 0, &[]);
rr_test!(rr_seccomp_sigsys_args, "seccomp_sigsys_args", 0, &[]);
rr_test!(rr_seccomp_sigsys_sigtrap, "seccomp_sigsys_sigtrap", 0, &[]);
rr_test!(
    rr_seccomp_sigsys_syscallbuf,
    "seccomp_sigsys_syscallbuf",
    0,
    &[]
);
rr_test!(rr_seccomp_tsync, "seccomp_tsync", 0, &[]);
rr_test!(rr_seccomp_veto_exec, "seccomp_veto_exec", 0, &[]);
rr_test!(rr_self_shebang, "self_shebang", 0, &[]);
rr_test!(rr_sendfile, "sendfile", 0, &[]);
rr_test!(rr_setgid, "setgid", 0, &[]);
rr_test!(rr_setgroups, "setgroups", 0, &[]);
rr_test!(rr_setitimer, "setitimer", 0, &[]);
rr_test!(rr_setsid, "setsid", 0, &[]);
rr_test!(rr_setuid, "setuid", 0, &[]);
rr_test!(rr_shared_exec, "shared_exec", 0, &[]);
rr_test!(rr_shared_monitor, "shared_monitor", 0, &[]);
rr_test!(rr_shared_offset, "shared_offset", 0, &[]);
rr_test!(rr_shared_write, "shared_write", 0, &[]);
rr_test!(rr_shm, "shm", 0, &[]);
rr_test!(rr_shm_unmap, "shm_unmap", 0, &[]);
rr_test!(rr_sigaction_old, "sigaction_old", 0, &[]);
rr_test!(rr_sigaltstack, "sigaltstack", 0, &[]);
rr_xfail!(
    rr_sigchld_interrupt_signal,
    "sigchld_interrupt_signal",
    &[],
    ExpectedFailure::Timeout,
    "hang in SIGCHLD interrupt/restart handling",
    "https://github.com/rrnewton/hermit/issues/115"
);
rr_test!(rr_sigcont, "sigcont", 0, &[]);
rr_test!(
    rr_sighandler_bad_rsp_sigsegv,
    "sighandler_bad_rsp_sigsegv",
    0,
    &[]
);
rr_test!(rr_sighandler_fork, "sighandler_fork", 0, &[]);
rr_test!(rr_sighandler_mask, "sighandler_mask", 0, &[]);
rr_test!(rr_sigill, "sigill", 0, &[]);
rr_test!(rr_signal_deferred, "signal_deferred", 0, &[]);
rr_test!(
    rr_signal_during_preload_init,
    "signal_during_preload_init",
    0,
    &[]
);
rr_test!(rr_signal_frame, "signal_frame", 0, &[]);
rr_test!(rr_signal_unstoppable, "signal_unstoppable", 0, &[]);
rr_test!(rr_signalfd, "signalfd", 0, &[]);
rr_test!(rr_sigprocmask, "sigprocmask", 0, &[]);
rr_test!(rr_sigprocmask_evil, "sigprocmask_evil", 0, &[]);
rr_xfail!(
    rr_sigprocmask_in_syscallbuf_sighandler,
    "sigprocmask_in_syscallbuf_sighandler",
    &[],
    ExpectedFailure::Timeout,
    "hang changing the signal mask from a syscall-buffer signal handler",
    "https://github.com/rrnewton/hermit/issues/112"
);
rr_test!(rr_sigprocmask_syscallbuf, "sigprocmask_syscallbuf", 0, &[]);
rr_test!(rr_sigpwr, "sigpwr", 0, &[]);
rr_test!(rr_sigqueueinfo, "sigqueueinfo", 0, &[]);
rr_test!(rr_sigreturn_reg, "sigreturn_reg", 0, &[]);
rr_test!(rr_sigtrap, "sigtrap", 0, &[]);
rr_test!(rr_simple_threads_stress, "simple_threads_stress", 0, &[]);
rr_test!(rr_small_holes, "small_holes", 0, &[]);
rr_xfail!(
    rr_spinlock_priorities,
    "spinlock_priorities",
    &[],
    ExpectedFailure::Timeout,
    "priority-sensitive scheduler progress hangs around a userspace spinlock",
    "https://github.com/rrnewton/hermit/issues/113"
);
rr_test!(rr_splice, "splice", 0, &[]);
rr_test!(
    rr_stack_growth_after_syscallbuf,
    "stack_growth_after_syscallbuf",
    0,
    &[]
);
rr_test!(
    rr_stack_growth_syscallbuf,
    "stack_growth_syscallbuf",
    0,
    &[]
);
rr_test!(rr_stack_invalid, "stack_invalid", 0, &[]);
rr_test!(rr_stack_overflow, "stack_overflow", 0, &[]);
rr_test!(
    rr_stack_overflow_altstack,
    "stack_overflow_altstack",
    0,
    &[]
);
rr_test!(
    rr_stack_overflow_with_guard,
    "stack_overflow_with_guard",
    0,
    &[]
);
rr_test!(rr_statx, "statx", 0, &[]);
rr_test!(rr_stdout_child, "stdout_child", 0, &[]);
rr_test!(rr_stdout_cloexec, "stdout_cloexec", 0, &[]);
rr_test!(rr_stdout_dup, "stdout_dup", 0, &[]);
rr_test!(rr_stdout_redirect, "stdout_redirect", 0, &[]);
rr_test!(rr_symlink, "symlink", 0, &[]);
rr_test!(rr_sync, "sync", 0, &[]);
rr_test!(rr_sync_file_range, "sync_file_range", 0, &[]);
rr_test!(rr_syscall_bp, "syscall_bp", 0, &[]);
rr_test!(
    rr_syscall_in_writable_mem,
    "syscall_in_writable_mem",
    0,
    &[]
);
rr_test!(
    rr_syscallbuf_signal_reset,
    "syscallbuf_signal_reset",
    0,
    &[]
);
rr_test!(rr_syscallbuf_sigstop, "syscallbuf_sigstop", 0, &[]);
rr_test!(rr_sysconf_conf, "sysconf_conf", 0, &[]);
rr_test!(rr_sysctl, "sysctl", 0, &[]);
rr_test!(rr_sysemu_singlestep, "sysemu_singlestep", 0, &[]);
rr_test!(rr_sysinfo, "sysinfo", 0, &[]);
rr_test!(rr_tgkill, "tgkill", 0, &[]);
rr_test!(rr_thread_stress, "thread_stress", 0, &[]);
rr_test!(rr_threads, "threads", 0, &[]);
rr_test!(rr_truncate_temp, "truncate_temp", 0, &[]);
rr_test!(rr_tun, "tun", 0, &[]);
rr_test!(rr_ulimit_low, "ulimit_low", 0, &[]);
rr_test!(rr_uname, "uname", 0, &[]);
rr_test!(rr_unexpected_exit_pid_ns, "unexpected_exit_pid_ns", 0, &[]);
rr_test!(rr_unjoined_thread, "unjoined_thread", 0, &[]);
rr_test!(rr_unshare, "unshare", 0, &[]);
rr_test!(
    rr_vdso_clock_gettime_stack,
    "vdso_clock_gettime_stack",
    0,
    &[]
);
rr_test!(
    rr_vdso_gettimeofday_stack,
    "vdso_gettimeofday_stack",
    0,
    &[]
);
rr_test!(rr_vdso_parts, "vdso_parts", 0, &[]);
rr_test!(rr_vdso_time_stack, "vdso_time_stack", 0, &[]);
rr_test!(rr_vfork_flush, "vfork_flush", 0, &[]);
rr_test!(
    rr_vfork_read_clone_stress,
    "vfork_read_clone_stress",
    0,
    &[]
);
rr_test!(rr_video_capture, "video_capture", 0, &[]);
rr_test!(rr_vm_readv_writev, "vm_readv_writev", 0, &[]);
rr_test!(rr_wait_for_all, "wait_for_all", 0, &[]);
rr_test!(rr_write_race, "write_race", 0, &[]);
rr_test!(rr_writev, "writev", 0, &[]);
rr_test!(rr_xattr, "xattr", 0, &[]);
rr_test!(rr_zero_length_read, "zero_length_read", 0, &[]);
