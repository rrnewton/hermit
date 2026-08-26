/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

// The one definition of hermit's own failure status, imported rather than
// written out. Copying the number here is what let eight tests keep asserting
// `1` for months after the product moved to `125`.
use hermit::GUEST_PROGRAM_NOT_EXECUTABLE_EXIT;
use hermit::GUEST_PROGRAM_NOT_FOUND_EXIT;
use hermit::HERMIT_INTERNAL_FAILURE_EXIT;

static DBT_MMAP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_EXEC_FAILURE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_EXECVEAT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_PID_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_PRLIMIT_SELF_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_WAIT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static KVM_EXACT_CHILD_WAITS_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_UNSUPPORTED_SYSCALL_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_SELF_SIGQUEUE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_STDERR_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_LOG_ENV_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_INERT_RUNTIME: OnceLock<PathBuf> = OnceLock::new();
static EXEC_CLOCK_CONTINUITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static STDIO_LSEEK_IDENTITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FORK_CHILD_GETRANDOM_GUEST: OnceLock<PathBuf> = OnceLock::new();
static STDIO_STATUS_FLAG_CONTAINMENT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

// This lock only serializes independent child processes; a failed assertion carries no
// protected state invariant and must not poison unrelated tests.
fn hermit_run_guard() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn hermit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
}

fn hermit_with_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));
    child
        .stdin
        .take()
        .expect("hermit stdin should be piped")
        .write_all(input)
        .expect("failed to write hermit stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to wait for hermit with {args:?}: {error}"))
}

fn dbt_stderr_guest() -> &'static Path {
    DBT_STDERR_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-stderr-nostdlib");
        fs::create_dir_all(&build_root).expect("failed to create DBT stderr guest directory");
        let guest = build_root.join("stderr_nostdlib");
        let output = Command::new("cc")
            .args([
                "-nostdlib",
                "-static",
                "-fno-pie",
                "-no-pie",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg(repository.join("hermit-cli/tests/fixtures/dbt/stderr_nostdlib.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT stderr guest");
        assert!(
            output.status.success(),
            "DBT stderr guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_log_env_guest() -> &'static Path {
    DBT_LOG_ENV_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-hermit-log-env");
        fs::create_dir_all(&build_root).expect("failed to create DBT log-env guest directory");
        let guest = build_root.join("hermit_log_env");
        let output = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("hermit-cli/tests/fixtures/dbt/hermit_log_env.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT log-env guest");
        assert!(
            output.status.success(),
            "DBT log-env guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn read_terminal_dbt_verdict(path: &Path) -> serde_json::Value {
    let verdict: serde_json::Value =
        serde_json::from_slice(&fs::read(path).expect("failed to read DBT verification verdict"))
            .expect("DBT verification verdict should be JSON");
    let matched = verdict["verdict"] == "matched";
    assert!(
        matched || verdict["verdict"] == "diverged",
        "verification did not reach a terminal comparison: {verdict}"
    );
    assert_eq!(
        verdict["verified"], matched,
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["bitwise_parity"], matched,
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["comparison"]["strictness"], "canonical",
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["comparison"]["log_scope"], "info",
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["comparison"]["record_envelope"], "all_records_v1",
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["guest_exit_code"], 0,
        "guest rejected its environment: {verdict}"
    );
    for side in ["left", "right"] {
        assert!(
            verdict["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "empty {side} INFO population: {verdict}"
        );
    }
    verdict
}

fn liteinst_inert_runtime() -> &'static Path {
    LITEINST_INERT_RUNTIME.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-inert-runtime");
        fs::create_dir_all(&build_root).expect("failed to create inert runtime directory");
        let runtime = build_root.join("libreverie_liteinst_inert.so");
        let output = Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/liteinst_inert_runtime.c"))
            .arg("-o")
            .arg(&runtime)
            .output()
            .expect("failed to compile inert LiteInst runtime fixture");
        assert!(
            output.status.success(),
            "inert LiteInst fixture compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        runtime
    })
}

fn exec_clock_continuity_guest() -> &'static Path {
    EXEC_CLOCK_CONTINUITY_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("exec-clock-continuity");
        fs::create_dir_all(&build_root)
            .expect("failed to create exec-clock-continuity guest directory");
        let guest = build_root.join("exec_clock_continuity");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/exec_clock_continuity.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile exec-clock-continuity guest");
        assert!(
            output.status.success(),
            "exec-clock-continuity guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn stdio_lseek_identity_guest() -> &'static Path {
    STDIO_LSEEK_IDENTITY_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stdio-lseek-identity");
        fs::create_dir_all(&build_root).expect("failed to create stdio-lseek build directory");
        let guest = build_root.join("stdio_lseek_identity");
        let output = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/stdio_lseek_identity.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile stdio-lseek fixture");
        assert!(
            output.status.success(),
            "stdio-lseek fixture compilation failed:
stdout:
{}
stderr:
{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// TODO-HUMAN-REVIEW(PR-1052): Review no-namespace fork-child RNG coverage.
fn fork_child_getrandom_guest() -> &'static Path {
    FORK_CHILD_GETRANDOM_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fork-child-getrandom");
        fs::create_dir_all(&build_root)
            .expect("failed to create fork-child-getrandom guest directory");
        let guest = build_root.join("fork_child_getrandom");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/fork_child_getrandom.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile fork-child-getrandom guest");
        assert!(
            output.status.success(),
            "fork-child-getrandom guest compilation failed:
stdout:
{}
stderr:
{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_mmap_guest() -> &'static Path {
    DBT_MMAP_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-mmap");
        fs::create_dir_all(&build_root).expect("failed to create DBT mmap guest directory");
        let guest = build_root.join("dbt_mmap_exec");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_mmap_exec.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT mmap guest");
        assert!(
            output.status.success(),
            "DBT mmap guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_exec_failure_guest() -> &'static Path {
    DBT_EXEC_FAILURE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-exec-failure");
        fs::create_dir_all(&build_root).expect("failed to create DBT exec-failure guest directory");
        let guest = build_root.join("dbt_exec_failure");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_exec_failure.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT exec-failure guest");
        assert!(
            output.status.success(),
            "DBT exec-failure guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_execveat_guest() -> &'static Path {
    DBT_EXECVEAT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-execveat");
        fs::create_dir_all(&build_root).expect("failed to create DBT execveat guest directory");
        let guest = build_root.join("dbt_execveat_unsupported");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_execveat_unsupported.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT execveat guest");
        assert!(
            output.status.success(),
            "DBT execveat guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_wait_guest() -> &'static Path {
    DBT_WAIT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-wait");
        fs::create_dir_all(&build_root).expect("failed to create DBT wait guest directory");
        let guest = build_root.join("dbt_wait_lifecycle");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_wait_lifecycle.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT wait guest");
        assert!(
            output.status.success(),
            "DBT wait guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn kvm_exact_child_waits_guest() -> &'static Path {
    KVM_EXACT_CHILD_WAITS_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-exact-child-waits");
        fs::create_dir_all(&build_root)
            .expect("failed to create KVM exact-child wait guest directory");
        let guest = build_root.join("kvm_exact_child_waits");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/kvm_exact_child_waits.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile KVM exact-child wait guest");
        assert!(
            output.status.success(),
            "KVM exact-child wait guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn stdio_status_flag_containment_guest() -> &'static Path {
    STDIO_STATUS_FLAG_CONTAINMENT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("stdio-status-flag-containment");
        fs::create_dir_all(&build_root)
            .expect("failed to create stdio status-flag containment guest directory");
        let guest = build_root.join("stdio_status_flag_containment");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/stdio_status_flag_containment.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the stdio status-flag containment guest");
        assert!(
            output.status.success(),
            "stdio status-flag containment guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// A guest must be able to change what IT sees about its own stderr without
/// changing what the supervising hermit process -- and therefore hermit's own
/// caller -- sees.
///
/// `fcntl(F_SETFL)` mutates the open file DESCRIPTION, not the descriptor. When
/// hermit passes its own stdio through to the guest, that description is the
/// one hermit inherited from the process that invoked it, so a forwarded
/// request escapes the container: it outlives the guest, outlives hermit, and
/// is visible to the caller afterwards.
///
/// Measured on hermit d7413071581f before the fix, on BOTH backends:
/// `hermit run -- /usr/bin/awk 'BEGIN { print 42 }'` left the caller's stderr
/// at 0x8401 where it had been 0x8001. Under `hermit run --strict --verify`
/// that also made the KVM backend report itself nondeterministic, because both
/// runs share one hermit process and the first run's mutation was still there
/// when the second started (0 of 20 verified before; 20 of 20 after).
///
/// BOTH halves are asserted on purpose. Dropping the guest's request entirely
/// would satisfy the containment half while silently breaking the guest, so the
/// guest's own before/after reading is checked first.
fn assert_guest_cannot_mutate_hermits_stderr_flags(backend: &str) {
    let _guard = hermit_run_guard();
    let guest = stdio_status_flag_containment_guest();

    let directory = tempfile::tempdir().expect("failed to create a temporary directory");
    let hermit_stderr_path = directory.path().join("hermit.err");
    // Opened WITHOUT O_APPEND, so "the bit turned on" is unambiguous.
    let hermit_stderr = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&hermit_stderr_path)
        .expect("failed to open the file standing in for hermit's stderr");

    // SAFETY: the descriptor is live and F_GETFL takes no third argument.
    let before = unsafe { libc::fcntl(hermit_stderr.as_raw_fd(), libc::F_GETFL) };
    assert!(before >= 0, "F_GETFL on the supervisor's descriptor failed");
    assert_eq!(
        before & libc::O_APPEND,
        0,
        "the supervisor's descriptor must start without O_APPEND for this test \
         to mean anything",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--backend", backend, "--"])
        .arg(guest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // `Stdio::from` dups this onto the child's fd 2, so hermit's stderr and
        // `hermit_stderr` are the SAME open file description -- exactly the
        // sharing a caller's shell redirect creates.
        .stderr(Stdio::from(
            hermit_stderr
                .try_clone()
                .expect("failed to duplicate the stderr stand-in"),
        ))
        .output()
        .unwrap_or_else(|error| panic!("failed to run the {backend} containment guest: {error}"));

    let diagnostics = fs::read_to_string(&hermit_stderr_path).unwrap_or_default();
    assert!(
        output.status.success(),
        "{backend} containment guest failed: {:?}\nhermit stderr:\n{diagnostics}",
        output.status,
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "append_before=0 append_after=1\n",
        "the guest must observe the status flag it set on its own stderr \
         ({backend} backend)\nhermit stderr:\n{diagnostics}",
    );

    // SAFETY: the descriptor is still live and F_GETFL takes no third argument.
    let after = unsafe { libc::fcntl(hermit_stderr.as_raw_fd(), libc::F_GETFL) };
    assert!(after >= 0, "F_GETFL on the supervisor's descriptor failed");
    assert_eq!(
        after, before,
        "a guest fcntl(F_SETFL) on stderr escaped the container and changed the \
         SUPERVISOR's file description ({backend} backend): 0x{before:x} -> \
         0x{after:x}\nhermit stderr:\n{diagnostics}",
    );
}

#[test]
fn run_ptrace_guest_cannot_mutate_hermits_stderr_flags() {
    assert_guest_cannot_mutate_hermits_stderr_flags("ptrace");
}

#[test]
fn run_kvm_guest_cannot_mutate_hermits_stderr_flags() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    assert_guest_cannot_mutate_hermits_stderr_flags("kvm");
}

// TODO-HUMAN-REVIEW(PR-723): Review the DBT PID fixture build.
fn dbt_pid_guest() -> &'static Path {
    DBT_PID_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-pid");
        fs::create_dir_all(&build_root).expect("failed to create DBT PID guest directory");
        let guest = build_root.join("dbt_pid_virtualization");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_pid_virtualization.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT PID guest");
        assert!(
            output.status.success(),
            "DBT PID guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1065): Review DBT self-prlimit fixture coverage.
fn dbt_prlimit_self_guest() -> &'static Path {
    DBT_PRLIMIT_SELF_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-prlimit-self");
        fs::create_dir_all(&build_root).expect("failed to create DBT self-prlimit guest directory");
        let guest = build_root.join("dbt_prlimit_self");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_prlimit_self.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT self-prlimit guest");
        assert!(
            output.status.success(),
            "DBT self-prlimit guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the DBT unsupported-syscall fixture build.
fn dbt_unsupported_syscall_guest() -> &'static Path {
    DBT_UNSUPPORTED_SYSCALL_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-unsupported-syscall");
        fs::create_dir_all(&build_root)
            .expect("failed to create DBT unsupported-syscall guest directory");
        let guest = build_root.join("dbt_unsupported_syscall");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_unsupported_syscall.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT unsupported-syscall guest");
        assert!(
            output.status.success(),
            "DBT unsupported-syscall guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// TODO-HUMAN-REVIEW(PR-1038): Review the DBT self-signal fixture build.
fn dbt_self_sigqueue_guest() -> &'static Path {
    DBT_SELF_SIGQUEUE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-self-sigqueue");
        fs::create_dir_all(&build_root)
            .expect("failed to create DBT self-sigqueue guest directory");
        let guest = build_root.join("dbt_self_sigqueue");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_self_sigqueue.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT self-sigqueue guest");
        assert!(
            output.status.success(),
            "DBT self-sigqueue guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hermit_with_closed_stdin(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: pre_exec closes only the child descriptor immediately before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::close(libc::STDIN_FILENO) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
}

fn assert_success(output: &Output, args: &[&str]) {
    assert!(
        output.status.success(),
        "hermit {args:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("hermit stdout should be UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("hermit stderr should be UTF-8")
}

/// WHICH failure a call site means to assert. Named at the call site, so the
/// assertion states an intention instead of inheriting one.
///
/// ⚠️ ONE HELPER CANNOT PIN ONE CODE HERE, AND ASSUMING IT COULD IS THE ORIGINAL
/// DEFECT. `assert_failure_contains` hardcoded `Some(1)` for every caller. When
/// hermit#2558 introduced 125 for hermit's own failures, and the guest-program
/// faults later took 127/126, the sixteen call sites stopped meaning one thing —
/// but they still shared one assertion, so a fix that simply swapped in the
/// newest observed number would have been wrong for five of them and would have
/// gone green anyway.
#[derive(Clone, Copy)]
enum Refusal {
    /// Hermit itself refused and NO GUEST WAS LAUNCHED: a contradictory flag
    /// pair, an unwritable log path, a denied capability, a backend used outside
    /// its supported command. The caller's tooling or invocation is at fault.
    Hermit,
    /// The named guest program does not exist. The caller's COMMAND LINE is at
    /// fault, not hermit — a distinction `Hermit` cannot express.
    GuestNotFound,
    /// The guest program exists but cannot be executed as given: a directory, a
    /// non-executable mode, a missing shebang interpreter target.
    GuestNotExecutable,
}

impl Refusal {
    fn code(self) -> i32 {
        match self {
            Refusal::Hermit => HERMIT_INTERNAL_FAILURE_EXIT,
            Refusal::GuestNotFound => GUEST_PROGRAM_NOT_FOUND_EXIT,
            Refusal::GuestNotExecutable => GUEST_PROGRAM_NOT_EXECUTABLE_EXIT,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Refusal::Hermit => "a hermit-internal refusal (no guest launched)",
            Refusal::GuestNotFound => "a guest-program-not-found refusal",
            Refusal::GuestNotExecutable => "a guest-program-not-executable refusal",
        }
    }
}

/// Assert that hermit refused for the stated REASON, said why, and did not panic.
///
/// ⚠️ THE EXIT CODE HERE IS A CLAIM, NOT A FORMALITY, WHICH IS WHY THE CALLER
/// NAMES IT. The three codes answer different questions — is my tooling broken
/// (125), is my program path wrong (127), is it unrunnable (126) — and they are
/// one scheme, not three unrelated numbers: GNU `env`/`chroot`/`timeout` reserve
/// exactly this split, which is where hermit's 125 came from.
///
/// ⚠️ DO NOT REPLACE A `Refusal` WITH WHATEVER CODE THE TEST HAPPENS TO EMIT.
/// This assertion was `Some(1)` until hermit#2558 and was passing for the wrong
/// reason: `1` is also the commonest guest exit, so it accepted "hermit refused"
/// and "the guest ran and failed" alike. Substituting today's observed value
/// without deciding what the test MEANS reintroduces that, one number later.
fn assert_hermit_refusal_contains(output: &Output, refusal: Refusal, expected: &[&str]) {
    // ⚠️ FAILURE FIRST, AND SEPARATELY FROM WHICH FAILURE. The equality below is
    // only as good as the constant it reads: if a `Refusal` code ever became 0,
    // `assert_eq!(code, Some(0))` would stop demanding a failure and start
    // demanding a SUCCESS, and all sixteen call sites would invert and still
    // pass. This line cannot be satisfied by any success, whatever the constants
    // say, so the two assertions fail independently rather than together.
    assert!(
        !output.status.success(),
        "expected {} but the command SUCCEEDED: {output:?}",
        refusal.describe()
    );
    assert_eq!(
        output.status.code(),
        Some(refusal.code()),
        "expected {}, got: {output:?}",
        refusal.describe()
    );
    let stderr = stderr(output);
    for message in expected {
        assert!(
            stderr.contains(message),
            "missing {message:?} in:\n{stderr}"
        );
    }
    assert!(!stderr.contains("panicked"), "unexpected panic:\n{stderr}");
}

fn deny_syscall(command: &mut Command, syscall: libc::c_long) {
    // SAFETY: The callback makes only async-signal-safe syscalls before exec. The filter is an
    // allow-all policy except for the single syscall used by each capability-probe test.
    unsafe {
        command.pre_exec(move || {
            let mut filter = [
                libc::sock_filter {
                    code: 0x20, // BPF_LD | BPF_W | BPF_ABS
                    jt: 0,
                    jf: 0,
                    k: 0, // offsetof(seccomp_data, nr)
                },
                libc::sock_filter {
                    code: 0x15, // BPF_JMP | BPF_JEQ | BPF_K
                    jt: 0,
                    jf: 1,
                    k: syscall as u32,
                },
                libc::sock_filter {
                    code: 0x06, // BPF_RET | BPF_K
                    jt: 0,
                    jf: 0,
                    k: 0x0005_0000 | libc::EPERM as u32, // SECCOMP_RET_ERRNO
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: 0x7fff_0000, // SECCOMP_RET_ALLOW
                },
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[test]
fn run_strict_flag_is_accepted_and_runs() {
    // Regression test for GH #12: `docs/Users.md` documents
    // `hermit run --strict ...`, and the CLI must accept that spelling and run
    // the guest to completion. Strict determinism is the default, so `--strict`
    // is a compatibility no-op over the defaults. `--max-timeslice=disabled`
    // and `--no-virtualize-cpuid` keep this runnable on hosts without accessible
    // PMU counters or CPUID faulting; neither weakens what `--strict` controls.
    let args = [
        "run",
        "--strict",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
}

#[test]
fn verify_verbose_requires_verify() {
    let args = ["run", "--verify-verbose", "--", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("--verify-verbose"),
        "unexpected error:\n{stderr}"
    );
    assert!(stderr.contains("--verify"), "unexpected error:\n{stderr}");
    assert!(stderr.contains("required"), "unexpected error:\n{stderr}");
}

#[test]
fn run_rejects_unknown_backends_during_argument_parsing() {
    let args = ["run", "--backend", "unknown", "--", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("invalid value 'unknown'"),
        "unexpected error:\n{stderr}"
    );
    for backend in ["ptrace", "dbt", "kvm"] {
        assert!(
            stderr.contains(backend),
            "missing {backend:?} in:\n{stderr}"
        );
    }
}

/// True when this binary cannot exercise the DBT backend, having said so.
///
/// ⚠️ KEYED ON THE COMPILE-TIME FEATURE, NEVER ON THE RUN'S OUTCOME. Skipping
/// because a `--backend dbt` invocation failed would be the opposite defect and
/// strictly worse than the reds it removes: it would convert every genuine DBT
/// regression into silence. `cfg!(feature = "dbt")` is a fact about how this
/// test binary was compiled, decided before any guest runs, so a broken but
/// PRESENT backend still fails exactly as it did before.
///
/// `default = []` in hermit-cli/Cargo.toml, so a plain `cargo test` excludes
/// DBT and these 18 tests failed on EVERY default build -- not merely on hosts
/// lacking DynamoRIO. Validate is unaffected: it builds
/// `--features third-party-backends`, which includes `dbt`.
///
/// Setting `HERMIT_REQUIRE_DBT` turns the skip back into a failure, so a CI job
/// that intends to cover DBT cannot silently stop covering it. That mirrors
/// `sabre_examples.rs`, which panics when an explicitly configured artifact is
/// missing and only skips when the default path is absent.
fn dbt_unavailable(test: &str) -> bool {
    if cfg!(feature = "dbt") {
        return false;
    }
    assert!(
        std::env::var_os("HERMIT_REQUIRE_DBT").is_none(),
        "HERMIT_REQUIRE_DBT is set, but this test binary was built WITHOUT the \
         `dbt` feature, so {test} cannot exercise the backend it claims to cover. \
         Rebuild with --features dbt (or third-party-backends), or unset \
         HERMIT_REQUIRE_DBT to allow skipping."
    );
    eprintln!(
        "skipping {test}: built without the `dbt` feature (hermit-cli default = []); \
         an absent backend is not a product failure. Build with --features dbt or \
         --features third-party-backends to exercise it."
    );
    true
}

#[test]
fn run_dbt_executes_integrated_backend() {
    if dbt_unavailable("run_dbt_executes_integrated_backend") {
        return;
    }
    let args = ["run", "--backend", "dbt", "--", "/bin/true"];
    let output = hermit(&args);
    assert_success(&output, &args);
}

#[test]
fn run_dbt_uses_the_requested_guest_environment() {
    if dbt_unavailable("run_dbt_uses_the_requested_guest_environment") {
        return;
    }
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--base-env=empty",
        "--env=DBT_GUEST_ONLY=present",
        "--",
        "/usr/bin/env",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("DBT_HOST_ONLY", "must-not-leak")
        .args(args)
        .output()
        .expect("failed to run DBT environment regression");

    assert_success(&output, &args);
    let stdout = stdout(&output);
    assert!(
        stdout.lines().any(|line| line == "DBT_GUEST_ONLY=present"),
        "DBT guest environment omitted the requested value:\n{stdout}",
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.starts_with("DBT_HOST_ONLY=")),
        "DBT guest inherited a host-only value:\n{stdout}",
    );
}

#[test]
fn run_dbt_verifies_simple_env_shebang() {
    if dbt_unavailable("run_dbt_verifies_simple_env_shebang") {
        return;
    }
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create DBT env-shebang test directory");
    let script = directory.path().join("env-echo");
    fs::write(&script, b"#!/usr/bin/env echo\n")
        .expect("failed to write DBT env-shebang test script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .expect("failed to mark DBT env-shebang test script executable");
    let program = script
        .to_str()
        .expect("DBT env-shebang test path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];

    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), format!("{}\n", script.display()));
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review ptrace verification warning delivery.
// After pidfd_send_signal/pidfd_getfd were determinized, restart_syscall is the
// lone remaining Unsupported syscall. The ptrace seccomp filter must honor the
// Detcore subscription so ordinary execution fails closed before the guest can
// publish its success marker. The explicit compatibility opt-out remains noisy
// and preserves the native -EINTR result.
#[test]
fn run_ptrace_fails_closed_by_default_on_unsupported_syscall() {
    let program = dbt_unsupported_syscall_guest()
        .to_str()
        .expect("unsupported-syscall guest path should be UTF-8");

    let supported_args = ["run", "--", "/bin/echo", "ptrace-supported-ok"];
    let supported = hermit(&supported_args);
    assert_success(&supported, &supported_args);
    assert_eq!(stdout(&supported), "ptrace-supported-ok\n");

    let default_args = ["run", "--", program];
    let default = hermit(&default_args);
    assert!(
        !default.status.success(),
        "default ptrace unexpectedly allowed restart_syscall:\n{}",
        stderr(&default)
    );
    assert!(
        stderr(&default).contains("unsupported syscall: restart_syscall"),
        "default ptrace failure omitted restart_syscall:\n{}",
        stderr(&default)
    );
    assert_eq!(
        stdout(&default),
        "",
        "unsupported guest published its success marker"
    );

    let compatibility_args = [
        "run",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        program,
    ];
    let compatibility = hermit(&compatibility_args);
    assert_success(&compatibility, &compatibility_args);
    assert_eq!(stdout(&compatibility), "dbt-unsupported-ok\n");
    let compatibility_stderr = stderr(&compatibility);
    let warning = "used but not yet supported";
    assert_eq!(
        compatibility_stderr.matches(warning).count(),
        1,
        "ptrace compatibility run omitted or duplicated the unsupported warning:\n\
         {compatibility_stderr}"
    );
    assert_eq!(
        compatibility_stderr
            .matches("a successful exit does not establish complete deterministic execution")
            .count(),
        1,
        "ptrace compatibility run omitted or duplicated its determinism warning:\n\
         {compatibility_stderr}"
    );
}
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review DBT normal aggregation and strict failure coverage.
#[test]
fn run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls() {
    if dbt_unavailable(
        "run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls",
    ) {
        return;
    }
    let program = dbt_unsupported_syscall_guest()
        .to_str()
        .expect("DBT unsupported-syscall guest path should be UTF-8");

    // Positive bracket: fail-closed changes only unsupported behavior. A supported
    // guest still succeeds through the same default DBT front door.
    let supported_args = [
        "run",
        "--backend",
        "dbt",
        "--",
        "/bin/echo",
        "dbt-supported-ok",
    ];
    let supported = hermit(&supported_args);
    assert_success(&supported, &supported_args);
    assert_eq!(stdout(&supported), "dbt-supported-ok\n");

    // Negative bracket: the real unsupported restart_syscall must fail and name
    // itself before the guest can publish its former success marker.
    let default_args = ["run", "--backend", "dbt", "--", program];
    let default = hermit(&default_args);
    assert!(
        !default.status.success(),
        "default DBT unexpectedly allowed an unsupported syscall:\n{}",
        stderr(&default)
    );
    assert!(
        stderr(&default).contains("unsupported syscall: restart_syscall"),
        "default DBT failure omitted unsupported syscall:\n{}",
        stderr(&default)
    );
    assert_eq!(
        stdout(&default),
        "",
        "unsupported guest published its success marker"
    );

    let normal_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        program,
    ];
    let normal = hermit(&normal_args);
    assert_success(&normal, &normal_args);
    assert_eq!(stdout(&normal), "dbt-unsupported-ok\n");
    let normal_stderr = stderr(&normal);
    let opt_out_warning = "a successful exit does not establish complete deterministic execution";
    assert_eq!(
        normal_stderr.matches(opt_out_warning).count(),
        1,
        "compatibility opt-out warning missing or duplicated:\n{normal_stderr}"
    );
    let warning = "syscalls restart_syscall used but not yet supported";
    assert_eq!(
        normal_stderr.matches(warning).count(),
        1,
        "expected one aggregate warning:\n{normal_stderr}"
    );

    let tamper_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--",
        program,
        "report-tamper",
    ];
    let tamper = hermit(&tamper_args);
    assert_success(&tamper, &tamper_args);
    assert_eq!(stdout(&tamper), "dbt-unsupported-report-tamper-ok\n");
    assert_eq!(
        stderr(&tamper).matches(warning).count(),
        1,
        "report tampering suppressed the aggregate warning:\n{}",
        stderr(&tamper)
    );

    let fork_tamper_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--",
        program,
        "fork-report-tamper",
    ];
    let fork_tamper = hermit(&fork_tamper_args);
    assert_success(&fork_tamper, &fork_tamper_args);
    assert_eq!(
        stdout(&fork_tamper),
        "dbt-unsupported-fork-report-tamper-ok\n"
    );
    assert_eq!(
        stderr(&fork_tamper).matches(warning).count(),
        1,
        "fork-child report tampering suppressed the aggregate warning:\n{}",
        stderr(&fork_tamper)
    );

    let strict_args = ["run", "--backend", "dbt", "--strict", "--", program];
    let strict = hermit(&strict_args);
    assert!(
        !strict.status.success(),
        "strict DBT unexpectedly succeeded:\n{}",
        stderr(&strict)
    );
    assert!(
        stderr(&strict).contains("unsupported syscall: restart_syscall"),
        "strict DBT failure omitted unsupported syscall:\n{}",
        stderr(&strict)
    );
    let normal_fork_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        program,
        "fork",
    ];
    let normal_fork = hermit(&normal_fork_args);
    assert_success(&normal_fork, &normal_fork_args);
    assert_eq!(stdout(&normal_fork), "dbt-unsupported-fork-ok\n");
    assert_eq!(
        stderr(&normal_fork).matches(warning).count(),
        1,
        "fork-child warning was not aggregated exactly once:\n{}",
        stderr(&normal_fork)
    );

    let normal_fork_exec_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        program,
        "fork-exec",
    ];
    let normal_fork_exec = hermit(&normal_fork_exec_args);
    assert_success(&normal_fork_exec, &normal_fork_exec_args);
    assert_eq!(
        stdout(&normal_fork_exec),
        "dbt-unsupported-exec-ok\ndbt-unsupported-fork-exec-parent-ok\n"
    );
    assert_eq!(
        stderr(&normal_fork_exec).matches(warning).count(),
        1,
        "fork-exec warning was not aggregated exactly once:\n{}",
        stderr(&normal_fork_exec)
    );

    for mode in ["fork", "fork-exec", "fork-setsid-exec", "exec-empty"] {
        let args = ["run", "--backend", "dbt", "--strict", "--", program, mode];
        let output = hermit(&args);
        assert!(
            !output.status.success(),
            "strict DBT {mode} unexpectedly succeeded:\n{}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("unsupported syscall"),
            "strict DBT {mode} omitted unsupported-syscall diagnostic:\n{}",
            stderr(&output)
        );
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review strict DBT teardown with a blocked stdin source.
#[test]
fn run_dbt_strict_returns_with_blocked_stdin_source() {
    if dbt_unavailable("run_dbt_strict_returns_with_blocked_stdin_source") {
        return;
    }
    let program = dbt_unsupported_syscall_guest()
        .to_str()
        .expect("DBT unsupported-syscall guest path should be UTF-8");
    let mut source = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start blocked DBT stdin source");
    let args = ["run", "--backend", "dbt", "--strict", "--", program];
    let output = Command::new("timeout")
        .args(["--kill-after", "2s", "10s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .stdin(source.stdout.take().expect("sleep stdout was not piped"))
        .output()
        .expect("failed to run strict DBT blocked-input regression");
    let _ = source.kill();
    let _ = source.wait();
    assert_ne!(output.status.code(), Some(124), "strict DBT hung on stdin");
    assert!(
        !output.status.success(),
        "strict DBT unexpectedly succeeded"
    );
    assert!(stderr(&output).contains("unsupported syscall"));
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-736): Review the real LiteInst Detcore CLI assertion.
#[test]
fn run_liteinst_verifies_detcore_backend() {
    liteinst_runtime::ensure_liteinst_runtime();
    let args = [
        "run",
        "--backend",
        "liteinst",
        "--strict",
        "--verify",
        "--",
        "/bin/echo",
        "liteinst-cli-ok",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "liteinst-cli-ok\n");
    let stderr = stderr(&output);
    assert!(
        stderr.contains(
            "liteinst host hybrid] activation verified (traps=1, hooks=31); Detcore Tool active in ptrace host"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "LiteInst host hybrid (reverie-liteinst patch runtime + ptrace Detcore Tool)"
        ),
        "{stderr}"
    );
}

/// Backend statistics are a HARNESS record and must stay out of the INFO parity
/// envelope, while remaining available to anyone who asks for DEBUG.
///
/// ⚠️ WHY THE LEVEL IS THE POINT, not a formatting preference.
/// `ComparedLogScope::Info` is `BitwiseInfoV1` — "every INFO message, exactly" —
/// so an INFO record is compared between two runs as if it were guest behaviour.
/// This record is hermit describing its own harness, and both of its fields say
/// which harness: `backend=<NAME>`, and `stats=` carrying that backend's own
/// instrumentation. Measured before the change, it was the ONE record of 303 in
/// a real ptrace INFO stream that named a backend — so two backends running an
/// identical guest could not agree under that envelope however correct they
/// were. Putting it back at INFO restores a divergence by construction.
#[test]
fn backend_stats_are_debug_gated_and_absent_from_the_info_envelope() {
    let default_args = ["run", "--strict", "--", "/bin/true"];
    let default_output = hermit(&default_args);
    assert_success(&default_output, &default_args);
    assert!(!stderr(&default_output).contains("backend run complete"));

    // THE REGRESSION THIS GUARDS: not merely that the record is absent, but that
    // the INFO envelope names no backend at all. A future record reintroducing a
    // backend name at INFO fails here even if it is spelled differently.
    let info_args = ["--log", "info", "run", "--strict", "--", "/bin/true"];
    let info_output = hermit(&info_args);
    assert_success(&info_output, &info_args);
    let info_stderr = stderr(&info_output);
    assert!(
        !info_stderr.contains("backend run complete"),
        "the backend-stats record must not be in the INFO parity envelope:\n{info_stderr}"
    );
    let naming_a_backend: Vec<&str> = info_stderr
        .lines()
        .filter(|line| line.contains(" INFO "))
        .filter(|line| {
            [
                "backend=ptrace",
                "backend=dbt",
                "backend=sabre",
                "backend=liteinst",
                "backend=kvm",
            ]
            .iter()
            .any(|needle| line.contains(needle))
        })
        .collect();
    assert!(
        naming_a_backend.is_empty(),
        "no INFO record may name the backend -- it cannot agree across backends by \
         construction, so it caps cross-backend parity:\n{naming_a_backend:#?}"
    );

    // POSITIVE, so this cannot pass by the record having been deleted: it is
    // still emitted, with both fields, one level down.
    let debug_args = ["--log", "debug", "run", "--strict", "--", "/bin/true"];
    let debug_output = hermit(&debug_args);
    assert_success(&debug_output, &debug_args);
    assert!(
        stderr(&debug_output).contains("backend run complete backend=ptrace stats=metrics=none"),
        "{}",
        stderr(&debug_output)
    );
}

#[test]
fn inherited_container_output_does_not_expose_capture_offset() {
    let _guard = HERMIT_RUN_LOCK.lock().unwrap();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create stdio-lseek test directory");
    let combined_log_path = directory.path().join("hermit-and-guest.log");
    let report_path = directory.path().join("guest-report");
    let guest_file_path = directory.path().join("guest-output");
    let combined_log = fs::File::create(&combined_log_path)
        .expect("failed to create combined Hermit/guest output log");
    let args = ["--log", "info", "run", "--strict", "--"];
    let status = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .arg(stdio_lseek_identity_guest())
        .arg(&report_path)
        .arg(&guest_file_path)
        .stdout(Stdio::from(
            combined_log
                .try_clone()
                .expect("failed to clone combined output log"),
        ))
        .stderr(Stdio::from(combined_log))
        .status()
        .expect("failed to run stdio-lseek identity fixture");
    let combined = fs::read_to_string(&combined_log_path)
        .expect("failed to read combined Hermit/guest output log");
    let report = fs::read_to_string(&report_path).expect("failed to read guest seek report");

    assert!(status.success(), "Hermit failed:\n{combined}");
    assert!(
        report.contains("inherited-stdout offset=-1 errno=29"),
        "inherited stdout exposed the outer capture offset:\n{report}"
    );
    assert!(
        report.contains("inherited-stderr offset=-1 errno=29"),
        "inherited stderr exposed the outer capture offset:\n{report}"
    );
    assert!(
        report.contains("stdout-alias offset=-1 errno=29"),
        "a dup of inherited stdout exposed the outer capture offset:\n{report}"
    );
    assert!(
        report.contains("stderr-alias offset=-1 errno=29"),
        "a dup of inherited stderr exposed the outer capture offset:\n{report}"
    );
    assert!(
        report.contains("guest-file-stdout offset=0 errno=0"),
        "guest-installed stdout lost ordinary file seek semantics:\n{report}"
    );
    assert!(
        report.contains("guest-file-stderr offset=0 errno=0"),
        "guest-installed stderr lost ordinary file seek semantics:\n{report}"
    );
}

#[test]
fn run_liteinst_rejects_a_non_runtime_override_before_activation_claim() {
    let args = [
        "run",
        "--backend",
        "liteinst",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_LITEINST_RUNTIME", "/bin/true")
        .args(args)
        .output()
        .expect("failed to run Hermit with a false LiteInst runtime");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("missing required export"), "{stderr}");
    assert!(!stderr.contains("activation verified"), "{stderr}");
    assert!(!stderr.contains("Success: deterministic"), "{stderr}");
}

#[test]
fn run_liteinst_rejects_an_inert_dso_before_activation_claim() {
    let args = [
        "run",
        "--backend",
        "liteinst",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_LITEINST_RUNTIME", liteinst_inert_runtime())
        .args(args)
        .output()
        .expect("failed to run Hermit with an inert LiteInst runtime");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("does not register reverie_liteinst_initialize as a preload constructor"),
        "{stderr}"
    );
    assert!(!stderr.contains("activation verified"), "{stderr}");
    assert!(!stderr.contains("Success: deterministic"), "{stderr}");
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#679): validate the dedicated DBT diagnostic channel.
#[test]
fn run_dbt_keeps_diagnostics_out_of_guest_stderr() {
    if dbt_unavailable("run_dbt_keeps_diagnostics_out_of_guest_stderr") {
        return;
    }
    let program = dbt_stderr_guest()
        .to_str()
        .expect("DBT stderr guest path should be UTF-8");
    let script = r#"set -euo pipefail; output=$("$1" 2>&1); test "$output" = guest-stderr; printf 'isolated=%s\n' "$output""#;
    let args = [
        "--log",
        "INFO",
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--",
        "/bin/bash",
        "-c",
        script,
        "dbt-stderr-fixture",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "isolated=guest-stderr\n");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("INFO detcore") && stderr.contains("DETLOG [syscall]"),
        "DBT child diagnostics were not emitted:\n{stderr}"
    );
    assert!(
        !stderr.contains("guest-stderr"),
        "guest fd 2 leaked into controller diagnostics:\n{stderr}"
    );

    // Reported verification transports controller evidence out of band and must
    // not overwrite the guest's own HERMIT_LOG value.  The canonical comparator
    // can currently diverge on run-specific DBT process IDs, so this test owns
    // the environment/evidence contract, not that separate determinism defect.
    for (guest_value, expected) in [
        (None, "<unset>"),
        (Some("guest-sentinel"), "guest-sentinel"),
    ] {
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("failed to create DBT log-env verification directory");
        let logs = directory.path().join("logs");
        fs::create_dir(&logs).expect("failed to create DBT log-env verification log directory");
        let verdict = directory.path().join("verdict.json");
        let expected_arg = format!("expect={expected}");
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command
            .args([
                "--log",
                "INFO",
                "run",
                "--backend",
                "dbt",
                "--strict",
                "--verify",
                "--verify-strict",
                "--keep-logs",
                "--verify-log-dir",
            ])
            .arg(&logs)
            .arg("--verify-json")
            .arg(&verdict)
            .arg("--")
            .arg(dbt_log_env_guest())
            .arg(&expected_arg)
            .env_remove("HERMIT_LOG");
        if let Some(value) = guest_value {
            command.env("HERMIT_LOG", value);
        }
        let output = command.output().expect("failed to run DBT log-env case");
        let report = read_terminal_dbt_verdict(&verdict);
        assert_eq!(
            output.status.success(),
            report["verified"] == true,
            "process status disagrees with terminal verdict: {report}"
        );
        let retained_logs = fs::read_dir(&logs)
            .expect("failed to read retained DBT log-env verification logs")
            .map(|entry| entry.expect("failed to read retained log entry").path())
            .collect::<Vec<_>>();
        assert_eq!(retained_logs.len(), 2, "unexpected logs: {retained_logs:?}");
        for log in retained_logs {
            let contents = fs::read_to_string(&log).expect("failed to read retained DBT log");
            assert!(contents.contains("INFO detcore"), "empty INFO log: {log:?}");
            assert!(
                !contents.contains("hermit_log="),
                "guest stdout leaked into DBT diagnostics: {log:?}"
            );
        }
    }
}

#[test]
fn run_dbt_forwards_detcore_info_logs() {
    if dbt_unavailable("run_dbt_forwards_detcore_info_logs") {
        return;
    }
    let args = [
        "--log",
        "INFO",
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("INFO detcore") && stderr.contains("DETLOG [syscall]"),
        "DBT did not forward the Detcore INFO syscall stream:\n{stderr}",
    );
}

#[test]
fn run_dbt_uses_the_normalized_backend_config() {
    if dbt_unavailable("run_dbt_uses_the_normalized_backend_config") {
        return;
    }
    let args = [
        "--log",
        "DEBUG",
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("detcore-dbt: using CLI-provided Detcore Config"),
        "DBT did not consume the CLI-provided config:\n{stderr}",
    );
    assert!(
        stderr.contains("backend_requires_thread_directed_process_signals: true"),
        "DBT did not receive its required process-signal translation capability:\n{stderr}",
    );
    assert!(
        !stderr.contains("backend_requires_thread_directed_process_signals: false"),
        "DBT received an unnormalized process-signal capability:\n{stderr}",
    );
}

// TODO-HUMAN-REVIEW(PR-1038): Review DBT queued self-signal verification.
#[test]
fn run_dbt_verifies_queued_self_signals() {
    if dbt_unavailable("run_dbt_verifies_queued_self_signals") {
        return;
    }
    let program = dbt_self_sigqueue_guest()
        .to_str()
        .expect("DBT self-sigqueue guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "dbt-self-sigqueue-ok\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#543): validate the explicit application-mmap DBT regression.
#[test]
fn run_dbt_verifies_application_mmap() {
    if dbt_unavailable("run_dbt_verifies_application_mmap") {
        return;
    }
    let program = dbt_mmap_guest()
        .to_str()
        .expect("DBT mmap guest path should be UTF-8");
    let args = ["run", "--backend", "dbt", "--verify", "--", program];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "dbt-mmap-exec-ok\n");
    assert!(
        stderr(&output).contains(":: DBT path confirmed: DynamoRIO client reported tool=Detcore"),
        "DBT confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbt_verifies_process_wait_lifecycle() {
    if dbt_unavailable("run_dbt_verifies_process_wait_lifecycle") {
        return;
    }
    let program = dbt_wait_guest()
        .to_str()
        .expect("DBT wait guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "wait4=7 waitid=9 sigchld=observed reaped=2 cpu=zero\n"
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_exact_child_waits_have_stable_scheduler_turns() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM exact-child waits: /dev/kvm is unavailable");
        return;
    }

    let _guard = hermit_run_guard();
    let program = kvm_exact_child_waits_guest()
        .to_str()
        .expect("exact-child wait guest path should be UTF-8");
    let args = [
        "--log=info",
        "run",
        "--backend=kvm",
        "--strict",
        "--max-timeslice=disabled",
        "--tmp=/tmp",
        "--",
        program,
    ];
    for iteration in 0..4 {
        let output = hermit(&args);

        assert_success(&output, &args);
        assert!(
            stdout(&output)
                == "wait4=7 waitid=9 wait4-any=11 waitid-any=13 \
                live-wnohang=empty child-ready-won\n",
            "iteration {iteration} did not exercise the KVM child-wait contract: {:?}",
            stdout(&output)
        );
        let log = stderr(&output);
        assert!(
            log.contains("hermit::kvm: launching guest through reverie-kvm"),
            "iteration {iteration} did not use the KVM backend:\n{log}"
        );
        let sigchld_deliveries = log
            .lines()
            .filter(|line| line.contains("Alarm fired, delivering signal SIGCHLD"))
            .count();
        assert!(
            sigchld_deliveries >= 4,
            "iteration {iteration} did not race each ready child against SIGCHLD:\n{log}"
        );
        let exact_wait_turns = log
            .lines()
            .filter(|line| {
                line.contains("resources {WaitChild") && line.contains("selector: Exact")
            })
            .count();
        let any_wait_turns = log
            .lines()
            .filter(|line| line.contains("resources {WaitChild") && line.contains("selector: Any"))
            .count();
        assert_eq!(
            (exact_wait_turns, any_wait_turns),
            (2, 2),
            "iteration {iteration} changed the scheduler child-wait turn population:\n{log}"
        );
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-723): Review DBT PID virtualization L2 coverage.
#[test]
fn run_dbt_virtualizes_process_identities() {
    if dbt_unavailable("run_dbt_virtualizes_process_identities") {
        return;
    }
    let program = dbt_pid_guest()
        .to_str()
        .expect("DBT PID guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        concat!(
            "root pid=3 ppid=1 tid=3\n",
            "grandchild pid=5 ppid=4 tid=5\n",
            "child pid=4 ppid=3 tid=4\n",
            "child grandchild=5 waited=5 exit=5\n",
            "root child=4 waited=4 exit=6\n",
            "exec-child pid=6 ppid=3 tid=6\n",
            "exec-proc stat=6/3 status=6/3 tracer=1\n",
            "root exec=6 waited=6 exit=8\n",
            "waitid-child pid=7 ppid=3 tid=7\n",
            "root waitid=7 reported=7 exit=9\n",
            "root vfork=8 waited=8 exit=0 pid=3 tid=3\n",
            "vfork-exec-child pid=9 ppid=3 tid=9\n",
            "root vfork-exec=9 waited=9 exit=10 pid=3 tid=3\n",
        )
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1065): Review DBT self-prlimit L2 coverage.
#[test]
fn run_dbt_verifies_self_prlimit() {
    if dbt_unavailable("run_dbt_verifies_self_prlimit") {
        return;
    }
    let program = dbt_prlimit_self_guest()
        .to_str()
        .expect("DBT self-prlimit guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "dbt-prlimit-self-ok\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbt_verifies_shell_process_lifecycle() {
    if dbt_unavailable("run_dbt_verifies_shell_process_lifecycle") {
        return;
    }
    let args = [
        "run",
        "--backend",
        "dbt",
        "--verify",
        "--",
        "/bin/sh",
        "-c",
        "/bin/echo hello; :",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#598): Confirm this captures the host-inherited O_NONBLOCK regression.
// TODO-HUMAN-REVIEW(#689): Confirm the split-write case protects partial-read semantics.
#[test]
fn run_dbt_verifies_pipe_backpressure() {
    if dbt_unavailable("run_dbt_verifies_pipe_backpressure") {
        return;
    }
    let args = [
        "run",
        "--backend",
        "dbt",
        "--verify",
        "--",
        "/bin/bash",
        "-c",
        r#"{ printf "%4096s" x; for _ in {1..100000}; do :; done; printf "%1371s" y; } | wc -c"#,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "5467\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbt_recovers_after_failed_exec() {
    if dbt_unavailable("run_dbt_recovers_after_failed_exec") {
        return;
    }
    let program = dbt_exec_failure_guest()
        .to_str()
        .expect("DBT exec-failure guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "recovered after failed exec\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}
#[test]
fn run_dbt_rejects_unfollowed_execveat() {
    if dbt_unavailable("run_dbt_rejects_unfollowed_execveat") {
        return;
    }
    let program = dbt_execveat_guest()
        .to_str()
        .expect("DBT execveat guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "execveat unsupported in root and fork child\n"
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_executes_dynamic_guest() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--",
        "/bin/echo",
        "hello",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
    assert!(
        !stderr(&output).contains("Hermit cannot use ptrace"),
        "kvm must not fall through to the ptrace backend:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_awk_mincore_probe_terminates() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/awk").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--",
        "/usr/bin/awk",
        "BEGIN { print 42 }",
    ];
    let output = Command::new("timeout")
        .args(["--kill-after", "2s", "20s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .output()
        .expect("failed to run the KVM awk mincore regression");

    assert_ne!(
        output.status.code(),
        Some(124),
        "KVM awk mincore probe hung"
    );
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "42\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "KVM determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_resolves_bare_program_from_guest_path() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "echo",
        "from-kvm-path",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "from-kvm-path\n");
}

#[test]
fn run_kvm_setpriv_capability_wrapper_is_deterministic() {
    if !Path::new("/dev/kvm").exists()
        || !Path::new("/usr/bin/setpriv").exists()
        || !Path::new("/bin/date").exists()
    {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/setpriv",
        "--bounding-set=-sys_time",
        "/bin/date",
        "-u",
        "+%s",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "1767225600\n");
    assert!(stderr(&output).contains(":: Success: deterministic. Determinism verified."));
}

/// The two sanitizer variables Hermit forces into *every* guest, on *every*
/// backend.
///
/// `hermit-cli/src/bin/hermit/run.rs` sets both to `detect_leaks=0` in one
/// backend-independent place. That is a deliberate cross-backend parity fix,
/// not a leak: the ptrace family forces them at spawn time inside
/// `reverie-ptrace`, while the out-of-process KVM backend has no such spawn
/// hook, so without this the guest would see two fewer variables under KVM than
/// under ptrace -- directly observable as a differing DETLOG `[env ...]` hash.
/// The unit test `guest_env_disables_sanitizer_leak_detection_on_every_backend`
/// pins that invariant where the command is constructed; this file observes it
/// end to end, in the guest's own `env` output.
///
/// So these two lines are *expected* output of `--base-env=empty`. Measured on
/// a dedicated KVM host at hermit `2b6005cf`: `--backend kvm` and `--backend
/// ptrace` both emit exactly this pair plus the explicit value, byte-identical, 5/5 runs
/// each.
const FORCED_GUEST_ENV: [&str; 2] = ["ASAN_OPTIONS=detect_leaks=0", "LSAN_OPTIONS=detect_leaks=0"];

/// Describes how a guest's `env` output differs from *exactly*
/// [`FORCED_GUEST_ENV`] plus `explicit`; `None` means it matched exactly.
///
/// This is an EXCLUSIVE set comparison on purpose. A `contains` check would
/// pass when a host variable leaks in *and* pass when the explicit variable is
/// dropped, so it could not tell a working `--base-env=empty` from a broken
/// one. This test has already been non-discriminating once -- it silently
/// skipped and reported success -- and a `contains` check would be the same
/// defect wearing a different costume.
fn guest_env_difference(stdout: &str, explicit: &[&str]) -> Option<String> {
    let mut expected: Vec<&str> = FORCED_GUEST_ENV
        .iter()
        .copied()
        .chain(explicit.iter().copied())
        .collect();
    expected.sort_unstable();
    let mut actual: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    actual.sort_unstable();
    if actual == expected {
        return None;
    }
    let missing: Vec<&str> = expected
        .iter()
        .filter(|value| !actual.contains(value))
        .copied()
        .collect();
    let unexpected: Vec<&str> = actual
        .iter()
        .filter(|value| !expected.contains(value))
        .copied()
        .collect();
    Some(format!(
        "guest environment mismatch: missing {missing:?}, unexpected {unexpected:?} \
         (observed {actual:?})"
    ))
}

// The four controls below deliberately do NOT invoke Hermit and are NOT named
// `run_kvm_*`. They therefore run on every host, including one without
// `/dev/kvm`, and survive the `--skip run_kvm_` filter that the portable DAG
// applies to `test.cli`. A control that only runs where the thing it controls
// runs is not a control.

#[test]
fn guest_env_difference_accepts_the_forced_pair_plus_the_explicit_value() {
    let observed = "ASAN_OPTIONS=detect_leaks=0\nKVM_M3C=passed\nLSAN_OPTIONS=detect_leaks=0\n";
    assert_eq!(guest_env_difference(observed, &["KVM_M3C=passed"]), None);
}

/// The exact string this test asserted before it was corrected -- which is also
/// the *pre-parity-fix* KVM behaviour, where the guest saw the explicit value
/// and not the two sanitizer variables. The corrected expectation must reject
/// it, or it would still pass against the divergence the product already fixed.
#[test]
fn guest_env_difference_rejects_the_pre_parity_fix_output() {
    let difference = guest_env_difference("KVM_M3C=passed\n", &["KVM_M3C=passed"])
        .expect("the pre-parity-fix output must not satisfy the corrected expectation");
    assert!(
        difference.contains("ASAN_OPTIONS=detect_leaks=0"),
        "{difference}"
    );
    assert!(
        difference.contains("LSAN_OPTIONS=detect_leaks=0"),
        "{difference}"
    );
}

#[test]
fn guest_env_difference_rejects_a_leaked_host_variable() {
    let observed = "ASAN_OPTIONS=detect_leaks=0\nKVM_HOST_ONLY=must-not-leak\n\
                    KVM_M3C=passed\nLSAN_OPTIONS=detect_leaks=0\n";
    let difference = guest_env_difference(observed, &["KVM_M3C=passed"])
        .expect("a leaked host variable must fail the expectation");
    assert!(
        difference.contains("KVM_HOST_ONLY=must-not-leak"),
        "{difference}"
    );
}

#[test]
fn guest_env_difference_rejects_a_dropped_explicit_variable() {
    let observed = "ASAN_OPTIONS=detect_leaks=0\nLSAN_OPTIONS=detect_leaks=0\n";
    let difference = guest_env_difference(observed, &["KVM_M3C=passed"])
        .expect("a dropped explicit variable must fail the expectation");
    assert!(difference.contains("KVM_M3C=passed"), "{difference}");
}

#[test]
fn run_kvm_propagates_explicit_environment() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=empty",
        "--env=KVM_M3C=passed",
        "--",
        "/usr/bin/env",
    ];
    // Plant a host-only value, as `run_dbt_uses_the_requested_guest_environment`
    // does: `--base-env=empty` only means something if there was something to
    // exclude. The exclusive comparison below fails on this value specifically
    // and on any other unexpected one.
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("KVM_HOST_ONLY", "must-not-leak")
        .args(args)
        .output()
        .expect("failed to run the KVM guest-environment regression");

    assert_success(&output, &args);
    let stdout = stdout(&output);
    assert_eq!(
        guest_env_difference(&stdout, &["KVM_M3C=passed"]),
        None,
        "guest environment was not exactly the forced sanitizer pair plus the \
         explicit value:\n{stdout}",
    );
}

#[test]
fn run_kvm_bash_process_substitution_is_deterministic() {
    if !Path::new("/dev/kvm").exists()
        || !Path::new("/bin/bash").exists()
        || !Path::new("/usr/bin/paste").exists()
        || !Path::new("/usr/bin/diff").exists()
    {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/bash",
        "-c",
        r#"set -euo pipefail; /usr/bin/paste -d: <(printf "alpha\nbeta\n") <(printf "1\n2\n") | /usr/bin/diff -u <(printf "alpha:1\nbeta:2\n") -; printf "paste-ok\n""#,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "paste-ok\n");
    assert!(stderr(&output).contains(":: Success: deterministic. Determinism verified."));
}

#[test]
fn run_kvm_cpuid_policy_is_deterministic() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .args(["-x", "c", "-fsyntax-only", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("KVM CPUID regression requires cc, gcc, or clang on PATH");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-cpuid");
    fs::create_dir_all(&build_root).expect("failed to create KVM CPUID guest directory");
    let binary = build_root.join("cpuid_probe");
    let compile = Command::new(compiler)
        .args(["-O2", "-g", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/backend-parity/fixtures/cpuid_probe.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("failed to compile KVM CPUID guest");
    assert!(
        compile.status.success(),
        "KVM CPUID guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    let program = binary.to_str().expect("CPUID guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "CPUID-SUCCESS vendor=GenuineIntel signature=00000663\n"
    );
    assert!(stderr(&output).contains(":: Success: deterministic. Determinism verified."));
}

#[test]
fn run_kvm_respects_workdir_for_relative_paths() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let temp = tempfile::tempdir().expect("failed to create KVM cwd fixture");
    fs::write(temp.path().join("message.txt"), b"from-kvm-cwd\n")
        .expect("failed to write KVM cwd fixture");
    let workdir = temp
        .path()
        .to_str()
        .expect("temporary path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--tmp=/tmp",
        "--workdir",
        workdir,
        "--",
        "/bin/cat",
        "message.txt",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "from-kvm-cwd\n");
}

#[test]
fn run_kvm_lists_host_directory_metadata() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let temp = tempfile::tempdir().expect("failed to create KVM directory fixture");
    fs::write(temp.path().join("alpha.txt"), b"alpha\n")
        .expect("failed to write KVM directory fixture");
    fs::create_dir(temp.path().join("subdir")).expect("failed to create KVM subdirectory");
    std::os::unix::fs::symlink("alpha.txt", temp.path().join("alpha-link"))
        .expect("failed to create KVM symlink fixture");
    let workdir = temp
        .path()
        .to_str()
        .expect("temporary path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--verify",
        "--base-env=minimal",
        "--tmp=/tmp",
        "--workdir",
        workdir,
        "--",
        "/bin/ls",
        "-ln",
        ".",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    let listing = stdout(&output);
    let alpha = listing
        .lines()
        .find(|line| line.ends_with(" alpha.txt") && !line.contains(" -> "))
        .unwrap_or_else(|| panic!("missing file in:\n{listing}"));
    let alpha_fields: Vec<_> = alpha.split_whitespace().collect();
    assert!(alpha_fields[0].starts_with("-rw"), "bad file mode: {alpha}");
    assert_eq!(alpha_fields[4], "6", "bad file size: {alpha}");
    let subdir = listing
        .lines()
        .find(|line| line.ends_with(" subdir"))
        .unwrap_or_else(|| panic!("missing directory in:\n{listing}"));
    assert!(subdir.starts_with("d"), "bad directory type: {subdir}");
    let link = listing
        .lines()
        .find(|line| line.ends_with(" alpha-link -> alpha.txt"))
        .unwrap_or_else(|| panic!("missing symlink in:\n{listing}"));
    assert!(link.starts_with("l"), "bad symlink type: {link}");
}

#[test]
fn run_kvm_reads_host_file() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let expected = fs::read_to_string("/etc/hostname").expect("failed to read host hostname");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/cat",
        "/etc/hostname",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), expected);
}

#[test]
fn run_kvm_reads_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/bin/cat",
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
}

#[test]
fn run_kvm_f_getfl_and_reads_standard_input() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/usr/bin/perl",
        "-MFcntl=F_GETFL",
        "-e",
        r#"defined(fcntl(STDIN, F_GETFL, 0)) or die "fcntl failed: $!\n"; my $line = <STDIN>; defined($line) && $line eq "hello\n" or die "stdin mismatch\n"; print "fcntl-stdin-ok\n";"#,
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "fcntl-stdin-ok\n");
}

#[test]
fn run_kvm_verify_f_getfl_with_isolated_standard_input() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/perl",
        "-MFcntl=F_GETFL",
        "-e",
        r#"defined(fcntl(STDIN, F_GETFL, 0)) or die "fcntl failed: $!\n"; my $line = <STDIN>; !defined($line) or die "verify stdin was not isolated\n"; print "fcntl-verify-ok\n";"#,
    ];
    let output = hermit_with_stdin(&args, b"not-visible-during-capture\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "fcntl-verify-ok\n");
    assert!(stderr(&output).contains(":: Success: deterministic. Determinism verified."));
}

#[test]
fn run_kvm_verify_isolates_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/cat",
    ];
    let output = hermit_with_stdin(&args, b"not-visible-during-capture\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "");
}

#[test]
fn run_kvm_preserves_closed_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/bin/cat",
    ];
    let output = hermit_with_closed_stdin(&args);

    // ⚠️ THE GUEST RAN, SO THIS IS THE GUEST'S OWN STATUS AND NOT A `Refusal`.
    // `/bin/cat` starts, finds stdin closed, prints its own diagnostic
    // ("/bin/cat: -: Bad file descriptor") and exits 1 of its own accord. That
    // the closed descriptor is PRESERVED INTO THE GUEST is the whole subject of
    // this test, so a hermit-internal code here would assert the opposite of
    // what the test exists to check.
    //
    // This assertion was briefly changed to `HERMIT_INTERNAL_FAILURE_EXIT` on
    // the theory that no guest was launched. It was wrong, and the reason it
    // looked right is worth keeping: from a checkout under /tmp the case fails
    // earlier, with "failed to resolve KVM guest working directory", and that
    // unrelated failure hides the real exit status. Verify this one from a
    // checkout OUTSIDE /tmp or the evidence is not about this test.
    //
    // So this is a deliberate bare literal, like the guest arm of
    // `a_guest_side_fault_is_not_reported_as_a_hermit_internal_failure`: it must
    // NOT track a hermit constant, because it is not hermit's number.
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected the guest's own exit status to survive a closed stdin: {output:?}"
    );
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output)
            .to_ascii_lowercase()
            .contains("bad file descriptor")
    );
}

#[test]
fn run_kvm_verify_does_not_write_to_standard_input() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    let temp = tempfile::tempdir().expect("failed to create stdin fixture");
    let path = temp.path().join("stdin");
    fs::write(&path, b"original-data").expect("failed to write stdin fixture");
    let stdin = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("failed to open stdin fixture");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/perl",
        "-MPOSIX",
        "-e",
        "POSIX::write(0, \"leak\", 4); exit 0",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .stdin(Stdio::from(stdin))
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));

    assert_success(&output, &args);
    assert_eq!(fs::read(path).unwrap(), b"original-data");
}

#[test]
fn run_kvm_counts_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/usr/bin/wc",
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output).split_whitespace().collect::<Vec<_>>(),
        ["1", "1", "6"]
    );
}

#[test]
fn run_kvm_reports_hostname() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/hostname",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hermetic-container.local\n");
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#544): Confirm the host C compiler is acceptable for this KVM smoke guest.
#[test]
fn run_kvm_pipe_pipe2_and_getgroups_round_trip() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .args(["-x", "c", "-fsyntax-only", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("KVM syscall regression requires cc, gcc, or clang on PATH");

    let temp = tempfile::tempdir().expect("failed to create pipe guest directory");
    let source = temp.path().join("pipe_roundtrip.c");
    let binary = temp.path().join("pipe_roundtrip");
    fs::write(
        &source,
        br#"#define _GNU_SOURCE
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int roundtrip(int flags) {
    int fds[2];
    char buffer[3] = {0};
    int result = flags < 0 ? pipe(fds) : pipe2(fds, flags);
    if (result != 0) return 1;
    if (write(fds[1], "ok", 2) != 2) return 2;
    if (read(fds[0], buffer, 2) != 2) return 3;
    if (close(fds[0]) != 0 || close(fds[1]) != 0) return 4;
    return strcmp(buffer, "ok") != 0;
}

int main(void) {
    gid_t groups[1] = {0};
    if (roundtrip(-1) || roundtrip(O_CLOEXEC | O_NONBLOCK)) return 1;
    if (getgroups(0, NULL) != 1) return 5;
    if (getgroups(1, groups) != 1 || groups[0] != 65534) return 6;
    puts("kvm-syscalls-ok");
    return 0;
}
"#,
    )
    .expect("failed to write pipe guest");
    let compile = Command::new(compiler)
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .expect("failed to invoke C compiler");
    assert!(
        compile.status.success(),
        "failed to compile pipe guest: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let program = binary.to_str().expect("pipe guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--tmp=/tmp",
        "--base-env=minimal",
        "--",
        program,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "kvm-syscalls-ok\n");
}

#[test]
fn run_kvm_random_device_lseek_matches_linux() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let _guard = hermit_run_guard();
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .args(["-x", "c", "-fsyntax-only", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("random-device lseek regression requires cc, gcc, or clang on PATH");

    let temp = tempfile::tempdir().expect("failed to create random-device lseek guest directory");
    let source = temp.path().join("random_device_lseek.c");
    let binary = temp.path().join("random_device_lseek");
    fs::write(
        &source,
        br#"#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int expect_lseek(int fd, off_t offset, int whence, off_t expected,
                        int expected_errno, const char *label) {
    errno = 0;
    off_t actual = syscall(SYS_lseek, fd, offset, whence);
    int actual_errno = errno;
    if (actual == expected && actual_errno == expected_errno) return 0;
    fprintf(stderr,
            "%s: expected %lld errno %d, got %lld errno %d\n",
            label, (long long)expected, expected_errno,
            (long long)actual, actual_errno);
    return 1;
}

int main(int argc, char **argv) {
    int opath = argc == 2 && strcmp(argv[1], "--opath") == 0;
    int fd = open("/dev/urandom", opath ? O_PATH : O_RDONLY);
    if (fd < 0) {
        perror("open /dev/urandom");
        return 1;
    }

    int failed = 0;
    if (opath) {
        failed |= expect_lseek(fd, 0, SEEK_SET, -1, EBADF, "O_PATH SEEK_SET");
        failed |= expect_lseek(fd, -4, SEEK_CUR, -1, EBADF, "O_PATH SEEK_CUR");
        failed |= expect_lseek(fd, 0, SEEK_END, -1, EBADF, "O_PATH SEEK_END");
        failed |= expect_lseek(fd, 0, SEEK_DATA, -1, EBADF, "O_PATH SEEK_DATA");
        failed |= expect_lseek(fd, 0, SEEK_HOLE, -1, EBADF, "O_PATH SEEK_HOLE");
        failed |= expect_lseek(fd, 0, 99, -1, EBADF, "O_PATH invalid whence");
        if (close(fd) != 0) {
            perror("close O_PATH /dev/urandom");
            return 2;
        }
        if (failed) return 3;
        puts("random-device-lseek-opath-ok");
        return 0;
    }

    unsigned char bytes[8];
    if (read(fd, bytes, sizeof(bytes)) != (ssize_t)sizeof(bytes)) {
        perror("read /dev/urandom before lseek");
        return 4;
    }
    failed |= expect_lseek(fd, -4, SEEK_CUR, 0, 0, "SEEK_CUR");
    failed |= expect_lseek(fd, 123, SEEK_SET, 0, 0, "SEEK_SET");
    failed |= expect_lseek(fd, 0, SEEK_END, 0, 0, "SEEK_END");
    failed |= expect_lseek(fd, 0, SEEK_DATA, 0, 0, "SEEK_DATA");
    failed |= expect_lseek(fd, 0, SEEK_HOLE, 0, 0, "SEEK_HOLE");
    failed |= expect_lseek(fd, 0, 99, -1, EINVAL, "invalid whence");
    if (read(fd, bytes, sizeof(bytes)) != (ssize_t)sizeof(bytes)) {
        perror("read /dev/urandom after lseek");
        return 5;
    }
    if (close(fd) != 0) {
        perror("close /dev/urandom");
        return 6;
    }
    if (failed) return 7;
    puts("random-device-lseek-ok");
    return 0;
}
"#,
    )
    .expect("failed to write random-device lseek guest");
    let compile = Command::new(compiler)
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .expect("failed to invoke C compiler");
    assert!(
        compile.status.success(),
        "failed to compile random-device lseek guest:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    let program = binary
        .to_str()
        .expect("random-device lseek guest path should be UTF-8");
    let kvm_args = [
        "--log=info",
        "run",
        "--backend=kvm",
        "--strict",
        "--verify",
        "--verify-strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--tmp=/tmp",
        "--base-env=minimal",
        "--",
        program,
    ];
    let kvm_output = hermit(&kvm_args);
    assert_success(&kvm_output, &kvm_args);
    assert_eq!(stdout(&kvm_output), "random-device-lseek-ok\n");
    assert!(
        stderr(&kvm_output).contains(":: Success: deterministic. Determinism verified."),
        "KVM determinism confirmation missing:\n{}",
        stderr(&kvm_output),
    );

    // Reverie KVM currently rejects O_PATH at openat. Exercise the Linux
    // lseek error precedence through the ptrace backend until KVM accepts the
    // descriptor itself.
    let ptrace_args = [
        "--log=info",
        "run",
        "--backend=ptrace",
        "--strict",
        "--verify",
        "--verify-strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--tmp=/tmp",
        "--base-env=minimal",
        "--",
        program,
        "--opath",
    ];
    let ptrace_output = hermit(&ptrace_args);
    assert_success(&ptrace_output, &ptrace_args);
    assert_eq!(stdout(&ptrace_output), "random-device-lseek-opath-ok\n");
    assert!(
        stderr(&ptrace_output).contains("Success: deterministic. Determinism verified."),
        "ptrace determinism confirmation missing:\n{}",
        stderr(&ptrace_output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#544): Confirm 65534 remains the fixed container overflow group.
#[test]
fn run_kvm_reports_fixed_supplementary_groups() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let kvm_args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "id",
        "-G",
    ];
    let kvm_output = hermit(&kvm_args);
    assert_success(&kvm_output, &kvm_args);
    assert_eq!(
        stdout(&kvm_output),
        "0 65534\n",
        "KVM must report its root-plus-overflow-group credential persona"
    );
}

#[test]
fn namespace_only_rejects_every_explicit_backend() {
    for backend in ["ptrace", "dbt", "kvm"] {
        let args = [
            "run",
            "--backend",
            backend,
            "--namespace-only",
            "--",
            "/bin/true",
        ];
        let output = hermit(&args);
        assert_eq!(output.status.code(), Some(2));
        let message = stderr(&output);
        assert!(
            message.contains("--backend"),
            "unexpected error:\n{message}"
        );
        assert!(
            message.contains("--namespace-only"),
            "unexpected error:\n{message}"
        );
    }
}

#[test]
fn backend_accepted_in_global_position() {
    if dbt_unavailable("backend_accepted_in_global_position") {
        return;
    }
    // The global-position `--backend` (before the subcommand) must be threaded
    // through to `run` and reach the integrated DBT backend.
    let dbt_args = ["--backend", "dbt", "run", "--", "/bin/true"];
    let dbt = hermit(&dbt_args);

    assert_success(&dbt, &dbt_args);

    if Path::new("/dev/kvm").exists() {
        let args = ["--backend", "kvm", "run", "--", "/bin/true"];
        let kvm = hermit(&args);
        assert_success(&kvm, &args);
        assert!(
            !stderr(&kvm).contains("Hermit cannot use ptrace"),
            "global-position kvm should reach its dispatch:\n{}",
            stderr(&kvm),
        );
    }
}

#[test]
fn sabre_backend_validation_honors_command_scope() {
    let non_run = hermit(&["--backend", "sabre", "record", "list"]);
    assert_hermit_refusal_contains(
        &non_run,
        Refusal::Hermit,
        &["SaBRe backend", "only through", "strace"],
    );

    let local_override = hermit(&[
        "--backend",
        "sabre",
        "run",
        "--backend",
        "ptrace",
        "--",
        "/definitely/missing/sabre-backend-override-test",
    ]);
    assert_hermit_refusal_contains(
        &local_override,
        Refusal::GuestNotFound,
        &["does not exist or is not accessible"],
    );
    assert!(!stderr(&local_override).contains("SaBRe backend"));

    let log = hermit(&[
        "--backend",
        "sabre",
        "--log",
        "info",
        "strace",
        "--",
        "/bin/true",
    ]);
    assert_hermit_refusal_contains(
        &log,
        Refusal::Hermit,
        &["does not support --log or --log-file"],
    );
}

#[test]
fn sabre_rpc_socket_is_hidden_from_proc_environ() {
    let hermit_binary = Path::new(env!("CARGO_BIN_EXE_hermit"));
    let executable_dir = hermit_binary.parent().unwrap();
    let target_dir = executable_dir.parent().unwrap();
    let loader = target_dir.join("sabre/sabre");
    let plugin = executable_dir.join("libdetcore_sabre.so");
    if !loader.is_file() || !plugin.is_file() {
        return;
    }

    let _guard = hermit_run_guard();
    let args = [
        "run",
        "--backend",
        "sabre",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/cat",
        "/proc/self/environ",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    let guest_environment = stdout(&output);
    assert!(
        !guest_environment.contains("REVERIE_SABRE_HERMIT_RPC_SOCKET"),
        "private coordinator setting leaked through procfs: {guest_environment:?}"
    );
    assert!(
        stderr(&output).contains("Determinism verified"),
        "strict repeat verification did not complete:\n{}",
        stderr(&output)
    );
}

#[test]
fn sabre_rpc_socket_ignores_host_tmpdir_hidden_by_container_tmp() {
    let hermit_binary = Path::new(env!("CARGO_BIN_EXE_hermit"));
    let executable_dir = hermit_binary.parent().unwrap();
    let target_dir = executable_dir.parent().unwrap();
    let loader = std::env::var_os("HERMIT_SABRE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| target_dir.join("sabre/sabre"));
    let plugin = std::env::var_os("HERMIT_INSTALL_DIR")
        .map(PathBuf::from)
        .map(|install| install.join("rsrcs/libdetcore_sabre.so"))
        .unwrap_or_else(|| executable_dir.join("libdetcore_sabre.so"));
    if !loader.is_file() || !plugin.is_file() {
        // ⚠️ SAY SO. A bare `return` here reports `ok` in 0.00s having executed
        // nothing, and the reader concludes the namespace fix is verified when
        // the test never ran. `sabre_examples.rs` already prints its skip for
        // exactly this reason; matching that rather than inventing a form.
        eprintln!(
            "skipping SaBRe RPC TMPDIR check: artifacts are unavailable: loader={}, plugin={}",
            loader.display(),
            plugin.display()
        );
        return;
    }

    let host_tmpdir = tempfile::Builder::new()
        .prefix("sabre-host-tmpdir-")
        .tempdir_in("/tmp")
        .expect("failed to create nested host TMPDIR");
    let verify_report =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("sabre-nested-host-tmpdir-verify.json");
    let _ = fs::remove_file(&verify_report);

    let _guard = hermit_run_guard();
    let args = [
        "--log=info",
        "run",
        "--backend",
        "sabre",
        "--strict",
        "--verify",
        "--verify-strict",
        "--verify-json",
        verify_report.to_str().unwrap(),
        "--",
        "/bin/true",
    ];
    let output = Command::new(hermit_binary)
        .env("TMPDIR", host_tmpdir.path())
        .env("HERMIT_SABRE_BINARY", &loader)
        .args(args)
        .output()
        .expect("failed to run SaBRe nested-TMPDIR regression");
    assert_success(&output, &args);

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&verify_report).expect("SaBRe nested-TMPDIR verification report was not written"),
    )
    .expect("SaBRe nested-TMPDIR verification report was not valid JSON");
    assert!(
        report["verified"] == true
            && report["bitwise_parity"] == true
            && report["verdict"] == "matched"
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["compare_logs"] == true
            && report["comparison"]["log_scope"] == "info",
        "SaBRe nested-TMPDIR run did not produce a canonical matched report:\n{report}"
    );
}

#[test]
fn global_position_rejects_unknown_backends() {
    let args = ["--backend", "unknown", "run", "--", "/bin/true"];
    let output = hermit(&args);
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("invalid value 'unknown'"),
        "unexpected error:\n{stderr}"
    );
}

#[test]
fn namespace_only_rejects_global_position_backend() {
    let args = [
        "--backend",
        "ptrace",
        "run",
        "--namespace-only",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);
    let message = stderr(&output);
    assert!(
        message.contains("--backend"),
        "unexpected error:\n{message}"
    );
    assert!(
        message.contains("--namespace-only"),
        "unexpected error:\n{message}"
    );
}

#[test]
fn incompatible_run_modes_fail_during_argument_parsing() {
    let args = ["run", "--namespace-only", "--chaos", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("--namespace-only"),
        "unexpected error:\n{stderr}"
    );
    assert!(stderr.contains("--chaos"), "unexpected error:\n{stderr}");
    assert!(
        stderr.contains("cannot be used with"),
        "unexpected error:\n{stderr}"
    );
}

#[test]
fn no_namespace_rejects_container_only_options() {
    let cases = [
        "--namespace-only",
        "--analyze-networking",
        "--mount=type=bind,source=/tmp,target=/tmp",
        "--bind=/tmp",
        "--network=local",
        "--network=host",
        "--tmp=/tmp/custom",
        "--replay-schedule-from=/tmp/schedule.json",
        "--replay-preemptions-from=/tmp/preemptions.json",
    ];

    for incompatible in cases {
        let args = ["run", "--no-namespace", incompatible, "/bin/true"];
        let output = hermit(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "hermit {args:?} unexpectedly ran"
        );

        let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
        assert!(
            stderr.contains("--no-namespace"),
            "unexpected error:\n{stderr}"
        );
        assert!(
            stderr.contains(incompatible.split_once("=").map_or(incompatible, |x| x.0)),
            "unexpected error:\n{stderr}"
        );
        assert!(
            stderr.contains("cannot be used with"),
            "unexpected error:\n{stderr}"
        );
    }
}

#[test]
fn no_namespace_runs_without_container_setup() {
    let _guard = hermit_run_guard();
    let args = [
        "run",
        "--no-namespace",
        "--max-timeslice=disabled",
        "--",
        "/bin/echo",
        "hello",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    assert_eq!(stdout(&output), "hello\n");
    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("WARNING: --no-namespace"),
        "unexpected stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("less deterministic"),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn no_namespace_preserves_affinity_for_run_and_verify() {
    let _guard = hermit_run_guard();

    let run_args = [
        "run",
        "--no-namespace",
        "--pin-threads",
        "--max-timeslice=disabled",
        "--",
        "/usr/bin/nproc",
    ];
    let output = hermit(&run_args);
    assert_success(&output, &run_args);
    assert_eq!(stdout(&output), "1\n");

    let verify_args = [
        "run",
        "--no-namespace",
        "--verify",
        "--pin-threads",
        "--max-timeslice=disabled",
        "--",
        "/usr/bin/nproc",
    ];
    let output = hermit(&verify_args);
    assert_success(&output, &verify_args);
    assert_eq!(stdout(&output), "1\n");
    assert!(
        stderr(&output).contains("Determinism verified"),
        "no-namespace verify did not complete:\n{}",
        stderr(&output),
    );
}

#[test]
fn no_namespace_fork_children_have_deterministic_distinct_rng_streams() {
    let _guard = hermit_run_guard();
    let guest = fork_child_getrandom_guest()
        .to_str()
        .expect("guest path should be UTF-8");
    let args = [
        "run",
        "--no-namespace",
        "--verify",
        "--pin-threads",
        "--max-timeslice=disabled",
        "--",
        guest,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("Determinism verified"),
        "missing verification success marker:\n{stderr}"
    );
}

#[test]
fn record_list_json_reports_an_empty_inventory() {
    let data_dir = tempfile::tempdir().expect("failed to create recording data directory");
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "list", "--json", "--data-dir"])
        .arg(data_dir.path())
        .output()
        .expect("failed to run hermit record list");
    assert!(
        output.status.success(),
        "hermit record list failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("record list should emit JSON");
    assert_eq!(value, serde_json::json!([]));
}

#[test]
fn record_list_rejects_a_non_directory_inventory() {
    let parent = tempfile::tempdir().expect("failed to create recording data parent");
    let data_file = parent.path().join("not-a-directory");
    fs::write(&data_file, b"not a recording inventory")
        .expect("failed to create non-directory data path");

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "list", "--json", "--data-dir"])
        .arg(&data_file)
        .output()
        .expect("failed to run hermit record list");
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["Failed to read recording inventory", "not-a-directory"],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "clean", "--data-dir"])
        .arg(&data_file)
        .output()
        .expect("failed to run hermit record clean");
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["Failed to read recording inventory", "not-a-directory"],
    );
    assert_eq!(
        fs::read(&data_file).expect("record clean removed the data path"),
        b"not a recording inventory"
    );
}

#[test]
fn run_rejects_invalid_programs_with_actionable_errors() {
    let output = hermit(&["run", "--", "/definitely/missing/hermit-program"]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::GuestNotFound,
        &["does not exist or is not accessible", "Check the path"],
    );

    let output = hermit(&["run", "--", "definitely-missing-hermit-program"]);
    assert_hermit_refusal_contains(
        &output,
        // ⚠️ GuestNotFound, THE SAME AS THE ABSOLUTE-PATH ARM ABOVE, AND THAT IS
        // THE POINT OF THIS ASSERTION. An earlier version of this comment
        // recorded the opposite as a filed inconsistency: a bare name that would
        // not resolve on the guest PATH exited 125 with `class=cli-error` while
        // `/nope/x` exited 127. Same condition, and the only difference was how
        // the caller SPELLED it -- a property of the command line, not of the
        // failure.
        //
        // ⚠️ AND THE SPLIT WAS BACKWARDS WITH RESPECT TO ITS OWN CONVENTION. The
        // scheme is borrowed from GNU `env`/`chroot`/`timeout`, where 127 is
        // PRIMARILY the PATH-lookup failure -- "command not found" is the shell
        // failing to resolve a bare name. The branch getting the non-PATH code
        // was the one the code was written for.
        Refusal::GuestNotFound,
        &["Could not resolve program", "guest PATH"],
    );

    let temp = tempfile::tempdir().expect("failed to create program fixture directory");
    let non_executable = temp.path().join("non-executable");
    fs::write(&non_executable, "#!/bin/sh\nexit 0\n").expect("failed to write program fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--tmp=/tmp", "--"])
        .arg(&non_executable)
        .output()
        .expect("failed to run hermit");
    assert_hermit_refusal_contains(
        &output,
        Refusal::GuestNotExecutable,
        &["is not executable", "chmod +x"],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--tmp=/tmp", "--"])
        .arg(temp.path())
        .output()
        .expect("failed to run hermit");
    assert_hermit_refusal_contains(
        &output,
        Refusal::GuestNotExecutable,
        &["is a directory", "executable file"],
    );

    let bad_shebang = temp.path().join("bad-shebang");
    fs::write(&bad_shebang, "#!/definitely/missing/interpreter\n").expect("failed to write script");
    let mut permissions = fs::metadata(&bad_shebang)
        .expect("failed to stat script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&bad_shebang, permissions).expect("failed to make script executable");

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--tmp=/tmp", "--"])
        .arg(&bad_shebang)
        .output()
        .expect("failed to run hermit");
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["uses shebang interpreter", "does not exist", "#! line"],
    );
}

#[test]
fn run_rejects_invalid_configuration_without_panicking() {
    let output = hermit(&["run", "--no-virtualize-time", "--", "/bin/true"]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["also requires --no-virtualize-metadata", "timestamps"],
    );

    let output = hermit(&["run", "--sched-sticky-random-param=-0.1", "--", "/bin/true"]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["must be between 0 and 1", "received -0.1"],
    );
}

#[test]
fn run_rejects_a_missing_bind_source_before_mounting() {
    let output = hermit(&[
        "run",
        "--bind=/definitely/missing/hermit-test:/tmp/input",
        "--",
        "/bin/true",
    ]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["--bind source", "does not exist", "correct"],
    );

    let output = hermit(&[
        "run",
        "--mount=type=bind,source=/definitely/missing/hermit-test,target=/tmp/input",
        "--",
        "/bin/true",
    ]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["--mount source", "does not exist", "correct"],
    );
}

#[test]
fn run_reports_denied_ptrace_and_seccomp_capabilities() {
    for (syscall, expected) in [
        (
            libc::SYS_ptrace,
            ["cannot use ptrace", "PTRACE_TRACEME", "--namespace-only"],
        ),
        (
            libc::SYS_seccomp,
            [
                "cannot install",
                "SECCOMP_SET_MODE_FILTER",
                "--namespace-only",
            ],
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command.args([
            "run",
            "--max-timeslice=disabled",
            "--no-virtualize-cpuid",
            "--",
            "/bin/true",
        ]);
        deny_syscall(&mut command, syscall);
        let output = command.output().expect("failed to run restricted hermit");
        assert_hermit_refusal_contains(&output, Refusal::Hermit, &expected);
    }
}

/// The container's virtual clock must keep advancing across `execve`.
///
/// `execve` replaces the process image, not the container, so a guest must
/// never see time restart at the configured epoch after an exec. This is the
/// property hermit#705 was ultimately about: showing the guest a *plausible*
/// epoch is not clock virtualization if the clock rewinds at every image
/// boundary. The guest itself samples the whole trajectory (repeated reads
/// before and after the exec) rather than a single value, because first-sample
/// agreement on a tidy origin is the classic false green here.
///
/// ptrace is the golden reference and keeps one container-wide `GlobalTime`
/// out-of-process, so this is a regression guard for that reference.
#[test]
fn run_ptrace_virtual_clock_advances_across_execve() {
    let program = exec_clock_continuity_guest()
        .to_str()
        .expect("exec-clock-continuity guest path should be UTF-8");
    let args = [
        "run",
        "--strict",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
        "--",
        program,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "exec-clock-continuity-ok\n");
}

/// The same exec-boundary clock trajectory must also be reproducible, so the
/// continuity above cannot be bought with a nondeterministic clock.
#[test]
fn run_ptrace_virtual_clock_across_execve_is_deterministic() {
    let program = exec_clock_continuity_guest()
        .to_str()
        .expect("exec-clock-continuity guest path should be UTF-8");
    let args = [
        "run",
        "--strict",
        "--verify",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
        "--",
        program,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert!(
        stderr(&output).contains("deterministic"),
        "expected a determinism verdict from --verify:\n{}",
        stderr(&output),
    );
}

/// `--log-file` must resolve on the HOST, exactly like a shell redirect.
///
/// The container mounts a fresh writable /tmp over its root, and tracing is
/// initialized inside the container. Opening the log there resolved `/tmp/x.log`
/// against the GUEST's tmpfs, where the create SUCCEEDED and the file then died with
/// the namespace: exit 0, no log, no warning. A user who asked for a log got success
/// and nothing, which is indistinguishable from a log that was legitimately empty.
/// Measured 2026-08-20; a debugging session was lost to it.
///
/// /tmp is the case that matters because it is both the natural place to put a
/// scratch log and the one directory the container replaces.
#[test]
fn log_file_under_tmp_lands_on_the_host() {
    let directory = tempfile::Builder::new()
        .prefix("hermit_log_file_host_ns_")
        .tempdir_in("/tmp")
        .unwrap();
    let log = directory.path().join("guest.log");

    let output = hermit(&[
        "--log=info",
        "--log-file",
        log.to_str().unwrap(),
        "run",
        "--",
        "/bin/true",
    ]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "unexpected status: {output:?}"
    );
    assert!(
        log.exists(),
        "--log-file under /tmp produced no host file: {output:?}"
    );
    let size = std::fs::metadata(&log).unwrap().len();
    // Non-empty, not merely created: an empty file is the symptom this fixes.
    assert!(
        size > 0,
        "--log-file under /tmp produced an empty host file"
    );
}

/// A log destination that cannot be opened must say so and fail, never exit 0
/// having written nothing. This half needs no policy ruling: silent success is
/// never the right answer to "write my diagnostics here".
#[test]
fn log_file_that_cannot_be_opened_is_refused_by_path() {
    let output = hermit(&[
        "--log=info",
        "--log-file",
        "/nonexistent-root-dir-for-hermit-test/guest.log",
        "run",
        "--",
        "/bin/true",
    ]);

    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &[
            "cannot open --log-file",
            "/nonexistent-root-dir-for-hermit-test/guest.log",
        ],
    );
}
#[test]
fn hermit_dap_forwards_remote_settings_to_gdb() {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-dap"))
        .args(["--gdb", "/bin/echo"])
        .output()
        .expect("failed to run hermit-dap");

    assert!(
        output.status.success(),
        "hermit-dap failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "--quiet --nx --init-eval-command=set debuginfod enabled off \
         --init-eval-command=set sysroot / --interpreter=dap\n"
    );
}

#[test]
fn hermit_dap_reports_a_missing_gdb_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-dap"))
        .arg("--gdb")
        .output()
        .expect("failed to run hermit-dap");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--gdb requires a path"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn hermit_dap_help_describes_managed_replay() {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-dap"))
        .arg("--help")
        .output()
        .expect("failed to run hermit-dap");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--replay ID"), "stdout:\n{stdout}");
    assert!(stdout.contains("stepBack"), "stdout:\n{stdout}");
    assert!(stdout.contains("reverseContinue"), "stdout:\n{stdout}");
}

#[test]
fn hermit_dap_rejects_replay_options_without_replay() {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-dap"))
        .args(["--data-dir", "/tmp/recordings"])
        .output()
        .expect("failed to run hermit-dap");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--data-dir requires --replay"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A tracer panic and a guest that exited 1 must be distinguishable from `$?`
/// ALONE, with no stderr parsing.
///
/// ⚠️ THIS IS THE WHOLE POINT AND IT IS EASY TO SATISFY BY ACCIDENT. Asserting
/// only that the panic arm is 125 would still pass if hermit returned 125 for
/// everything, so both arms are asserted together and the test is named for the
/// DIFFERENCE rather than for either value.
///
/// Measured before the fix: both arms returned 1, with the `HERMIT_TASK_PANIC`
/// marker present on stderr in the panic arm. The information existed and only
/// `$?` could not carry it -- every harness and gate on this project decides
/// pass/fail from exactly that value.
///
/// The panic is induced with reverie's own fault injector rather than a mock, so
/// this exercises the real task-boundary path: the guest thread panics, reverie
/// emits the marker and exits 101 inside the sandbox container, and hermit's CLI
/// error arm is what turns that into a status.
#[test]
fn tracer_panic_and_guest_failure_have_different_exit_codes() {
    let _guard = hermit_run_guard();

    // A guest with enough retired conditional branches to reach the timer path;
    // a trivial guest exits before the injected zero skid margin can bite.
    let busy = "awk 'BEGIN{s=0;for(i=0;i<300000;i++)s+=i;print s}'";
    let panicked = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--", "/bin/sh", "-c", busy])
        .env("REVERIE_SKID_MARGIN_OVERRIDE", "0")
        .output()
        .expect("failed to run hermit under the skid injector");

    let stderr = String::from_utf8_lossy(&panicked.stderr);
    // If the injector stopped inducing a panic this test would silently become a
    // comparison of two ordinary runs, so require the panic actually happened.
    assert!(
        stderr.contains("HERMIT_TASK_PANIC") || stderr.contains("panicked at"),
        "the skid injector did not induce a tracer panic; this test is measuring nothing:\n{stderr}"
    );

    let guest_failed = hermit(&["run", "--", "/bin/sh", "-c", "exit 1"]);

    let panic_code = panicked.status.code();
    let guest_code = guest_failed.status.code();
    assert_ne!(
        panic_code, guest_code,
        "a tracer panic and a guest exiting 1 are indistinguishable from $? alone \
         (both {panic_code:?}); every gate reading the exit code cannot tell a crash \
         from a failure"
    );
    // Deliberately a literal `1`: this is the GUEST's own chosen status passing
    // through, not hermit's reserved code, so it must NOT track
    // `HERMIT_INTERNAL_FAILURE_EXIT`. If that constant ever became 1 this
    // assertion pair should start failing, and substituting the constant here
    // would hide exactly that.
    assert_eq!(
        guest_code,
        Some(1),
        "the guest's own exit status must pass through unchanged"
    );
    assert_eq!(
        panic_code,
        Some(HERMIT_INTERNAL_FAILURE_EXIT),
        "hermit-internal failure should use the reserved wrapper code"
    );
}

/// A container child that exits with a status IT DID NOT CHOOSE must be
/// distinguishable from an ordinary CLI error, and neither may be confused with
/// the guest's own exit.
///
/// ⚠️ MEASURED BEFORE THE FIX, at main `b92c2227fc`: all three arms returned 1
/// and emitted NO classification at all, so `$?` and stderr agreed on nothing.
/// The information was never missing — reverie hands hermit a typed
/// `RunError::ExitStatus`, and `with_container` discarded it with
/// `.context(..)?`. This test fails if that discard comes back.
///
/// It deliberately asserts on the TYPED classification rather than on an exit
/// code: every value in `0..=255` is a legal guest status, so no exit code can
/// separate these classes without colliding with some guest.
#[test]
fn container_child_exit_is_distinguishable_from_an_ordinary_cli_error() {
    // (a) The container child dies of a fault `catch_unwind` cannot intercept,
    // so reverie reports a real typed status rather than a reported error.
    let child_exit = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "segv")
        .args(["run", "--strict", "--", "/bin/true"])
        .output()
        .expect("failed to run the fault-injected container child");
    let child_exit_stderr = String::from_utf8_lossy(&child_exit.stderr).into_owned();

    // (c) An ordinary CLI failure: hermit cannot open the requested log file.
    let cli_error = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log-file",
            "/nonexistent-directory-for-hermit-cli-test/log",
            "run",
            "--strict",
            "--",
            "/bin/true",
        ])
        .output()
        .expect("failed to run the unwritable-log-path case");
    let cli_error_stderr = String::from_utf8_lossy(&cli_error.stderr).into_owned();

    assert!(
        child_exit_stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-exit"),
        "a container child that exited with an unchosen status must be classified as \
         such\nstderr:\n{child_exit_stderr}"
    );
    assert!(
        cli_error_stderr.contains("HERMIT_INTERNAL_FAILURE class=cli-error"),
        "an ordinary CLI error must be classified as such\nstderr:\n{cli_error_stderr}"
    );
    // The whole point: the two must not read the same.
    assert!(
        !cli_error_stderr.contains("class=container-child-exit"),
        "an ordinary CLI error must NOT be reported as a container child exit\nstderr:\n\
         {cli_error_stderr}"
    );

    // The typed status must survive, not just the class. This is what
    // `.context(..)?` used to destroy.
    assert!(
        child_exit_stderr.contains("status=Signaled(SIGSEGV"),
        "the child's typed status must survive to the classification\nstderr:\n\
         {child_exit_stderr}"
    );

    // (a2) A container-child panic that IS caught still reports the tracer
    // breaking, not the CLI refusing. This is the third flattening: the panic
    // crosses a process boundary through `SerializableError`, which carries
    // only strings, so without the `kind` discriminant it arrives
    // indistinguishable from an ordinary reported error.
    let child_panic = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "panic")
        .args(["run", "--strict", "--", "/bin/true"])
        .output()
        .expect("failed to run the panic-injected container child");
    let child_panic_stderr = String::from_utf8_lossy(&child_panic.stderr).into_owned();
    assert!(
        child_panic_stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-panic"),
        "a CAUGHT container-child panic must be classified as a panic, not as a CLI          error
stderr:
{child_panic_stderr}"
    );
    assert!(
        !child_panic_stderr.contains("class=cli-error"),
        "a caught container-child panic must NOT read as an ordinary CLI          error
stderr:
{child_panic_stderr}"
    );

    // (b) The guest's own exit is NOT an internal failure and must carry no
    // marker at all — "hermit's exit IS the guest's exit" stays intact.
    let guest_exit = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--strict", "--", "/bin/false"])
        .output()
        .expect("failed to run the guest-exit case");
    assert_eq!(
        guest_exit.status.code(),
        Some(1),
        "a guest exiting 1 must still surface as 1"
    );
    assert!(
        !String::from_utf8_lossy(&guest_exit.stderr).contains("HERMIT_INTERNAL_FAILURE"),
        "the guest's own exit must not be classified as a hermit-internal failure"
    );
}

/// A guest must not be able to escape the deterministic pipe-capacity pin, and
/// must not be able to read the host's ceiling.
///
/// ⚠️ THIS IS THE WIRING TEST, AND IT EXISTS BECAUSE THE UNIT TESTS DO NOT COVER
/// IT. `pipe_capacity_request` is unit-tested in `detcore`, but deleting the
/// `F_SETPIPE_SZ` arm from `handle_fcntl` entirely leaves every one of those
/// unit tests green while restoring the escape in full — measured. Only an
/// end-to-end run catches that, so this asserts through a real guest.
///
/// ⚠️ AND IT IS A DETERMINISM TEST, NOT A POLICY PREFERENCE. Before the fix the
/// request was forwarded to the host, so the guest-visible answer was decided by
/// `/proc/sys/fs/pipe-max-size`: on this host (ceiling 1048576) the guest got
/// success and a 1 MiB pipe; on a host with the common hardened 65536 the same
/// guest got EPERM and kept 8192. Same binary, same `--strict`, different answer
/// per host.
#[test]
fn a_guest_cannot_escape_the_deterministic_pipe_capacity_pin() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pipe-capacity-pin");
    fs::create_dir_all(&build_root).expect("failed to create the pipe-capacity build root");
    let source = build_root.join("pipecap.c");
    fs::write(
        &source,
        r#"
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
int main(void) {
    int fd[2];
    if (pipe(fd)) return 1;
    printf("initial=%d\n", fcntl(fd[0], F_GETPIPE_SZ));
    printf("grow=%d\n", fcntl(fd[0], F_SETPIPE_SZ, 1 << 20));
    printf("after=%d\n", fcntl(fd[0], F_GETPIPE_SZ));
    char buf[64] = {0};
    FILE *f = fopen("/proc/sys/fs/pipe-max-size", "r");
    if (f && fgets(buf, sizeof buf, f)) printf("ceiling=%s", buf);
    if (f) fclose(f);
    return 0;
}
"#,
    )
    .expect("failed to write the pipe-capacity guest");
    let guest = build_root.join("pipecap");
    let built = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&guest)
        .arg(&source)
        .output()
        .expect("failed to invoke cc for the pipe-capacity guest");
    assert!(
        built.status.success(),
        "failed to build the pipe-capacity guest:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--strict", "--"])
        .arg(&guest)
        .output()
        .expect("failed to run the pipe-capacity guest under hermit");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert!(
        stdout.contains("initial=8192"),
        "the creation-time pin must still apply\nstdout:\n{stdout}"
    );
    // -1 is EPERM: exactly what Linux returns when the request exceeds the
    // ceiling, except the ceiling is now one Detcore owns.
    assert!(
        stdout.contains("grow=-1"),
        "a guest must not be able to grow a pipe past the pinned capacity\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("after=8192"),
        "the pinned capacity must survive the guest's own F_SETPIPE_SZ\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ceiling=8192"),
        "the guest must read the enforced ceiling, not the host's\nstdout:\n{stdout}"
    );
}

/// A guest-side fact must not be reported as a hermit-internal failure — and a
/// genuine hermit-internal failure must keep saying so.
///
/// ⚠️ BOTH ARMS ARE PINNED DELIBERATELY. A change that returned 127 for
/// everything would be WORSE than the behaviour it replaces: today the two are
/// equally wrong, and that change would make the common case (a typo in a guest
/// path) silently claim the rarer one. A test asserting only the new code would
/// pass on exactly that broken change, so the 125 arm has to be load-bearing --
/// which means it has to reach `failure_exit_code`, and as first written it did
/// not. See the comment on that arm below for the measurement.
///
/// Measured before the fix, at main `b97a4bc3a4`: `/no/such/program` and an
/// unwritable `--log-file` both returned 125 with the same class.
#[test]
fn a_guest_side_fault_is_not_reported_as_a_hermit_internal_failure() {
    let missing = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--strict",
            "--",
            "/no/such/program-for-hermit-cli-test",
        ])
        .output()
        .expect("failed to run the missing-program case");
    let missing_stderr = String::from_utf8_lossy(&missing.stderr).into_owned();
    assert_eq!(
        missing.status.code(),
        Some(127),
        "a missing program is command-not-found, the GNU convention 125 came from\nstderr:\n{missing_stderr}"
    );
    assert!(
        missing_stderr.contains("class=guest-program-not-found"),
        "the class must name a guest-side fault\nstderr:\n{missing_stderr}"
    );

    // Present but not executable: 126, distinct from both 127 and 125.
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("guest-fault-not-executable");
    fs::create_dir_all(&build_root).expect("failed to create the not-executable build root");
    let unexecutable = build_root.join("not-executable");
    fs::write(&unexecutable, b"\x7fELF not really\n").expect("failed to write the file");
    fs::set_permissions(&unexecutable, fs::Permissions::from_mode(0o644))
        .expect("failed to drop the execute bit");
    let denied = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--strict", "--"])
        .arg(&unexecutable)
        .output()
        .expect("failed to run the not-executable case");
    let denied_stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
    assert_eq!(
        denied.status.code(),
        Some(126),
        "found-but-not-executable is 126, not 127\nstderr:\n{denied_stderr}"
    );

    // ⚠️ THE ARM THAT STOPS THIS BECOMING A BLANKET 127, AND IT HAS TO REACH THE
    // MAPPING TO BE THAT ARM. An injected container-child fault is a genuine
    // hermit-internal failure that travels the ordinary route: `main` -> `Err` ->
    // `failure_exit_code`, which is the function a blanket 127 would live in.
    //
    // ⚠️ IT WAS WRITTEN WITH AN UNWRITABLE `--log-file` AND THAT DID NOT WORK.
    // `--log-file` fails in `open_log_file`, BEFORE the command is dispatched, and
    // that path raises the 125 constant directly without consulting
    // `failure_exit_code` at all. Measured on this branch: with `None => 127`
    // substituted for `None => HERMIT_INTERNAL_FAILURE_EXIT`, the test as
    // originally written still reported `1 passed` -- the arm named for the
    // mutation did not see it. Through the fault-injected route the same mutation
    // fails here.
    let internal = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "segv")
        .args(["run", "--strict", "--", "/bin/true"])
        .output()
        .expect("failed to run the hermit-internal case");
    let internal_stderr = String::from_utf8_lossy(&internal.stderr).into_owned();
    assert_eq!(
        internal.status.code(),
        Some(125),
        "a hermit-internal failure must stay 125\nstderr:\n{internal_stderr}"
    );
    assert!(
        internal_stderr.contains("class=container-child-exit"),
        "this arm is only load-bearing if it goes through the mapping, which needs a \
         failure that reaches it\nstderr:\n{internal_stderr}"
    );
    assert!(
        !internal_stderr.contains("guest-program"),
        "a hermit-internal failure must not be classed as guest-side\nstderr:\n{internal_stderr}"
    );

    // The pre-dispatch path keeps its own answer, which is a SEPARATE fact: an
    // unwritable `--log-file` never reaches `failure_exit_code`, so this pins the
    // constant in `main` rather than the mapping. Kept, and no longer described as
    // the arm that guards the mapping.
    let pre_dispatch = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log-file",
            "/nonexistent-directory-for-hermit-cli-test/log",
            "run",
            "--strict",
            "--",
            "/bin/true",
        ])
        .output()
        .expect("failed to run the unwritable-log-file case");
    let pre_dispatch_stderr = String::from_utf8_lossy(&pre_dispatch.stderr).into_owned();
    assert_eq!(
        pre_dispatch.status.code(),
        Some(125),
        "a failure before dispatch must still be 125\nstderr:\n{pre_dispatch_stderr}"
    );
    // ⚠️ THE NEGATIVE CLASS ASSERTION BELONGS ON THIS PATH TOO, NOT ONLY ON THE
    // injected-fault arm above. The exit code alone does not distinguish a
    // pre-dispatch failure from a guest-side one: both can be 125 while the
    // classification is wrong. This arm previously carried it, and it was lost
    // when the assertion migrated to the injected-segv invocation -- the text
    // survived branch-wide while its coverage of THIS path did not.
    assert!(
        !pre_dispatch_stderr.contains("guest-program"),
        "a pre-dispatch failure must not be classified guest-side\nstderr:\n{pre_dispatch_stderr}"
    );

    // And the guest's own exit is untouched by any of this.
    let guest = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--strict", "--", "/bin/false"])
        .output()
        .expect("failed to run the guest-exit case");
    assert_eq!(
        guest.status.code(),
        Some(1),
        "a guest exiting 1 still exits 1"
    );
}

/// The REPLAY-stage classification site of `record --verify-with-gdbex`, the
/// sixth and last of them.
///
/// ⚠️ THIS SITE WAS TWICE DECLARED UNTESTABLE BY THIS BRANCH AND IS NEITHER.
/// First the claim was that no fault could reach a replay stage at all; then,
/// after that was disproved, that reaching THIS one wedges the process. Both
/// were wrong, and `agent(codex-rev-2628)` supplied the missing step each time.
/// THE HANG THAT BLOCKED THIS TEST WAS THE PROBE'S: killing the replay gdbserver
/// while leaving GDB ALIVE leaves GDB holding the captured stderr pipe open, so
/// the reader never sees EOF. Making GDB `quit` after the kill returns promptly
/// -- rc=125 in about a second.
///
/// ⚠️ THAT IS NOT THE SAME AS SAYING HERMIT HAS NO BUG HERE, AND AN EARLIER
/// VERSION OF THIS COMMENT SAID EXACTLY THAT. `agent(hermit-dbgrev7)` measured
/// the two cases apart: killing the replay CONTAINER CHILD makes hermit exit in
/// 0.050s, but killing GDB makes hermit NEVER RETURN -- 188s, killed by hand.
/// `--verify-with-gdbex` waits forever when GDB dies before completing the
/// connection, and `--record-timeout` arms the recording only. That defect is
/// real, is filed, and is out of scope for a test-only change. Correcting the
/// CAUSE of a hang is not the same as retracting the hang.
///
/// ⚠️ SO `--verify-with-gdbex` IS ITSELF THE CONTROL HOOK. The CLI already
/// accepts GDB commands, GDB runs while the replay container is alive, and GDB
/// can run Python. No production change, no new injector, and no external
/// process supervision: the kill is issued from inside the run being tested and
/// the same `-ex` sequence then shuts GDB down.
#[test]
fn record_classifies_a_gdbserver_replay_stage_container_child_failure() {
    let data_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create a recording dir");
    let script = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kill-gdbserver-replay-peer.py");
    // Finds the OUTER `hermit record` by walking GDB's own ancestry, then kills
    // that process's non-ancestor children -- the replay container. Written from
    // inside the run rather than supervised from outside, so the test does not
    // have to guess a pid or race the fork.
    fs::write(
        &script,
        r#"import os, signal

def parent_of(pid):
    try:
        return int(open("/proc/%d/stat" % pid).read().rsplit(")", 1)[-1].split()[1])
    except (OSError, IndexError, ValueError):
        return None

def cmdline(pid):
    try:
        with open("/proc/%d/cmdline" % pid, "rb") as handle:
            return handle.read().replace(b"\0", b" ").decode("utf-8", "replace")
    except OSError:
        return ""

ancestors = []
current = os.getpid()
for _ in range(30):
    ancestors.append(current)
    current = parent_of(current)
    if current is None or current <= 1:
        break

outer = None
for candidate in ancestors:
    text = cmdline(candidate)
    if "hermit" in text and " record " in text:
        outer = candidate
        break

if outer is not None:
    known = set(ancestors)
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        if pid in known:
            continue
        if parent_of(pid) == outer:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
"#,
    )
    .expect("failed to write the gdb kill script");

    let mut child = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_DATA_DIR", data_dir.path())
        .args([
            "record",
            "--verify-with-gdbex",
            // `;` is the -ex delimiter: kill the replay container, then shut GDB
            // down. ⚠️ THE `quit` IS LOAD-BEARING -- without it GDB stays alive
            // holding this test's stderr pipe and the run never appears to end.
            &format!("pi exec(open(\"{}\").read());quit", script.display()),
            "--",
            "/bin/true",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the gdbserver verify");

    // ⚠️ A DEADLINE, SO A HANG IS A FAILURE RATHER THAN A STUCK SUITE. This
    // drives hermit into an error path on purpose and an earlier version of the
    // probe really did wedge; a test that can stall CI is not an acceptable
    // price for a covered call site.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut timed_out = false;
    loop {
        match child.try_wait().expect("failed to poll hermit") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
    let output = child
        .wait_with_output()
        .expect("failed to collect the gdbserver verify output");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !timed_out,
        "hermit did not exit within 120s after its gdbserver replay container was killed\n\
         stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-exit"),
        "a gdbserver-replay container child killed by a signal it did not choose must be \
         classified as a container-child exit, not as a CLI error\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("status=Signaled(SIGKILL"),
        "the typed status must survive to the classification\nstderr:\n{stderr}"
    );
}

/// The REPLAY-stage classification site of `record --verify`, which the fault
/// injector cannot reach and which was therefore left unpinned.
///
/// ⚠️ THIS EXISTS BECAUSE "UNREACHABLE" WAS WRONG. The sibling test below covers
/// four of `record`'s six `run_guarded` sites, and its doc comment used to
/// assert that the remaining two -- the replay stages of `--verify` and
/// `--verify-with-gdbex` -- could not be reached without teaching
/// `inject_test_fault` to name a stage. `agent(codex-rev-2628)` disproved that
/// in review, and the idea is the one this file was missing: THE FAULT DOES NOT
/// HAVE TO COME FROM INSIDE THE CHILD. Waiting for `:: Replaying...` on stderr
/// and killing the container child from outside reaches the site, with no
/// change to production code and no new injector.
///
/// ⚠️ AND `SIGKILL` IS WHAT MAKES IT A CONTAINER-CHILD EXIT rather than a
/// reported error: it is a status the child did not choose and no handler can
/// intercept, so reverie hands hermit a typed `RunError::ExitStatus` -- exactly
/// the shape `.classified()` preserves and `.context(..)??` destroyed.
///
/// ⚠️ THE `--verify-with-gdbex` REPLAY SITE IS COVERED BY THE TEST ABOVE, AND AN
/// EARLIER VERSION OF THIS COMMENT BLAMED HERMIT FOR NOT COVERING IT. It said
/// hermit hangs when that replay container is killed, on the evidence of three
/// invocations that sat for 25 minutes and an in-suite probe that timed out at
/// 300s. The attribution was wrong twice over, and the second correction is the
/// one this comment used to get backwards:
///
///   1. Killing the container child does NOT hang hermit -- that path exits in
///      0.050s. What stalled the original probe was leaving GDB alive holding
///      the captured stderr pipe, so the reader never saw EOF. Issuing `quit`
///      after the kill returns in about a second, which is what the test above
///      does.
///
///   2. ⚠️ BUT HERMIT DOES HAVE A BUG HERE, AND THIS COMMENT USED TO SAY IT DOES
///      NOT. When GDB exits or dies WITHOUT completing its connection, the
///      container child blocks forever in an unbounded
///      `listener.accept().await` (`reverie-ptrace/src/gdbstub/server.rs`),
///      waiting for a client that is already gone. Nothing bounds it;
///      `--record-timeout` arms the recording only. Reproduced with no kill and
///      no signal, by putting a `gdb` on PATH that exits 0 without connecting.
///      Filed as `hermit_never_exits_when`.
///
/// ⚠️ THE TEST ABOVE CANNOT WITNESS THAT, AND MUST NOT BE READ AS EVIDENCE
/// AGAINST IT. It supplies `-ex quit`, so its GDB connects successfully and the
/// accept is satisfied long before the kill. It will keep passing after the hang
/// is fixed, which is correct for a coverage test and is exactly why the hang
/// needs its own.
#[test]
fn record_classifies_a_replay_stage_container_child_failure() {
    let data_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create a recording dir");

    // The direct children of a process, found by scanning `/proc/<pid>/stat`.
    //
    // ⚠️ NOT `/proc/<pid>/task/<tid>/children`, WHICH IS EMPTY HERE. That file is
    // the obvious answer and it is the wrong one: hermit's container child is put
    // in a NEW PID NAMESPACE, and the kernel omits such a child from the
    // `children` list of a reader in the parent namespace. Measured at the same
    // instant, with the child plainly alive: `children` across every task read
    // `[]` while a ppid scan found it. The first version of this test used
    // `children`, found nothing on five runs out of five, and reported "no
    // container child appeared" about a child that was there.
    fn children_of(pid: u32) -> Vec<u32> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let Some(candidate) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(format!("/proc/{candidate}/stat")) else {
                continue;
            };
            // `comm` is parenthesised and may itself contain spaces, so the
            // fields after it are taken from the LAST ')' rather than by
            // splitting the whole line. ppid is the second field after that.
            let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let (_state, parent) = (fields.next(), fields.next());
            if parent.and_then(|value| value.parse::<u32>().ok()) == Some(pid) {
                found.push(candidate);
            }
        }
        found
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_DATA_DIR", data_dir.path())
        // ⚠️ THE GUEST ARGUMENT BUYS NO HEADROOM AND AN EARLIER COMMENT HERE
        // CLAIMED IT DID. Guest time is virtualized, so `sleep 1`, `sleep 5` and
        // `sleep 60` all give the same ~0.4s window and `/bin/true` gives 0.2s --
        // measured by `agent(hermit-dbgrev7)`. What makes the kill land is the
        // poll loop below, which waits for the replay container to appear rather
        // than assuming it is already there; the guest is incidental.
        .args(["record", "--verify", "--", "/bin/sleep", "5"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the fault-injected verify");
    let pid = child.id();
    let stderr = child.stderr.take().expect("hermit stderr must be piped");

    // ⚠️ A DEADLINE, SO A HANG IS A FAILURE RATHER THAN A STUCK SUITE. This test
    // drives hermit into an error path on purpose, and hermit really does hang
    // on a neighbouring one: under `--verify-with-gdbex`, GDB dying before it
    // connects leaves hermit waiting forever. A test that can wedge CI is not an
    // acceptable price for a covered call site.
    let deadline = Instant::now() + Duration::from_secs(120);
    let captured = Arc::new(Mutex::new(String::new()));
    let reader_captured = Arc::clone(&captured);
    let reader = thread::spawn(move || {
        let mut killed = false;
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            {
                let mut buffer = reader_captured.lock().expect("stderr buffer poisoned");
                buffer.push_str(&line);
                buffer.push('\n');
            }
            if !killed && line.contains("Replaying...") {
                // The replay container is forked after the announcement, so poll
                // rather than assume it is already there.
                let until = Instant::now() + Duration::from_secs(10);
                while Instant::now() < until {
                    if let Some(target) = children_of(pid).first().copied() {
                        let _ = Command::new("kill")
                            .args(["-9", &target.to_string()])
                            .status();
                        killed = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
        killed
    });

    let mut timed_out = false;
    loop {
        match child.try_wait().expect("failed to poll hermit") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = child.wait();
    let killed = reader.join().unwrap_or(false);
    let stderr = captured.lock().expect("stderr buffer poisoned").clone();

    assert!(
        !timed_out,
        "hermit did not exit within 120s after its replay container child was killed; a \
         hanging error path is a defect, not a slow test\nstderr:\n{stderr}"
    );
    // ⚠️ REFUSES RATHER THAN PASSES ON AN UNFIRED PROBE. If the kill never
    // landed, this test proves nothing and must say so instead of going green.
    assert!(
        killed,
        "no container child was signalled after `Replaying...`, so this test never \
         exercised the replay stage it exists for\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-exit"),
        "a replay-stage container child killed by a signal it did not choose must be \
         classified as a container-child exit, not as a CLI error\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("status=Signaled(SIGKILL"),
        "the typed status must survive to the classification\nstderr:\n{stderr}"
    );
}

/// `hermit record` must classify a container-child failure the same way
/// `hermit run` does -- IN EVERY SPELLING THAT ENTERS A CONTAINER, not just the
/// bare one.
///
/// ⚠️ THIS IS THE CALL SITE THE FIX ORIGINALLY MISSED. `record` calls
/// `RunGuarded::run_guarded` directly at six sites in `record_start.rs` and
/// never goes through `with_container`, so the `.context(..)??` discard survived
/// there and BOTH container-child classes surfaced as `class=cli-error`. A wrong
/// machine-readable class is worse than none, because a gate believes it.
///
/// ⚠️ AND ONE SPELLING IS NOT SIX. The first version of this test drove only
/// `record -- prog`, which reaches exactly one of the six sites; adversarial
/// review pointed out that the other five could be reverted to the old
/// flattening with this test still green, so the "every spelling" claim in the
/// paragraph above was not the claim being tested. Four of the six are now
/// driven, one per entry point that can be reached with the guest's FIRST
/// container:
///
/// | spelling | site |
/// | --- | --- |
/// | `record -- prog` | `main`, no deadline |
/// | `record --record-timeout N -- prog` | `main`, deadline armed |
/// | `record --verify -- prog` | `record_verify`, record stage |
/// | `record --verify-with-gdbex ... -- prog` | `record_verify_debug`, record stage |
///
/// ⚠️ THE REMAINING TWO ARE THE REPLAY STAGES OF THOSE LAST TWO, AND THIS
/// INJECTOR CANNOT REACH THEM: `inject_test_fault` is a process-local
/// environment check with no notion of which stage it is in, so with the
/// variable set the RECORD stage faults first and the replay stage is never
/// entered.
///
/// ⚠️ THAT IS A LIMIT OF THIS INJECTOR AND NOT OF TESTING, AND AN EARLIER
/// VERSION OF THIS COMMENT SAID OTHERWISE. It claimed the two sites could not be
/// reached without a stage-aware injector, and `agent(codex-rev-2628)` disproved
/// it in review: the fault does not have to come from inside the child.
/// `record_classifies_a_replay_stage_container_child_failure` above pins `:535`
/// by killing the container child from outside after `:: Replaying...`, and
/// `record_classifies_a_gdbserver_replay_stage_container_child_failure` pins
/// `:660` by killing from inside GDB and then quitting it. All six sites are
/// covered; this test carries four of them.
#[test]
fn record_classifies_a_container_child_failure_the_same_way_run_does() {
    let data_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create a recording dir");
    let case = |fault: &str, extra: &[&str]| -> String {
        let mut args = vec!["record"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", "/bin/true"]);
        let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
            .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", fault)
            .env("HERMIT_DATA_DIR", data_dir.path())
            .args(&args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run {args:?}: {error}"));
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    // One entry point per row of the table above. Each is asserted for BOTH
    // classes, because the two travel different paths out of the child: an
    // unchosen exit status is observed by reverie, a caught panic is reported
    // through `SerializableError`'s discriminant.
    let spellings: [(&str, &[&str]); 4] = [
        ("bare", &[]),
        ("with a recording deadline", &["--record-timeout", "600"]),
        ("with --verify", &["--verify"]),
        ("with --verify-with-gdbex", &["--verify-with-gdbex", "quit"]),
    ];

    // ⚠️ EVERY SPELLING IS DRIVEN AND EVERY FAILURE IS COLLECTED, RATHER THAN
    // ASSERTED ONE AT A TIME. An `assert!` inside this loop aborts at the FIRST
    // failing spelling, so a run in which one call site regressed could never
    // show that the other three were unaffected -- and "each spelling reaches
    // one site and no other" is precisely the property this test is here to
    // carry. Collecting turns one revert into a one-line proof of the mapping:
    // exactly one spelling fails and the report names the three that did not.
    let mut failures: Vec<String> = Vec::new();
    for (name, extra) in spellings {
        let segv = case("segv", extra);
        if !segv.contains("HERMIT_INTERNAL_FAILURE class=container-child-exit") {
            failures.push(format!(
                "  [{name}] a record-path container child that exited with an unchosen \
                 status was not classified as such\n    stderr: {}",
                segv.replace('\n', " | ")
            ));
        }

        let panicked = case("panic", extra);
        if !panicked.contains("HERMIT_INTERNAL_FAILURE class=container-child-panic") {
            failures.push(format!(
                "  [{name}] a record-path CAUGHT container-child panic was not classified \
                 as a panic\n    stderr: {}",
                panicked.replace('\n', " | ")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} record spelling/fault combinations misclassified a container-child \
         failure. The combinations NOT listed here passed in this same run, which is what \
         makes this a mapping rather than an aggregate:\n{}",
        failures.len(),
        spellings.len() * 2,
        failures.join("\n")
    );
}

/// Every `record` container site classifies a child fault, addressed BY NAME.
///
/// ⚠️ WHY A NAME AND NOT AN OCCURRENCE INDEX. `record` enters a container at six
/// `run_guarded` sites. With only `HERMIT_TEST_CONTAINER_CHILD_FAULT` set, the
/// FIRST child to run faults and every later stage is never entered, so two of the
/// six -- the replay stages of `--verify` and `--verify-with-gdbex` -- could not be
/// reached by any test and their classification was asserted rather than measured.
/// An occurrence index would aim at them but is positional: it retargets silently
/// the moment a site is added, removed or reordered, and the test keeps passing
/// while pointing somewhere else. A process-local counter does not work at all,
/// because each `run_guarded` forks a fresh child and the injector runs in the
/// CHILD, so a static counter resets every time. The label is identity, which is
/// the thing whose absence made these sites untestable.
#[test]
fn every_record_container_site_classifies_a_child_fault_by_name() {
    let data_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("failed to create a data dir");

    let case = |site: &str, fault: &str, extra: &[&str]| -> String {
        let mut args = vec!["record"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", "/bin/true"]);
        let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
            .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", fault)
            .env("HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE", site)
            .env("HERMIT_DATA_DIR", data_dir.path())
            .args(&args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run {args:?} for site {site}: {error}"));
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    for (site, extra) in RECORD_FAULT_SITES {
        for (fault, class) in [
            ("segv", "container-child-exit"),
            ("panic", "container-child-panic"),
        ] {
            let stderr = case(site, fault, extra);
            assert!(
                stderr.contains(&format!("HERMIT_INTERNAL_FAILURE class={class}")),
                "site {site} under an injected {fault} must be classified as {class}, \
                 not folded into a CLI error\nstderr:\n{stderr}"
            );
        }
    }
}

/// The control WITHOUT WHICH THE TEST ABOVE PROVES NOTHING.
///
/// If the site filter were ignored -- the injector faulting on every container as it
/// does today -- every row above would still pass, because the first child would
/// fault and produce the expected class. Naming a site that does not exist must
/// therefore fault NOTHING: the recording completes and no internal-failure class is
/// printed. That is what distinguishes "the fault was aimed" from "the fault always
/// fires".
#[test]
fn a_fault_aimed_at_no_existing_site_fires_nowhere() {
    let data_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("failed to create a data dir");
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "segv")
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE", "no.such.site")
        .env("HERMIT_DATA_DIR", data_dir.path())
        .args(["record", "--", "/bin/true"])
        .output()
        .expect("failed to run the unaimed fault injection");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("HERMIT_INTERNAL_FAILURE"),
        "a fault aimed at a site that does not exist must fire nowhere; firing anyway \
         means the site filter is not consulted and the by-name test above is vacuous\
         \nstderr:\n{stderr}"
    );
}

/// One row per `run_guarded_at` site in `record_start.rs`, with a spelling that
/// reaches it. Kept at module scope so
/// `no_container_site_is_unreachable_by_the_fault_injector` can hold the source
/// to it.
const RECORD_FAULT_SITES: [(&str, &[&str]); 6] = [
    ("record.main", &[]),
    ("record.main.deadline", &["--record-timeout", "600"]),
    ("record_verify.record", &["--verify"]),
    ("record_verify.replay", &["--verify"]),
    (
        "record_verify_debug.record",
        &["--verify-with-gdbex", "quit"],
    ),
    (
        "record_verify_debug.replay",
        &["--verify-with-gdbex", "quit"],
    ),
];

/// Sites that exist but are deliberately not driven by the `record` table above,
/// each with the reason. An entry here is a DECLARATION, not an exemption from
/// thought: it says a different test already reaches this site.
const FAULT_SITES_DRIVEN_ELSEWHERE: [(&str, &str); 1] = [(
    "with_container",
    "the `run` path; covered by the existing run-mode fault-injection tests",
)];

/// ⚠️ A CLASSIFICATION SITE CANNOT SILENTLY OPT OUT OF BEING ADDRESSABLE.
///
/// This is the invariant made STRUCTURAL rather than left as a convention. Two
/// sites -- the replay stages -- were untestable for as long as they existed, and
/// nothing said so; they were discovered by a human reading the code. The failure
/// mode is not that a test broke, it is that no test could ever have existed, and
/// silence is indistinguishable from coverage.
///
/// So: every `run_guarded_at("...")` label in the sources is extracted here and
/// must appear either in [`RECORD_FAULT_SITES`] or in
/// [`FAULT_SITES_DRIVEN_ELSEWHERE`] with a reason. A site added without a row
/// fails THIS test by name, at the moment it is added, rather than being
/// untestable by default and noticed years later.
#[test]
fn no_container_site_is_unreachable_by_the_fault_injector() {
    const SOURCES: [(&str, &str); 2] = [
        (
            "record_start.rs",
            include_str!("../src/bin/hermit/record_start.rs"),
        ),
        (
            "container.rs",
            include_str!("../src/bin/hermit/container.rs"),
        ),
    ];

    let mut declared: Vec<(String, String)> = Vec::new();
    for (file, text) in SOURCES {
        for (needle, close) in [("run_guarded_at(\"", '"'), ("inject_test_fault(\"", '"')] {
            let mut rest = text;
            while let Some(at) = rest.find(needle) {
                rest = &rest[at + needle.len()..];
                if let Some(end) = rest.find(close) {
                    declared.push((file.to_string(), rest[..end].to_string()));
                }
            }
        }
    }
    assert!(
        !declared.is_empty(),
        "extracted zero site labels from the sources; this test would pass \
         vacuously and prove nothing about coverage"
    );

    let covered: Vec<&str> = RECORD_FAULT_SITES
        .iter()
        .map(|(site, _)| *site)
        .chain(FAULT_SITES_DRIVEN_ELSEWHERE.iter().map(|(site, _)| *site))
        .collect();

    let mut unreachable: Vec<String> = declared
        .iter()
        .filter(|(_, site)| !covered.contains(&site.as_str()))
        .map(|(file, site)| format!("{file}: {site}"))
        .collect();
    unreachable.sort();
    unreachable.dedup();
    assert!(
        unreachable.is_empty(),
        "these container sites can be faulted but no test aims at them, so their \
         classification would be asserted rather than measured -- add a row to \
         RECORD_FAULT_SITES, or to FAULT_SITES_DRIVEN_ELSEWHERE with the test that \
         covers it:\n  {}",
        unreachable.join("\n  ")
    );

    // And the other direction: a row naming a site that no longer exists is a test
    // aimed at nothing, which passes while covering less than it claims.
    let existing: Vec<&str> = declared.iter().map(|(_, s)| s.as_str()).collect();
    let stale: Vec<&str> = covered
        .iter()
        .copied()
        .filter(|site| !existing.contains(site))
        .collect();
    assert!(
        stale.is_empty(),
        "these rows name a site that no longer exists in the sources, so they aim \
         at nothing: {stale:?}"
    );
}
