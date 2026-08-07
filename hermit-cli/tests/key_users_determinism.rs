/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for live key quota accounting in /proc/key-users.

use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
const PROC_KEY_USERS: &str = "/proc/key-users";
const KEY_SPEC_SESSION_KEYRING: libc::c_long = -3;
const KEYCTL_UPDATE: libc::c_long = 2;
const KEYCTL_UNLINK: libc::c_long = 9;

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
}

struct KeyChurn {
    key: libc::c_long,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl KeyChurn {
    /// Start the churn, or fail the test.
    ///
    /// This deliberately does NOT return `Option` and does NOT skip. Without a
    /// live key the quota columns never move, so a "deterministic" reading of
    /// `/proc/key-users` proves nothing: the file would have been constant with
    /// or without Hermit. A run that cannot churn has produced a NO-RESULT, and
    /// reporting a no-result as a pass is what let this test report `ok` in
    /// 0.7s while exercising nothing on any host lacking keyring support.
    ///
    /// There is deliberately no environment opt-out, because an opt-out would
    /// just be the same silent pass under a new spelling. The adjacent
    /// `/proc/key-users` existence check already hard-fails; this now matches it.
    fn start() -> Self {
        let key_type = CString::new("user").unwrap();
        let description = CString::new(format!("hermit-key-users-{}", std::process::id())).unwrap();
        let payload = b"x";
        let key = unsafe {
            libc::syscall(
                libc::SYS_add_key,
                key_type.as_ptr(),
                description.as_ptr(),
                payload.as_ptr(),
                payload.len(),
                KEY_SPEC_SESSION_KEYRING,
            )
        };
        assert!(
            key >= 0,
            "add_key failed ({}); /proc/key-users cannot be churned, so this test \
             cannot establish determinism and must not report success. A host \
             running this lane needs kernel keyring support (CONFIG_KEYS).",
            std::io::Error::last_os_error()
        );

        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_failed = Arc::clone(&failed);
        let worker = thread::spawn(move || {
            let mut size = 1;
            while !worker_stop.load(Ordering::Relaxed) {
                let payload = vec![b'x'; size];
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_keyctl,
                        KEYCTL_UPDATE,
                        key,
                        payload.as_ptr(),
                        payload.len(),
                    )
                };
                if result < 0 {
                    worker_failed.store(true, Ordering::Relaxed);
                    break;
                }
                size = if size == 4_000 { 1 } else { size + 1 };
            }
        });

        Self {
            key,
            stop,
            failed,
            worker: Some(worker),
        }
    }

    fn assert_healthy(&self) {
        assert!(
            !self.failed.load(Ordering::Relaxed),
            "temporary key payload updates failed"
        );
    }
}

impl Drop for KeyChurn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("key churn worker panicked");
        }
        unsafe {
            libc::syscall(
                libc::SYS_keyctl,
                KEYCTL_UNLINK,
                self.key,
                KEY_SPEC_SESSION_KEYRING,
            );
        }
    }
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn required_program(case: &ProgramCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "required program {} is missing; expected one of {:?}",
                case.name, case.candidates
            )
        })
}

fn assert_churn_is_visible(churn: &KeyChurn) {
    let initial = fs::read(PROC_KEY_USERS).expect("failed to read /proc/key-users");
    for _ in 0..100 {
        thread::sleep(Duration::from_millis(2));
        churn.assert_healthy();
        let current = fs::read(PROC_KEY_USERS).expect("failed to reread /proc/key-users");
        if current != initial {
            return;
        }
    }
    panic!("temporary key payload churn did not change /proc/key-users");
}

fn assert_l2(case: &ProgramCase) {
    let program = required_program(case);
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            "DEBUG",
            "run",
            "--backend",
            "ptrace",
            "--strict",
            "--verify",
            "--verify-logs",
            "--panic-on-unsupported-syscalls",
            "--base-env",
            "minimal",
            "--",
        ])
        .arg(&program)
        .args(case.args);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{} failed strict verification ({rendered})\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
        output.status,
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{} omitted Hermit's verification marker ({rendered})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
    );
}

/// AUTONOMOUS-BOT-IMPLEMENTED
/// TODO-HUMAN-REVIEW(PR-951): Review /proc/key-users access-form mediation.
///
/// Review items 1 and 2 on PR #951 predicted that alias spellings,
/// dirfd-relative opens and positioned reads would bypass `/proc/key-users`
/// normalization. The systemic procfs work (issue #973) closed all of them
/// before this test existed; it is here so a regression is caught by CI rather
/// than rediscovered by review. Nine access forms, all compared against the
/// plain-`read` snapshot: seven must be mediated, two (`readv`, `preadv`) must
/// be refused with ENOSYS. The probe asserts the refusal rather than counting
/// it as mediation.
#[test]
fn key_users_access_forms_are_mediated_or_refused() {
    let _guard = hermit_run_lock();
    assert!(
        Path::new(PROC_KEY_USERS).is_file(),
        "{PROC_KEY_USERS} is required for the portable regression"
    );
    // Churn so the host file is genuinely moving underneath the guest; a stable
    // host file would let a bypass masquerade as mediation.
    let churn = KeyChurn::start();
    assert_churn_is_visible(&churn);

    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let source = repository.join("tests/c/key_users_access_forms_probe.c");
    let guest = Path::new(env!("CARGO_TARGET_TMPDIR")).join("key-users-access-forms-probe");
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile key-users access-form probe");
    assert!(
        compile.status.success(),
        "probe compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--backend=ptrace",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .output()
        .expect("failed to run key-users access-form probe");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success(),
        "strict run failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        run.status
    );
    // Each marker proves its group actually executed, so a probe that exited
    // early cannot be mistaken for a pass.
    for marker in [
        "key-users-positioned-mediated-ok",
        "key-users-vectored-refused-ok",
        "key-users-aliases-mediated-ok",
        "key-users-snapshot-stable-ok",
    ] {
        assert!(
            stdout.contains(marker),
            "probe omitted {marker}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    churn.assert_healthy();
}

#[test]
fn key_user_consumers_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    assert!(
        Path::new(PROC_KEY_USERS).is_file(),
        "{PROC_KEY_USERS} is required for the portable regression"
    );
    let churn = KeyChurn::start();
    assert_churn_is_visible(&churn);

    let cases = [
        ProgramCase {
            name: "cat",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &[PROC_KEY_USERS],
        },
        ProgramCase {
            name: "awk",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["{print $1, $2, $3, $4, $5}", PROC_KEY_USERS],
        },
        ProgramCase {
            name: "sed",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1,10p", PROC_KEY_USERS],
        },
    ];

    for case in &cases {
        assert_l2(case);
        churn.assert_healthy();
    }
}
