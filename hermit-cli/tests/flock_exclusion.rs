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
//! | [`dbt_fork_does_not_restore_a_lock_released_by_the_child`] | copied DBT state restored a lock the child released |
//! | [`failed_process_clone_preserves_known_flock_state`] | a failed clone made an unlocked descriptor permanently unknown |
//! | [`transferred_lock_state_is_unknown_to_the_sender`] | the sender restored stale state after the receiver unlocked the OFD |
//! | [`dbt_vfork_child_flock_fails_closed_without_deadlock`] | a copied vfork child blocked in the kernel while its parent was suspended |
//! | [`replay_reissues_every_flock_for_a_materialized_file`] | replay consumed the recorded return and took no lock |
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
            .args(["-O1", "-std=c11", "-Wall", "-Wextra", "-Werror"])
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
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", timeout])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .arg(format!("--log={log}"))
        .args(["run", &format!("--backend={backend}"), "--base-env=minimal"])
        .args(extra)
        .arg("--")
        .arg(guest())
        .arg(scenario);
    finish(
        command
            .output()
            .unwrap_or_else(|error| panic!("failed to start hermit for {scenario}: {error}")),
    )
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

/// A fork shares kernel open file descriptions even when a backend hosts the
/// parent and child descriptor metadata separately. After the child unlocks,
/// the parent must not use a stale cached mode to restore that released lock.
#[cfg(feature = "dbt")]
#[test]
fn dbt_fork_does_not_restore_a_lock_released_by_the_child() {
    let run = hermit_run_backend("dbt", "error", &[], "fork-unlock");
    assert!(
        run.status.success(),
        "DBT fork/unlock run failed\n{}",
        run.combined()
    );
    for marker in [
        "flock-fork-child-released-inherited-lock",
        "flock-fork-parent-upgrade-refused errno=37",
        "flock-fork-release-remained-unlocked",
        "flock-fork-unlock-ok",
    ] {
        assert!(
            run.stdout.contains(marker),
            "missing {marker}\n{}",
            run.combined()
        );
    }
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
/// Refuse that unsafe vfork before copying, while still allowing vfork when no
/// known flock is held. The exact exit and diagnostic distinguish refusal from
/// a timeout.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_child_flock_fails_closed_without_deadlock() {
    let run = hermit_run_backend_timeout("dbt", "info", &[], "vfork-upgrade", "5s");
    assert_eq!(
        run.status.code(),
        Some(101),
        "copied DBT vfork flock must fail closed with exit 101, not time out or return\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(
            "detcore-dbt: refusing vfork while an open file description may hold a flock"
        ),
        "copied DBT vfork did not report the flock refusal\n{}",
        run.combined()
    );
}

/// A successful process fork makes inherited flock state unknown. Unknown can
/// still mean held, so the same vfork guard must refuse rather than let the
/// unobservable child block in the kernel.
#[cfg(feature = "dbt")]
#[test]
fn dbt_vfork_with_unknown_flock_state_fails_closed_without_deadlock() {
    let run = hermit_run_backend_timeout("dbt", "info", &[], "vfork-unknown-upgrade", "5s");
    assert_eq!(
        run.status.code(),
        Some(101),
        "DBT vfork with unknown flock state must fail closed, not time out\n{}",
        run.combined()
    );
    assert!(
        run.stderr.contains(
            "detcore-dbt: refusing vfork while an open file description may hold a flock"
        ),
        "DBT vfork with unknown flock state missed its refusal diagnostic\n{}",
        run.combined()
    );
}

/// When no descriptor can possibly carry flock state, ordinary DBT vfork remains
/// available. This brackets the conservative pre-copy refusal above.
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
        "recording flock on the external file failed\\n{}",
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
        "external-file replay timed out instead of failing closed\\n{}",
        replayed.combined()
    );
    assert!(
        !replayed.status.success(),
        "external-file replay reported success without reproducing the flock side effect\\n{}",
        replayed.combined()
    );
    assert!(
        replayed.stderr.contains("cannot replay flock side effects")
            && replayed.stderr.contains("outside the replay root"),
        "external-file replay did not report the unsupported flock side effect\\n{}",
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
/// bumped to 0x10c precisely so the gate in `hermit-cli/src/replay.rs` refuses
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
        current, 0x10c,
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
