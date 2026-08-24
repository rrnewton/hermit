/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for `flock(2)` under Detcore (PR #2373).
//!
//! Before #2373 `handle_flock` was an unconditional no-op success, so two
//! guests held the same `LOCK_EX` at once. These tests pin down the properties
//! that fix depends on, each with a stated pre-fix control so none
//! of them can quietly go inert:
//!
//! | test | pre-fix behavior it stops |
//! | --- | --- |
//! | [`flock_excludes_a_second_open_and_a_second_process`] | the no-op: every `flock` returned 0 |
//! | [`contended_blocking_upgrade_is_refused_without_losing_the_shared_lock`] | the `LOCK_NB` probe destroyed the caller's `LOCK_SH` |
//! | [`contended_blocking_upgrade_is_fail_closed_under_strict`] | refusal policy ignored two of three config knobs |
//! | [`blocking_upgrade_on_received_fd_preserves_unknown_lock_state`] | the probe destroyed a lock received through `SCM_RIGHTS` |
//! | [`dbt_forked_child_blocking_flock_fails_closed_without_deadlock`] | a copied DBT child ran blocking flock natively and deadlocked |
//! | [`dbt_forked_child_preserves_safe_flock_operations`] | copied-child refusal overmatched malformed, nonblocking, and unlock operations |
//! | [`dbt_nested_vfork_child_blocking_flock_reaches_the_copied_policy`] | a copied fork child's vfork path was assumed to bypass the copied-syscall flock guard |
//! | [`failed_process_clone_preserves_known_flock_state`] | a failed clone made an unlocked descriptor permanently unknown |
//! | [`pidfd_getfd_alias_mutation_invalidates_source_flock_authority`] | a pidfd_getfd duplicate unlocked its source OFD while the source cache stayed stale and restored the released lock |
//! | [`pidfd_getfd_relaxed_mode_refuses_before_any_kernel_injection`] | the direct handler test passed while the raw dispatcher reordered the relaxed-mode guard |
//! | [`blocking_accept_allows_sibling_descriptor_mutations_to_unblock_it`] | a generic descriptor-table token made a parked accept reject the sibling open/dup/dup2/close sequence needed before connect |
//! | [`blocking_recvmsg_allows_sibling_descriptor_mutations_to_unblock_it`] | a generic descriptor-table token made parked recvmsg hold the table needed by its sending sibling |
//! | [`transferred_lock_state_is_unknown_to_the_sender`] | the sender restored stale state after the receiver unlocked the OFD |
//! | [`dbt_vfork_child_flock_fails_closed_without_deadlock`] | a root DBT vfork copied a child that blocked in the kernel while its parent was suspended |
//! | [`dbt_clone_vfork_forms_fail_closed_before_copy`] | clone/clone3 `CLONE_VFORK` spellings bypassed the root vfork guard |
//! | [`dbt_first_syscall_vfork_forms_fail_closed_before_runtime_initialization`] | a vfork-family first syscall bypassed the guard while runtime state was still null |
//! | [`dbt_process_clone_files_is_refused_before_copied_child_mutation`] | a copied DBT process shared the kernel descriptor table while mutating only a private model copy |
//! | [`dbt_vfork_with_inherited_stdio_fails_closed_promptly`] | ordinary DBT vfork was tested only after closing the unknown startup descriptors |
//! | [`dbt_vfork_without_flock_state_still_runs`] | the conservative guard accidentally disabled every root-process vfork |
//! | [`replay_reissues_every_flock_for_a_materialized_file`] | replay consumed the recorded return and took no lock |
//! | [`replay_reissues_pidfd_getfd_success_and_failure`] | raw pidfd_getfd results were not recorded, the flags-first EINVAL matrix was incomplete, and replay injected live without validating the OFD alias or errno |
//! | [`replay_refuses_flock_for_a_non_materialized_file`] | replay reported success while locking only a placeholder |
//! | [`pre_flock_recordings_are_refused_by_the_version_gate`] | a 0x10b recording has no flock event and desynchronized |

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

/// `hermit record` writes to shared per-run state; serialize the record/replay
/// cases the same way `record_replay.rs` does.
static RECORD_LOCK: Mutex<()> = Mutex::new(());
#[cfg(feature = "dbt")]
const DBT_VFORK_FLOCK_REFUSAL: &str =
    "detcore-dbt: refusing vfork/CLONE_VFORK while an open file description may hold a flock";
#[cfg(feature = "dbt")]
const DBT_PROCESS_CLONE_FILES_REFUSAL: &str =
    "detcore-dbt: refusing process clone with CLONE_FILES without CLONE_THREAD";

fn record_lock() -> MutexGuard<'static, ()> {
    RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// Compile `tests/c/flock_exclusion.c` once per test binary.
fn guest() -> &'static Path {
    static GUEST: OnceLock<PathBuf> = OnceLock::new();
    GUEST.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("flock-exclusion");
        fs::create_dir_all(&build_root).expect("failed to create the flock guest build directory");
        let binary = build_root.join("flock_exclusion");
        let source = repository().join("tests/c/flock_exclusion.c");
        let compile = Command::new("cc")
            .args(["-O1", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap_or_else(|error| panic!("failed to start cc for {}: {error}", source.display()));
        assert!(
            compile.status.success(),
            "failed to compile {}:\n{}",
            source.display(),
            String::from_utf8_lossy(&compile.stderr)
        );
        binary
    })
}

#[cfg(feature = "dbt")]
fn compile_first_syscall_vfork_guest(name: &str, form: u8) -> PathBuf {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("flock-first-syscall-vfork");
    fs::create_dir_all(&build_root)
        .expect("failed to create the first-syscall vfork guest build directory");
    let binary = build_root.join(name);
    let source = repository().join("tests/c/flock_exclusion.c");
    let compile = Command::new("cc")
        .args([
            "-O2",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-nostdlib",
            "-static",
            "-no-pie",
            "-fno-stack-protector",
            "-Wl,-e,_start",
        ])
        .arg(format!("-DHERMIT_DBT_FIRST_VFORK_FORM={form}"))
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "failed to start cc for first-syscall fixture {}: {error}",
                source.display()
            )
        });
    assert!(
        compile.status.success(),
        "failed to compile first-syscall fixture {}:\n{}",
        source.display(),
        String::from_utf8_lossy(&compile.stderr)
    );
    binary
}

#[cfg(feature = "dbt")]
fn first_syscall_vfork_guests() -> &'static [PathBuf; 3] {
    static GUESTS: OnceLock<[PathBuf; 3]> = OnceLock::new();
    GUESTS.get_or_init(|| {
        [
            compile_first_syscall_vfork_guest("first-vfork", 1),
            compile_first_syscall_vfork_guest("first-clone-vfork", 2),
            compile_first_syscall_vfork_guest("first-clone3-vfork", 3),
        ]
    })
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    fn combined(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }
}

fn finish(output: Output) -> Run {
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Run the guest under `hermit run`, with `extra` inserted before `--`.
///
/// `log` is the global `--log` level; `--verify` requires at least `info`, so
/// it cannot simply be pinned to `off`.
fn hermit_run(log: &str, extra: &[&str], scenario: &str) -> Run {
    hermit_run_backend("ptrace", log, extra, scenario)
}

fn hermit_run_backend(backend: &str, log: &str, extra: &[&str], scenario: &str) -> Run {
    hermit_run_backend_timeout(backend, log, extra, scenario, "120s")
}

fn hermit_run_backend_timeout(
    backend: &str,
    log: &str,
    extra: &[&str],
    scenario: &str,
    timeout: &str,
) -> Run {
    hermit_run_program_backend_timeout(backend, log, extra, guest(), &[scenario], timeout)
}

fn hermit_run_program_backend_timeout(
    backend: &str,
    log: &str,
    extra: &[&str],
    program: &Path,
    args: &[&str],
    timeout: &str,
) -> Run {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", timeout])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .arg(format!("--log={log}"))
        .args(["run", &format!("--backend={backend}"), "--base-env=minimal"])
        .args(extra)
        .arg("--")
        .arg(program)
        .args(args);
    finish(command.output().unwrap_or_else(|error| {
        panic!("failed to start hermit for {}: {error}", program.display())
    }))
}

fn injected_syscall_offset(stderr: &str, syscall: &str) -> Option<usize> {
    stderr.find(&format!("beginning inject of syscall: {syscall},"))
}

fn injected_syscall_offset_after(stderr: &str, syscall: &str, after: usize) -> Option<usize> {
    stderr[after..]
        .find(&format!("beginning inject of syscall: {syscall},"))
        .map(|offset| after + offset)
}

#[cfg(feature = "dbt")]
fn dbt_summary_counter(run: &Run, field: &str) -> u64 {
    let summary = run
        .stderr
        .lines()
        .rev()
        .find(|line| line.starts_with("reverie-dbt: tool=Detcore "))
        .unwrap_or_else(|| panic!("DBT summary missing\n{}", run.combined()));
    summary
        .split_ascii_whitespace()
        .find_map(|value| value.strip_prefix(field))
        .unwrap_or_else(|| panic!("DBT summary omitted {field}\n{}", run.combined()))
        .parse()
        .unwrap_or_else(|error| panic!("invalid DBT {field} counter: {error}\n{}", run.combined()))
}

/// Mutual exclusion, the property the pre-#2373 no-op removed outright: under
/// the no-op every `flock` returned 0, so both the second open file description
/// and the second process "acquired" a lock the first process was holding, and
/// the guest below printed `FAIL` on its very first contention probe. The
/// `--verify` pass additionally pins that forwarding to the kernel did not cost
/// determinism -- the exclusion outcome is fixed by Detcore's schedule, not by
/// which process happens to reach the kernel first.
#[test]
fn flock_excludes_a_second_open_and_a_second_process() {
    let run = hermit_run(
        "info",
        &["--strict", "--verify", "--panic-on-unsupported-syscalls"],
        "exclusion",
    );
    assert!(
        run.status.success(),
        "strict --verify exclusion run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-first-holder-acquired",
        "flock-second-open-excluded",
        "flock-second-process-excluded",
        "flock-released-and-reacquired",
        "flock-exclusion-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    assert!(
        run.stdout.contains("Determinism verified") || run.stderr.contains("Determinism verified"),
        "exclusion run did not verify as deterministic\n{}",
        run.combined()
    );
}

/// A contended BLOCKING `LOCK_SH` -> `LOCK_EX` conversion must be refused
/// without costing the caller the lock it already holds.
///
/// Detcore rewrites a blocking request to `LOCK_NB` so a guest thread cannot
/// park in the kernel where the deterministic scheduler cannot see it. Linux
/// converts a lock non-atomically -- `flock_lock_inode` deletes the caller's
/// existing lock before it scans for a conflict -- so that rewrite silently
/// destroys a `LOCK_SH` the guest is relying on and then reports `EWOULDBLOCK`.
/// Natively the guest never sees that state, because a blocking request sleeps
/// and eventually acquires.
///
/// The guest measures survival through a fresh open file description after
/// releasing the separate shared contender, so the probe can only be answered
/// by the original lock. Keeping both descriptions in one process ensures this
/// test exercises the known-mode restore path; the separate SCM_RIGHTS test
/// below covers state whose history Detcore did not observe. Pre-fix control:
/// with the
/// restore suppressed, the probe acquires and the guest prints
/// `FAIL: the refused upgrade destroyed this process's shared lock`.
#[test]
fn contended_blocking_upgrade_is_refused_without_losing_the_shared_lock() {
    let run = hermit_run("off", &[], "upgrade");
    assert!(
        run.status.success(),
        "non-strict upgrade run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-upgrade-parent-holds-shared",
        "flock-upgrade-contender-holds-shared",
        // Non-strict policy: the guest gets a normal errno rather than a
        // fail-closed shutdown. ENOLCK is 37 on Linux/x86_64.
        "flock-upgrade-refused errno=37",
        "flock-upgrade-preserved-shared-lock",
        "flock-upgrade-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// The same contention under a fail-closed run must stop the run rather than
/// hand back an errno.
///
/// `--strict` sets `panic_on_unsupported_syscalls`, which the CLI couples to
/// `shutdown_on_unsupported_syscall`, so the refusal goes through
/// `Detcore::refuse_unserviceable_operation` and terminates. Asserting the
/// diagnostic as well as the exit status keeps this from passing on an
/// unrelated failure: a timeout or a broken guest would exit non-zero too.
#[test]
fn contended_blocking_upgrade_is_fail_closed_under_strict() {
    let run = hermit_run("error", &["--strict"], "upgrade");
    assert_eq!(
        run.status.code(),
        Some(1),
        "a contended blocking flock upgrade must fail closed with exit 1, not time out or die for an unrelated reason\n{}",
        run.combined()
    );
    assert!(
        run.stdout.contains("flock-upgrade-contender-holds-shared"),
        "the strict run did not reach the contended upgrade\n{}",
        run.combined()
    );
    assert!(
        !run.stdout.contains("flock-upgrade-ok"),
        "the strict run completed the upgrade scenario instead of failing closed\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains("blocking flock(fd=")
            && run
                .stderr
                .contains("Refusing rather than granting a lock another guest holds"),
        "the strict run did not report the specific flock refusal\n{}",
        run.combined()
    );
}

/// A descriptor received through `SCM_RIGHTS` can already hold a lock, but its
/// acquisition history is outside Detcore's descriptor model. A blocking
/// conversion must therefore be refused before issuing the destructive
/// nonblocking probe. The guest proves the received shared lock still excludes
/// a fresh open file description after the refusal.
#[test]
fn blocking_upgrade_on_received_fd_preserves_unknown_lock_state() {
    let run = hermit_run("error", &[], "received");
    assert!(
        run.status.success(),
        "received-fd upgrade run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-received-upgrade-refused errno=37",
        "flock-received-upgrade-preserved-shared-lock",
        "flock-received-upgrade-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    assert!(
        run.stderr
            .contains("existed before Detcore observed its lock state"),
        "the run did not take the unknown-state refusal path\n{}",
        run.combined()
    );
}

/// A copied DBT fork child cannot enter the Rust Detcore syscall handler. Its
/// blocking flock must therefore fail closed before reaching the kernel; otherwise
/// it sleeps behind the parent lock while the parent waits for the child.
#[cfg(feature = "dbt")]
#[test]
fn dbt_forked_child_blocking_flock_fails_closed_without_deadlock() {
    let run = hermit_run_backend_timeout("dbt", "error", &[], "fork-blocking-refusal", "5s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "copied DBT child flock must return ENOLCK, not time out or run natively\n{}",
        run.combined()
    );
    for marker in [
        "flock-fork-child-blocking-refused errno=37",
        "flock-fork-child-refusal-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// Copied DBT children still use the kernel for operations that cannot block:
/// malformed operations retain EINVAL, nonblocking locks retain their real
/// result, and unlock changes the shared open file description.
#[cfg(feature = "dbt")]
#[test]
fn dbt_forked_child_preserves_safe_flock_operations() {
    let run = hermit_run_backend_timeout("dbt", "error", &[], "fork-safe-operations", "5s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "copied DBT child safe flock operations changed or timed out\n{}",
        run.combined()
    );
    for marker in [
        "flock-fork-child-malformed-einval",
        "flock-fork-child-nonblocking-contended errno=11",
        "flock-fork-child-nonblocking-ok",
        "flock-fork-child-unlock-ok",
        "flock-fork-child-safe-operations-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// A copied DBT fork child is already outside the ordinary Detcore syscall
/// path. Its nested vfork child nevertheless reaches the copied-syscall policy
/// on the pinned Reverie runtime, where blocking flock returns ENOLCK before
/// reaching the kernel. Native Linux times out on the identical operation.
///
/// The exact event shape distinguishes this from both ordinary Detcore flock
/// handling and the root-process vfork guard: only the root's two setup and two
/// cleanup flocks appear as Detcore inbound events, while the nested child
/// reports ENOLCK and exits.
/// Removing only `copied_child_flock_action`'s blocking refusal makes the DBT
/// half time out with status 124, just like the native control.
#[cfg(feature = "dbt")]
#[test]
fn dbt_nested_vfork_child_blocking_flock_reaches_the_copied_policy() {
    let mut native = Command::new("timeout");
    native
        .args(["--kill-after", "1s", "2s"])
        .arg(guest())
        .arg("fork-vfork-blocking");
    let native = finish(
        native
            .output()
            .expect("failed to start the native nested-vfork control"),
    );
    assert_eq!(
        native.status.code(),
        Some(124),
        "the native control must block in the kernel or the DBT comparison proves nothing\n{}",
        native.combined()
    );
    assert!(
        native.stdout.contains("flock-fork-vfork-child-entered"),
        "the native control never reached the copied-fork-child analogue\n{}",
        native.combined()
    );

    let run = hermit_run_backend_timeout("dbt", "info", &[], "fork-vfork-blocking", "5s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "the copied-syscall flock guard did not stop the nested vfork child before the kernel block\n{}",
        run.combined()
    );
    for marker in [
        "flock-fork-vfork-child-entered",
        "flock-nested-vfork-blocking-refused errno=37",
        "flock-fork-vfork-copied-policy-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
    assert_eq!(
        run.stderr.matches("inbound syscall: flock").count(),
        4,
        "the nested flock unexpectedly entered the ordinary Detcore syscall handler\n{}",
        run.combined()
    );
    assert!(
        !run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
        "the root-process vfork guard fired instead of the copied-child flock policy\n{}",
        run.combined()
    );
    assert!(
        !run.stderr.contains(&format!(
            "unsupported syscall {} in copied child",
            libc::SYS_vfork
        )),
        "the nested vfork itself was refused instead of preserving its safe copied-child path\n{}",
        run.combined()
    );
    assert!(
        !run.stderr.contains("blocking flock(fd="),
        "the nested flock unexpectedly reached the ordinary Detcore refusal path\n{}",
        run.combined()
    );
}

fn assert_failed_clone_preserves_known_flock_state(backend: &str) {
    let run = hermit_run_backend(backend, "error", &[], "failed-clone");
    assert!(
        run.status.success(),
        "{backend} failed-clone run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-failed-clone-rejected errno=22",
        "flock-after-failed-clone-acquired",
        "flock-failed-clone-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "{backend} missed {marker}\n{}",
            run.combined()
        );
    }
}

/// A failed process-clone syscall creates no child capable of changing an
/// inherited open file description. It must therefore preserve known flock
/// state. The invalid CLONE_SIGHAND-without-CLONE_VM call returns EINVAL; an
/// uncontended blocking LOCK_EX immediately afterwards must still succeed.
#[test]
fn failed_process_clone_preserves_known_flock_state() {
    assert_failed_clone_preserves_known_flock_state("ptrace");
}

#[cfg(feature = "dbt")]
#[test]
fn dbt_failed_process_clone_preserves_known_flock_state() {
    assert_failed_clone_preserves_known_flock_state("dbt");
}

fn assert_pidfd_getfd_alias_mutation_invalidates_source_flock_authority(backend: &str) {
    let run = hermit_run_backend(backend, "error", &[], "pidfd-getfd");
    assert!(
        run.status.success(),
        "{backend} pidfd_getfd flock-alias run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-pidfd-duplicate-unlocked",
        "flock-pidfd-source-upgrade-refused errno=37",
        "flock-pidfd-stale-restore-absent",
        "flock-pidfd-failed-getfd-preserved errno=9",
        "flock-pidfd-valid-pidfd-valid-targetfd-flags-precedence errno=22",
        "flock-pidfd-valid-pidfd-invalid-targetfd-flags-precedence errno=22",
        "flock-pidfd-invalid-pidfd-valid-targetfd-flags-precedence errno=22",
        "flock-pidfd-invalid-pidfd-invalid-targetfd-flags-precedence errno=22",
        "flock-pidfd-recorded-failure-preserved errno=22",
        "flock-pidfd-unrelated-authority-preserved",
        "flock-pidfd-foreign-source-refused errno=95",
        "flock-pidfd-getfd-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "{backend} missed {marker}\n{}",
            run.combined()
        );
    }
}

/// A self `pidfd_getfd` result aliases the source open file description. After
/// the duplicate releases its lock, Detcore must not let the source's stale
/// cache restore that lock during a refused blocking conversion. Failed calls
/// preserve source authority, and duplicating an unrelated descriptor does not
/// poison an independently held lock.
#[test]
fn pidfd_getfd_alias_mutation_invalidates_source_flock_authority() {
    assert_pidfd_getfd_alias_mutation_invalidates_source_flock_authority("ptrace");
}

/// The raw dispatcher, not only the handler unit, must apply the zero-flags
/// relaxed-mode boundary before any identity or pidfd syscall injection. The
/// invalid descriptors distinguish Detcore's EOPNOTSUPP from Linux EBADF.
#[test]
fn pidfd_getfd_relaxed_mode_refuses_before_any_kernel_injection() {
    let run = hermit_run(
        "debug",
        &["--no-sequentialize-threads"],
        "pidfd-getfd-relaxed-refusal",
    );
    assert!(
        run.status.success() && run.stdout.contains("flock-pidfd-relaxed-refused errno=95"),
        "raw relaxed-mode pidfd_getfd did not return EOPNOTSUPP and continue\n{}",
        run.combined()
    );
    for syscall in ["getpid", "gettid", "pidfd_getfd"] {
        assert!(
            injected_syscall_offset(&run.stderr, syscall).is_none(),
            "relaxed-mode pidfd_getfd reached an injected {syscall} syscall\n{}",
            run.combined()
        );
    }
}

/// `accept` may park its thread while a sibling sharing the same Linux files
/// table performs the descriptor operations needed to create the eventual
/// connection. Those ordinary operations must retain Linux semantics; a
/// handler-spanning table token either rejects them or deadlocks the wakeup.
#[test]
fn blocking_accept_allows_sibling_descriptor_mutations_to_unblock_it() {
    let run = hermit_run_backend_timeout("ptrace", "debug", &[], "shared-table-fd-liveness", "10s");
    assert!(
        run.status.success(),
        "shared-table descriptor liveness run failed\n{}",
        run.combined()
    );
    assert!(
        run.stdout.contains("flock-shared-table-fd-liveness-ok"),
        "shared-table descriptor liveness marker missing\n{}",
        run.combined()
    );
    let accept = ["accept", "accept4"]
        .into_iter()
        .filter_map(|syscall| injected_syscall_offset(&run.stderr, syscall))
        .min()
        .unwrap_or_else(|| panic!("accept injection missing\n{}", run.combined()));
    let mutation = injected_syscall_offset_after(&run.stderr, "dup2", accept)
        .unwrap_or_else(|| panic!("sibling dup2 injection missing\n{}", run.combined()));
    let client = run
        .stderr
        .rfind("beginning inject of syscall: socket,")
        .unwrap_or_else(|| panic!("client socket injection missing\n{}", run.combined()));
    assert!(
        accept < mutation && mutation < client,
        "accept must be injected and parked before sibling mutation and later client creation\n{}",
        run.combined()
    );
}

/// `recvmsg` uses the same scheduler-aware nonblocking path as `accept`. A
/// sibling must be able to change shared descriptor slots and then send the
/// message that makes the parked receive runnable. The injection order proves
/// the structural mutation and send happened after recvmsg first reached the
/// kernel, rather than relying only on a settling delay.
#[test]
fn blocking_recvmsg_allows_sibling_descriptor_mutations_to_unblock_it() {
    let run = hermit_run_backend_timeout(
        "ptrace",
        "debug",
        &[],
        "shared-table-recvmsg-liveness",
        "10s",
    );
    assert!(
        run.status.success()
            && run
                .stdout
                .contains("flock-shared-table-recvmsg-liveness-ok"),
        "shared-table recvmsg liveness run failed\n{}",
        run.combined()
    );
    let recvmsg = injected_syscall_offset(&run.stderr, "recvmsg")
        .unwrap_or_else(|| panic!("recvmsg injection missing\n{}", run.combined()));
    let mutation = injected_syscall_offset_after(&run.stderr, "dup2", recvmsg)
        .unwrap_or_else(|| panic!("sibling dup2 injection missing\n{}", run.combined()));
    let sendmsg = injected_syscall_offset_after(&run.stderr, "sendmsg", mutation)
        .unwrap_or_else(|| panic!("sendmsg injection missing\n{}", run.combined()));
    assert!(
        recvmsg < mutation && mutation < sendmsg,
        "recvmsg must be injected and parked before sibling mutation and the unblocking sendmsg\n{}",
        run.combined()
    );
}

fn assert_transferred_lock_state_is_unknown_to_the_sender(backend: &str) {
    for scenario in ["sent-after-fork", "sent-after-fork-mmsg"] {
        let run = hermit_run_backend(backend, "error", &[], scenario);
        assert!(
            run.status.success(),
            "{backend} transferred-lock run failed\n{}",
            run.combined()
        );
        for marker in [
            "flock-sender-locked-after-fork",
            "flock-receiver-unlocked-transferred-lock",
            "flock-sender-upgrade-refused errno=37",
            "flock-transfer-release-remained-unlocked",
            "flock-sent-after-fork-ok",
        ] {
            assert!(
                run.stdout.contains(marker),
                "{backend} missed {marker}\n{}",
                run.combined()
            );
        }
    }
}

/// A successful SCM_RIGHTS transfer creates another process that can mutate the
/// same open file description. Once the receiver unlocks it, the sender must not
/// restore its stale pre-transfer shared mode during a later failed conversion.
#[test]
fn transferred_lock_state_is_unknown_to_the_sender() {
    assert_transferred_lock_state_is_unknown_to_the_sender("ptrace");
}

/// A failed send transfers no descriptor, so the sender retains authoritative
/// flock state and an uncontended blocking conversion must still succeed.
#[test]
fn failed_send_preserves_known_flock_state() {
    let run = hermit_run("error", &[], "failed-send");
    assert!(
        run.status.success(),
        "failed-send run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-failed-send-rejected errno=9",
        "flock-after-failed-send-acquired",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// A positive sendmmsg result says at least one message was consumed, but mutable
/// guest metadata cannot safely identify a narrower descriptor set across a
/// deschedule. The conservative rule makes all cached flock modes unknown.
#[test]
fn partial_sendmmsg_invalidates_all_flock_state() {
    let run = hermit_run("error", &[], "partial-sendmmsg");
    assert!(
        run.status.success(),
        "partial-sendmmsg run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-partial-sendmmsg-sent-one",
        "flock-partial-sendmmsg-invalidated-all",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
}

/// A DBT vfork child is not observable by the external runtime before it execs
/// or exits. If an inherited open file description already holds a flock, a
/// blocking conversion in that child can deadlock the complete process tree.
/// Refuse that unsafe root-process vfork before copying. The guest closes stdio
/// immediately before the call, so the regular-file lock is the only unsafe
/// modeled state. Guest-visible EOPNOTSUPP lets the caller continue and exit 0;
/// that exit plus the out-of-band diagnostic distinguishes pre-copy refusal
/// from a timeout, runtime-tree abort, or copied-child policy.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_child_flock_fails_closed_without_deadlock() {
    let run = hermit_run_backend_timeout("dbt", "info", &[], "vfork-upgrade", "5s");
    assert!(
        run.status.success(),
        "root DBT vfork flock must return guest-visible EOPNOTSUPP and continue\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
        "root DBT vfork did not report the pre-copy flock refusal\n{}",
        run.combined()
    );
}

/// A successful process fork makes inherited flock state unknown. Unknown can
/// still mean held, so the same vfork guard must refuse rather than let the
/// unobservable child block in the kernel. The guest closes stdio before vfork,
/// so the post-fork regular-file state is the causal unknown descriptor.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_with_unknown_flock_state_fails_closed_without_deadlock() {
    let run = hermit_run_backend_timeout("dbt", "info", &[], "vfork-unknown-upgrade", "5s");
    assert!(
        run.status.success(),
        "DBT vfork with unknown flock state must return EOPNOTSUPP and continue\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
        "DBT vfork with unknown flock state missed its refusal diagnostic\n{}",
        run.combined()
    );
}

/// `clone(CLONE_VFORK)` and `clone3(CLONE_VFORK)` have the same parent-suspension
/// hazard as `vfork(2)`. The native controls reach the blocking child flock and
/// time out. The guest closes stdio before each call, making the known-held
/// regular-file locks causal. DBT must instead reject each call in the root
/// pre-syscall callback, before the kernel copies a child. The clone3 case also
/// proves the guard reads flags from the guest `struct clone_args`, rather than
/// looking only at raw arg0.
#[cfg(feature = "dbt")]
#[test]
fn dbt_clone_vfork_forms_fail_closed_before_copy() {
    for scenario in ["clone-vfork-upgrade", "clone3-vfork-upgrade"] {
        let mut native = Command::new("timeout");
        native
            .args(["--kill-after", "1s", "2s"])
            .arg(guest())
            .arg(scenario);
        let native = finish(
            native
                .output()
                .unwrap_or_else(|error| panic!("failed to start native {scenario}: {error}")),
        );
        assert_eq!(
            native.status.code(),
            Some(124),
            "native {scenario} must block or the DBT refusal is not causal\n{}",
            native.combined()
        );
        let run = hermit_run_backend_timeout("dbt", "info", &[], scenario, "5s");
        assert!(
            run.status.success(),
            "DBT {scenario} must return EOPNOTSUPP before copying and continue\n{}",
            run.combined()
        );
        assert!(
            run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
            "DBT {scenario} missed the pre-copy flock diagnostic\n{}",
            run.combined()
        );
    }
}

/// A freestanding static guest has no loader or libc startup syscalls: each
/// binary's first trapped syscall is exactly vfork, clone(CLONE_VFORK), or
/// clone3(CLONE_VFORK). Null runtime state therefore means unknown descriptor
/// provenance, not an empty table. DBT must use action 1 to return exact
/// EOPNOTSUPP, emit the refusal diagnostic, avoid all child output, and let the
/// caller write its success marker. The only suppressed syscalls are the
/// lifecycle refusal and that reporting write; exit remains a deferred native
/// lifecycle operation, so `rewritten=2` is an exact callback contract.
#[cfg(feature = "dbt")]
#[test]
fn dbt_first_syscall_vfork_forms_fail_closed_before_runtime_initialization() {
    let definitions = [
        ("vfork", "flock-first-vfork-refused errno=95 continued"),
        (
            "clone(CLONE_VFORK)",
            "flock-first-clone-vfork-refused errno=95 continued",
        ),
        (
            "clone3(CLONE_VFORK)",
            "flock-first-clone3-vfork-refused errno=95 continued",
        ),
    ];
    for ((name, marker), program) in definitions.into_iter().zip(first_syscall_vfork_guests()) {
        let run = hermit_run_program_backend_timeout("dbt", "info", &[], program, &[], "5s");
        assert!(
            run.status.success() && run.stdout.contains(marker),
            "DBT first-syscall {name} did not return EOPNOTSUPP and continue\n{}",
            run.combined()
        );
        assert!(
            !run.stdout
                .contains("flock-first-vfork-child-reached-kernel"),
            "DBT first-syscall {name} executed a child mutation\n{}",
            run.combined()
        );
        assert!(
            run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
            "DBT first-syscall {name} missed the pre-copy diagnostic\n{}",
            run.combined()
        );
        assert!(
            !run.stderr
                .contains("detcore-dbt: initializing Detcore thread state"),
            "DBT first-syscall {name} initialized state before applying the null-state guard\n{}",
            run.combined()
        );
        assert_eq!(
            dbt_summary_counter(&run, "rewritten="),
            2,
            "DBT first-syscall {name} must rewrite exactly the refused lifecycle call and the reporting write\n{}",
            run.combined()
        );
        assert_eq!(
            dbt_summary_counter(&run, "syscalls="),
            3,
            "DBT first-syscall {name} must observe only lifecycle refusal, reporting write, and exit\n{}",
            run.combined()
        );
    }
}

/// Linux permits a fork-like clone to share its parent's descriptor table
/// without joining the parent's thread group. A native child that closes and
/// reuses a slot therefore changes the parent's slot too. DynamoRIO copies the
/// Rust Tool state into that child instead of sharing the parent's
/// `FileMetadata`, so DBT must refuse before copy rather than let the two models
/// diverge. The native controls prove each fixture actually mutates the shared
/// table; the DBT half requires guest-visible EOPNOTSUPP, continued execution,
/// and the dedicated pre-copy diagnostic.
#[cfg(feature = "dbt")]
#[test]
fn dbt_process_clone_files_is_refused_before_copied_child_mutation() {
    for scenario in ["clone-files-process", "clone3-files-process"] {
        let native = finish(
            Command::new(guest())
                .arg(scenario)
                .output()
                .unwrap_or_else(|error| panic!("failed to start native {scenario}: {error}")),
        );
        assert!(
            native.status.success()
                && native
                    .stdout
                    .contains(&format!("flock-{scenario}-shared-mutation-observed")),
            "native {scenario} did not prove CLONE_FILES table sharing\n{}",
            native.combined()
        );

        let run = hermit_run_backend_timeout("dbt", "info", &[], scenario, "5s");
        assert!(
            run.status.success(),
            "DBT {scenario} must return EOPNOTSUPP before copying and continue\n{}",
            run.combined()
        );
        assert!(
            run.stderr.contains(DBT_PROCESS_CLONE_FILES_REFUSAL),
            "DBT {scenario} missed the shared-files pre-copy diagnostic\n{}",
            run.combined()
        );
        assert!(
            !run.stdout
                .contains(&format!("flock-{scenario}-shared-mutation-observed")),
            "DBT {scenario} executed the copied child's descriptor mutation\n{}",
            run.combined()
        );
        assert!(
            run.stdout
                .contains(&format!("flock-{scenario}-refused errno=95")),
            "DBT {scenario} did not observe guest-visible EOPNOTSUPP\n{}",
            run.combined()
        );
    }
}

/// Startup stdio is inherited before Detcore can observe its lock history. It
/// cannot be presumed harmless by kind: native Linux accepts a flock on an
/// anonymous pipe end and a distinct end contends, and two opens of one PTY
/// slave contend likewise. `Command::output` supplies pipe stdout/stderr here,
/// so ordinary DBT vfork must refuse promptly rather than silently treating
/// those unknown OFDs as unlocked. The next test is the success bracket after
/// every unknown descriptor is closed.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_with_inherited_stdio_fails_closed_promptly() {
    let native = finish(
        Command::new(guest())
            .arg("vfork-stdio-open")
            .output()
            .expect("failed to start native stdio-open vfork control"),
    );
    assert!(
        native.status.success() && native.stdout.contains("flock-vfork-stdio-open-ok"),
        "native stdio-open vfork control failed\n{}",
        native.combined()
    );

    let run = hermit_run_backend_timeout("dbt", "info", &[], "vfork-stdio-open", "5s");
    assert!(
        run.status.success(),
        "DBT vfork with inherited pipe stdio must return EOPNOTSUPP and continue\n{}",
        run.combined()
    );
    assert!(
        run.stdout.contains("flock-vfork-stdio-open-entered"),
        "the stdio-open guest did not reach vfork\n{}",
        run.combined()
    );
    assert!(
        run.stdout
            .contains("flock-vfork-stdio-open-refused errno=95")
            && !run.stdout.contains("flock-vfork-stdio-open-ok")
            && run.stderr.contains(DBT_VFORK_FLOCK_REFUSAL),
        "the stdio-open run did not take the root pre-copy refusal path\n{}",
        run.combined()
    );
}

/// When no descriptor can possibly carry flock state, ordinary DBT vfork remains
/// available. This brackets both known/unknown refusal paths above and prevents
/// the conservative guard from becoming an unconditional vfork ban.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_without_flock_state_still_runs() {
    let run = hermit_run_backend_timeout("dbt", "error", &[], "vfork-no-flock-state", "5s");
    assert_eq!(
        run.status.code(),
        Some(0),
        "DBT vfork without possible flock state must remain available\n{}",
        run.combined()
    );
}

/// Replay must take the kernel lock again for a materialized file, not merely
/// repeat the recorded return value.
///
/// `Replayer::handle_simple` consumes the recorded `Return` and injects
/// nothing, which is valid only when nothing outside the return value depends
/// on the call. `flock`'s entire product is kernel state, so under
/// `handle_simple` a replayed run *printed exactly the output asserted below
/// while holding no locks at all* -- which is why this test counts injections
/// instead of trusting stdout.
///
/// Bracket, measured on this guest at this commit: 5 guest `flock` calls, 5
/// re-issued to the kernel during replay. With the arm reverted to
/// `handle_simple` the count is 0 and the stdout assertions still pass -- the
/// silent failure this test exists to catch.
#[test]
fn replay_reissues_every_flock_for_a_materialized_file() {
    let _guard = record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("exclusion");
    let recorded = finish(record.output().expect("failed to start hermit record"));
    assert!(
        recorded.status.success(),
        "recording the flock exclusion guest failed\n{}",
        recorded.combined()
    );
    assert!(
        recorded.stdout.contains("flock-exclusion-ok"),
        "the recorded run did not complete the exclusion scenario\n{}",
        recorded.combined()
    );

    // `--log=debug` surfaces reverie's injection of each syscall the replayer
    // re-issues. That line is the witness that the kernel really performed the
    // lock operation during replay.
    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=debug", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(replay.output().expect("failed to start hermit replay"));
    assert!(
        replayed.status.success(),
        "replaying the flock exclusion guest failed\n{}",
        replayed.combined()
    );

    let requested = replayed.stderr.matches("inbound syscall: flock").count();
    let injected = replayed
        .stderr
        .matches("beginning inject of syscall: flock")
        .count();
    assert!(
        requested > 0,
        "the replayed guest issued no flock calls at all; the probe is measuring nothing\n{}",
        replayed.combined()
    );
    assert_eq!(
        injected,
        requested,
        "replay re-issued {injected} of {requested} flock calls to the kernel; a replayed \
         flock that is not re-issued establishes no lock, so the replayed run only claims to \
         hold one\n{}",
        replayed.combined()
    );

    for marker in [
        "flock-second-open-excluded",
        "flock-second-process-excluded",
        "flock-exclusion-ok",
    ] {
        assert!(
            replayed.stdout.contains(marker),
            "missing {marker} in the replayed run\n{}",
            replayed.combined()
        );
    }
}

/// `pidfd_getfd` creates a real descriptor alias, so replay must both consume an
/// exact recorded result and execute the syscall again. The guest brackets two
/// successful self-target aliases with all four valid/invalid pidfd and
/// targetfd nonzero-flags combinations, each requiring exact `EINVAL`, and then
/// uses flock state through the aliases; matching stdout alone cannot prove
/// those kernel effects occurred. Count replay injections and require all
/// scenario markers so successful side effects, exact errno replay, and
/// failed-call model preservation remain causal. The separate zero-flags
/// `EBADF` source remains a pre-kernel validation bracket.
#[test]
fn replay_reissues_pidfd_getfd_success_and_failure() {
    let _guard = record_lock();
    let data_dir =
        tempfile::tempdir().expect("failed to create the pidfd_getfd recording directory");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("pidfd-getfd");
    let recorded = finish(
        record
            .output()
            .expect("failed to start pidfd_getfd recording"),
    );
    assert!(
        recorded.status.success(),
        "recording pidfd_getfd failed\n{}",
        recorded.combined()
    );
    assert!(recorded.stdout.contains("flock-pidfd-getfd-ok"));

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=debug", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(replay.output().expect("failed to start pidfd_getfd replay"));
    assert!(
        replayed.status.success(),
        "replaying pidfd_getfd failed\n{}",
        replayed.combined()
    );
    for marker in [
        "flock-pidfd-duplicate-unlocked",
        "flock-pidfd-failed-getfd-preserved errno=9",
        "flock-pidfd-valid-pidfd-valid-targetfd-flags-precedence errno=22",
        "flock-pidfd-valid-pidfd-invalid-targetfd-flags-precedence errno=22",
        "flock-pidfd-invalid-pidfd-valid-targetfd-flags-precedence errno=22",
        "flock-pidfd-invalid-pidfd-invalid-targetfd-flags-precedence errno=22",
        "flock-pidfd-recorded-failure-preserved errno=22",
        "flock-pidfd-unrelated-authority-preserved",
        "flock-pidfd-getfd-ok",
    ] {
        assert!(
            replayed.stdout.contains(marker),
            "replayed pidfd_getfd scenario missed {marker}\n{}",
            replayed.combined()
        );
    }
    assert_eq!(
        replayed
            .stderr
            .matches("beginning inject of syscall: pidfd_getfd")
            .count(),
        6,
        "replay must re-execute both successful self-target pidfd_getfd calls and all four flags-first EINVAL calls; the zero-flags invalid source is refused before kernel injection\n{}",
        replayed.combined()
    );
}

/// Replay cannot reproduce the lock side effect for an external file that was
/// not materialized in the replay root. It must fail closed rather than replay
/// the recorded success while holding no lock.
#[test]
fn replay_refuses_flock_for_a_non_materialized_file() {
    let _guard = record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("holder")
        .arg("/etc/hosts");
    let recorded = finish(record.output().expect("failed to start hermit record"));
    assert!(
        recorded.status.success(),
        "recording flock on the external file failed\n{}",
        recorded.combined()
    );
    assert!(recorded.stdout.contains("flock-holder-ok"));

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(replay.output().expect("failed to start hermit replay"));
    assert_ne!(
        replayed.status.code(),
        Some(124),
        "external-file replay timed out instead of failing closed\n{}",
        replayed.combined()
    );
    assert!(
        !replayed.status.success(),
        "external-file replay reported success without reproducing the flock side effect\n{}",
        replayed.combined()
    );
    assert!(
        replayed.stderr.contains("cannot replay flock side effects")
            && replayed.stderr.contains("outside the replay root"),
        "external-file replay did not report the unsupported flock side effect\n{}",
        replayed.combined()
    );
}

/// A contended blocking conversion is two physical nonblocking operations:
/// the substituted `LOCK_EX|LOCK_NB` probe and, after `EWOULDBLOCK`, one
/// `LOCK_SH|LOCK_NB` restore. Record and replay must agree on both operations
/// while exposing only the deterministic `ENOLCK` refusal to the guest.
///
/// This is deliberately separate from the ordinary-run upgrade test above.
/// A record/replay implementation can pass that test yet consume only the
/// probe's event, inject the guest's original blocking request, omit or double
/// consume the restore event, or return the recorded probe errno. Any of those
/// mistakes either wedges replay, desynchronizes its event stream, or changes
/// the guest-visible errno. The debug-log count binds the internal restore:
/// this scenario has one more kernel `flock` injection than guest `flock`
/// requests, on both record and replay.
#[test]
fn replay_preserves_contended_blocking_upgrade_event_shape_and_errno() {
    let _guard = record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=debug", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("upgrade");
    let recorded = finish(record.output().expect("failed to start hermit record"));
    assert!(
        recorded.status.success(),
        "recording the contended flock upgrade failed or wedged\n{}",
        recorded.combined()
    );

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=debug", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()));
    let replayed = finish(replay.output().expect("failed to start hermit replay"));
    assert!(
        replayed.status.success(),
        "replaying the contended flock upgrade failed, wedged, or desynchronized\n{}",
        replayed.combined()
    );

    for (phase, run) in [("record", &recorded), ("replay", &replayed)] {
        for marker in [
            "flock-upgrade-parent-holds-shared",
            "flock-upgrade-contender-holds-shared",
            "flock-upgrade-refused errno=37",
            "flock-upgrade-preserved-shared-lock",
            "flock-upgrade-ok",
        ] {
            assert!(
                run.stdout.contains(marker),
                "{phase} missed {marker}; the guest-visible refusal or restored lock changed\n{}",
                run.combined()
            );
        }

        let requested = run.stderr.matches("inbound syscall: flock").count();
        let injected = run
            .stderr
            .matches("beginning inject of syscall: flock")
            .count();
        assert!(
            requested > 0,
            "{phase} observed no guest flock requests; the event-shape bracket is inert\n{}",
            run.combined()
        );
        assert_eq!(
            injected,
            requested + 1,
            "{phase} must inject every guest flock request plus exactly one internal restore; \
             requested={requested}, injected={injected}\n{}",
            run.combined()
        );
    }
}

/// A recording made before flock forwarding must be refused, not replayed.
///
/// Under the old handler `flock` returned `Ok(0)` before ever reaching
/// `record_or_replay`, so a 0x10b recording contains no flock event. This
/// replayer expects one per call, so replaying such a stream would consume the
/// *next* event for every flock and desynchronize the run. `RECORD_VERSION` was
/// bumped after 0x10b precisely so the gate in `hermit-cli/src/replay.rs` refuses
/// it up front.
///
/// The fixture is a real current recording with only its metadata version
/// rewritten, so this exercises the live gate rather than a hand-built file,
/// and the unmodified recording is replayed first to prove the refusal comes
/// from the version and not from a broken fixture.
#[test]
fn pre_flock_recordings_are_refused_by_the_version_gate() {
    let _guard = record_lock();
    let data_dir = tempfile::tempdir().expect("failed to create the flock recording directory");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "10s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=120"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .args(["--"])
        .arg(guest())
        .arg("holder");
    let recorded = finish(record.output().expect("failed to start hermit record"));
    assert!(
        recorded.status.success(),
        "recording the flock holder guest failed\n{}",
        recorded.combined()
    );

    let replay_command = |dir: &Path| {
        let mut replay = Command::new("timeout");
        replay
            .args(["--kill-after", "10s", "180s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["--log=off", "replay", "--autopilot"])
            .arg(format!("--data-dir={}", dir.display()));
        finish(replay.output().expect("failed to start hermit replay"))
    };

    // Positive half of the bracket: the untouched recording replays.
    let accepted = replay_command(data_dir.path());
    assert!(
        accepted.status.success(),
        "the current-version recording did not replay; the fixture is broken, so the refusal \
         below would prove nothing\n{}",
        accepted.combined()
    );

    let metadata_path = find_metadata(data_dir.path());
    let text = fs::read_to_string(&metadata_path).expect("failed to read the recording metadata");
    let mut metadata: serde_json::Value =
        serde_json::from_str(&text).expect("recording metadata is not JSON");
    let current = metadata["version"]
        .as_u64()
        .expect("recording metadata has no numeric version");
    assert_eq!(
        current, 0x10d,
        "RECORD_VERSION moved; point this test at the new pre-flock predecessor"
    );
    metadata["version"] = serde_json::json!(0x10b);
    fs::write(
        &metadata_path,
        serde_json::to_string(&metadata).expect("failed to serialize the rewritten metadata"),
    )
    .expect("failed to rewrite the recording metadata");

    // Negative half: the same recording, labelled as pre-flock, is refused.
    let refused = replay_command(data_dir.path());
    assert!(
        !refused.status.success(),
        "a 0x10b recording was replayed instead of refused; it has no flock event, so the \
         replay would read another thread's event for every flock\n{}",
        refused.combined()
    );
    assert!(
        refused.stderr.contains("Version mismatch"),
        "the 0x10b recording failed for some reason other than the version gate\n{}",
        refused.combined()
    );
}

fn find_metadata(data_dir: &Path) -> PathBuf {
    for entry in fs::read_dir(data_dir).expect("failed to read the recording directory") {
        let path = entry
            .expect("failed to read a recording directory entry")
            .path();
        let candidate = path.join("metadata.json");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("no recording metadata.json below {}", data_dir.display());
}
