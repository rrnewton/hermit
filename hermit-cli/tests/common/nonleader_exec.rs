// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved. Licensed under the BSD-style license in LICENSE.

//! A worker survives exec as the leader twice, retaining its clock and PMU work.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use detcore::Digest;
use detcore::preemptions::PreemptionRecord;
use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::RecordEnvelopeReport;
use hermit::canonical_verdict::Verdict;
use hermit::canonical_verdict::VerificationReport;
use regex::Regex;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Scenario {
    Original,
    ExitOnly,
    Preempted,
    RunnableLeader,
}

fn assert_pmu_handoffs(log: &str, stdout: &str) {
    let identity =
        Regex::new(r"(?m)^before round=(\d+) pid=(\d+) worker=(\d+) peer=(\d+)$").unwrap();
    let identities: Vec<_> = identity.captures_iter(stdout).collect();
    assert_eq!(identities.len(), 2, "two actual nonleader exec boundaries");
    let lines: Vec<_> = log.lines().collect();
    for (round, identity) in identities.iter().enumerate() {
        assert_eq!(identity[1].parse::<usize>().unwrap(), round);
        let leader = &identity[2];
        let worker = &identity[3];
        assert_ne!(leader, worker);
        let exec = lines
            .iter()
            .position(|line| {
                line.contains(&format!(
                    "[detcore, dtid {worker}] inbound syscall: execve("
                ))
            })
            .expect("the identified worker really called execve");
        let raw_clock = |line: &str| {
            line.split_once("local rcb clock_value ")
                .expect("actual PMU counter")
                .1
                .parse::<u64>()
                .expect("numeric PMU counter")
        };
        let before = lines[..exec]
            .iter()
            .rfind(|line| line.contains(&format!("[dtid {worker}] updated rcb clock,")))
            .expect("worker clock accounted before exec");
        let after = lines[exec + 1..]
            .iter()
            .find(|line| line.contains(&format!("[dtid {leader}] updated rcb clock,")))
            .expect("replacement image resumes clock accounting as the leader");
        assert!(raw_clock(before) > 0, "worker executed counted branches");
        assert!(
            raw_clock(after) >= raw_clock(before),
            "exec must retain the actual PMU counter: {before}\n{after}"
        );
        let logical_rcbs = |line: &str| {
            line.split_once(", rcbs: ")
                .expect("logical branch accounting")
                .1
                .split_once(',')
                .unwrap()
                .0
                .parse::<u64>()
                .expect("numeric logical branch count")
        };
        // update_logical_time_rcbs charges exactly the raw-counter delta. The
        // surviving logical clock must retain prior work without charging it
        // twice, even though its scheduler identity becomes the leader.
        assert_eq!(
            logical_rcbs(after),
            logical_rcbs(before) + raw_clock(after) - raw_clock(before),
            "exec must preserve logical branch accounting: {before}\n{after}"
        );
    }
}

pub(super) fn run() {
    run_fixture(Scenario::Original);
}

pub(super) fn run_exit_only() {
    run_fixture(Scenario::ExitOnly);
}

pub(super) fn run_preempted() {
    run_fixture(Scenario::Preempted);
}

pub(super) fn run_runnable_leader() {
    run_fixture(Scenario::RunnableLeader);
}

fn assert_preemption_handoffs(log: &str, stdout: &str, runnable_leader: bool) {
    let identity =
        Regex::new(r"(?m)^before round=(\d+) pid=(\d+) worker=(\d+) peer=(\d+)$").unwrap();
    let identities: Vec<_> = identity.captures_iter(stdout).collect();
    assert_eq!(identities.len(), 2);
    let lines: Vec<_> = log.lines().collect();
    let mut previous_handoff = 0;
    for identity in identities {
        let leader = &identity[2];
        let worker = &identity[3];
        let exec = lines
            .iter()
            .position(|line| {
                line.contains(&format!(
                    "[detcore, dtid {worker}] inbound syscall: execve("
                ))
            })
            .expect("the identified worker actually execs");
        let after_clock = exec
            + 1
            + lines[exec + 1..]
                .iter()
                .position(|line| line.contains(&format!("[dtid {leader}] updated rcb clock,")))
                .expect("replacement starts accounting the survivor's clock");
        let ending = |tid: &str| format!("[detcore, dtid {tid}] ending timeslice T");
        let number = |line: &str, tid: &str| {
            line.split_once(&ending(tid))
                .unwrap()
                .1
                .split_once('.')
                .unwrap()
                .0
                .parse::<u64>()
                .unwrap()
        };
        let before = lines[..after_clock]
            .iter()
            .rfind(|line| line.contains(&ending(worker)))
            .expect("worker ends a real timeslice before takeover");
        let after = lines[after_clock..]
            .iter()
            .find(|line| line.contains(&ending(leader)))
            .expect("replacement ends the next timeslice");
        assert_eq!(
            number(after, leader),
            number(before, worker) + 1,
            "timeslice numbering survives takeover: {before}\n{after}"
        );
        let timer = |tid: &str| format!("[detcore, dtid {tid}] inbound timer preemption event");
        assert!(
            lines[previous_handoff..exec]
                .iter()
                .any(|line| line.contains(&timer(worker))),
            "worker must receive an actual PMU timer before exec"
        );
        let next_exec = lines[after_clock..]
            .iter()
            .position(|line| line.contains("inbound syscall: execve("))
            .map_or(lines.len(), |offset| after_clock + offset);
        // In the runnable case the replacement later becomes the next round's
        // spinning leader. Its spin must not satisfy the replacement timer
        // check: require the timer before the next leader-spin phase.
        let replacement_end = lines[after_clock..next_exec]
            .iter()
            .position(|line| {
                line.contains(&format!(
                    "[detcore, dtid {leader}] inbound syscall: getppid("
                ))
            })
            .map_or(next_exec, |offset| after_clock + offset);
        assert!(
            lines[after_clock..replacement_end]
                .iter()
                .any(|line| line.contains(&timer(leader))),
            "replacement must receive an actual PMU timer after exec"
        );
        if runnable_leader {
            let spin_start = previous_handoff
                + lines[previous_handoff..exec]
                    .iter()
                    .position(|line| {
                        line.contains(&format!(
                            "[detcore, dtid {leader}] inbound syscall: getppid("
                        ))
                    })
                    .expect("the displaced leader actually entered its runnable spin");
            let leader_preemptions = lines[spin_start..exec]
                .iter()
                .filter(|line| line.contains(&timer(leader)))
                .count();
            assert!(
                leader_preemptions >= 3,
                "the displaced leader's spin needs several PMU preemptions, got {leader_preemptions}"
            );
        }
        previous_handoff = after_clock;
    }
}

fn run_fixture(scenario: Scenario) {
    let exit_only = scenario == Scenario::ExitOnly;
    let preempted = matches!(scenario, Scenario::Preempted | Scenario::RunnableLeader);
    let _lock = super::hermit_run_guard();
    let start = Instant::now();
    let remaining = || {
        Duration::from_secs(55)
            .checked_sub(start.elapsed())
            .expect("the complete regression must fit the existing 57-second test budget")
    };
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let prefix = match scenario {
        Scenario::Original => "ptrace-nonleader-exec-",
        Scenario::ExitOnly => "ptrace-nonleader-exec-exit-",
        Scenario::Preempted => "ptrace-nonleader-exec-preempted-",
        Scenario::RunnableLeader => "ptrace-nonleader-exec-runnable-",
    };
    let root = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained fixture directory")
        .keep();
    eprintln!(
        "ptrace nonleader exec artifacts retained at {}",
        root.display()
    );
    let fixture_name = if exit_only {
        "nonleader_exec_exit.c"
    } else {
        "nonleader_exec.c"
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture_name);
    fs::copy(&fixture, root.join("guest.c")).expect("retain exact fixture");
    let guest = root.join("program");
    let compile = root.join("compile");
    let mut compiler = Command::new("cc");
    compiler.args([
        "-std=gnu11",
        "-O0",
        "-g",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-pthread",
    ]);
    if preempted {
        compiler.arg("-DNONLEADER_EXEC_PREEMPT");
    }
    if scenario == Scenario::RunnableLeader {
        compiler.arg("-DNONLEADER_EXEC_RUNNABLE_LEADER");
    }
    compiler.arg(&fixture).arg("-o").arg(&guest);
    let status = bounded_command_with_timeout(&mut compiler, &compile, remaining());
    assert!(
        status.success(),
        "fixture compilation failed: {}",
        compile.display()
    );
    let mut previous_stdout = None;
    for pair in 0..3 {
        let directory = root.join(format!("pair-{pair}"));
        let logs = directory.join("verify-logs");
        fs::create_dir_all(&logs).unwrap();
        let report_path = directory.join("verification.json");
        let mut args = vec![
            "--log=trace",
            "run",
            "--backend=ptrace",
            "--base-env=minimal",
            // CARGO_TARGET_TMPDIR may itself be under host /tmp. Keep that
            // fixture visible just as the neighboring CLI guest tests do.
            "--tmp=/tmp",
            "--strict",
            "--epoch=2026-01-01T00:00:00.123456789+00:00",
            if preempted {
                "--max-timeslice=1000000"
            } else {
                "--max-timeslice=200000000"
            },
            "--verify",
            "--verify-strict",
            "--verify-json",
            report_path.to_str().unwrap(),
            "--keep-logs",
            "--verify-log-dir",
            logs.to_str().unwrap(),
            "--",
            guest.to_str().unwrap(),
        ];
        if scenario == Scenario::RunnableLeader {
            // PAUSE slows branch retirement in both hot loops. A 768-RCB early
            // notification shortens each single-step correction tail while
            // retaining the same workload and repeated leader preemptions.
            // The precise target and refusal of every overshoot are unchanged.
            args.insert(8, "--skid-margin=768");
        } else if preempted {
            // Keep the blocked-leader cell's existing early notification.
            args.insert(8, "--skid-margin=3072");
        }
        let mut command = super::hermit_command(&args);
        command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
        let status = bounded_command_with_timeout(&mut command, &directory, remaining());
        assert_eq!(
            status.code(),
            Some(0),
            "ptrace pair {pair}: {}",
            directory.display()
        );
        let output = bounded_read(&directory.join("stdout"), MIB);
        let stdout = std::str::from_utf8(&output).expect("guest trajectory text");
        if exit_only {
            // The replacement only exits successfully. Its success must not
            // depend on an external supervisor noticing missing output; the
            // skipped-reconnect mutation must fail in the runtime itself.
            assert!(stdout.is_empty(), "the exit-only guest has no output");
        } else {
            assert_eq!(stdout.lines().count(), 23, "complete two-exec trajectory");
            assert_eq!(stdout.matches("sample round=").count(), 16);
            assert_eq!(stdout.matches("gone=ESRCH").count(), 2);
            assert_eq!(stdout.matches(" running\n").count(), 2);
            assert!(stdout.ends_with("nonleader-exec-ok rounds=2 final=73 reaped=once\n"));
        }
        if let Some(previous) = &previous_stdout {
            assert_eq!(
                &output, previous,
                "all three full nanosecond trajectories agree"
            );
        }
        let report = VerificationReport::from_current_json_slice(&bounded_read(&report_path, MIB))
            .expect("complete current typed verification receipt");
        report
            .require_canonical_match()
            .expect("nonempty canonical INFO match");
        report
            .require_exact_output_match()
            .expect("exact status/stdout/stderr match");
        assert_eq!(report.guest_exit_code, Some(0));
        assert!(report.guest_signal.is_none());
        let policy = report.comparison.as_ref().unwrap();
        assert_eq!(policy.display_name.as_deref(), Some("BitwiseInfoV1"));
        assert_eq!(policy.record_envelope, RecordEnvelopeReport::AllRecordsV1);
        assert_eq!(policy.compare_io_buffers, Some(true));
        assert_eq!(policy.virtualize_time, Some(true));
        assert_eq!(policy.strip_lines, Some(false));
        assert_eq!(policy.canonicalize_addresses, Some(true));
        assert_eq!(policy.full_trace, Some(true));
        assert_eq!(policy.exact_remainder, Some(true));
        assert_eq!(policy.ignore_lines, Some(false));
        assert_eq!(policy.skip_commit, Some(false));
        assert_eq!(policy.skip_detlog, Some(false));
        assert_eq!(policy.log_scope, Some(ComparedLogScope::Info));
        assert_eq!(
            policy.stripped_prefixes.as_deref(),
            Some(["real-wall-clock-prefix/v1".to_owned()].as_slice())
        );
        assert_eq!(
            policy.canonicalizations.as_deref(),
            Some(["host-address-to-first-appearance-ordinal/v1".to_owned()].as_slice())
        );
        let counts = report.compared_log_messages.as_ref().unwrap();
        assert_eq!(counts.left, counts.right);
        let outputs = report.compared_outputs.as_ref().unwrap();
        for operand in [&outputs.left, &outputs.right] {
            assert_eq!(operand.exit_code, Some(0));
            assert!(operand.signal.is_none());
            assert_eq!(operand.stdout_bytes, output.len() as u64);
            assert_eq!(operand.stdout_sha256, Digest::new(&output).to_string());
            assert_eq!(operand.stderr_bytes, 0);
            assert_eq!(operand.stderr_sha256, Digest::new(b"").to_string());
        }
        for prefix in ["run1_log_", "run2_log_"] {
            let matches: Vec<_> = fs::read_dir(&logs)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(prefix)
                })
                .collect();
            assert_eq!(matches.len(), 1, "one complete retained log per execution");
            let log = String::from_utf8(bounded_read(&matches[0], 64 * MIB)).unwrap();
            if !exit_only {
                assert_pmu_handoffs(&log, stdout);
            }
            if preempted {
                assert_preemption_handoffs(&log, stdout, scenario == Scenario::RunnableLeader);
            }
        }
        previous_stdout = Some(output);
        eprintln!("ptrace nonleader exec pair {pair}: two full canonical executions");
    }
}

const PREEMPTION_REFUSAL: &str =
    "unsupported: preemption recording and replay across nonleader exec";

fn preemption_artifact_case(
    directory: &Path,
    guest: &Path,
    target: &Path,
    artifact_option: &str,
    refused: bool,
    expected_identity: Option<(&str, &str)>,
    timeout: Duration,
) -> String {
    let logs = directory.join("verify-logs");
    fs::create_dir_all(&logs).unwrap();
    let report_path = directory.join("verification.json");
    let args = [
        "--log=trace",
        "run",
        "--backend=ptrace",
        "--base-env=minimal",
        "--tmp=/tmp",
        "--strict",
        "--epoch=2026-01-01T00:00:00.123456789+00:00",
        "--max-timeslice=1000000",
        // Earlier PMU notification, never permission to deliver past target.
        "--skid-margin=3072",
        "--chaos",
        "--seed=2",
        artifact_option,
        "--verify",
        "--verify-strict",
        "--verify-json",
        report_path.to_str().unwrap(),
        "--keep-logs",
        "--verify-log-dir",
        logs.to_str().unwrap(),
        "--",
        guest.to_str().unwrap(),
        target.to_str().unwrap(),
    ];
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let status = bounded_command_with_timeout(&mut command, directory, timeout);
    let stderr = String::from_utf8(bounded_read(&directory.join("stderr"), 16 * MIB)).unwrap();
    let stdout = String::from_utf8(bounded_read(&directory.join("stdout"), MIB)).unwrap();
    let report = VerificationReport::from_current_json_slice(&bounded_read(&report_path, MIB))
        .expect("complete current typed verification receipt");
    if refused {
        // EXIT-CLASS: hermit
        assert_eq!(status.code(), Some(HERMIT_POLICY_REFUSAL_EXIT), "{stderr}");
        assert!(stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"));
        assert_eq!(report.verdict, Verdict::NoResult);
        assert!(report.guest_exit_code.is_none());
        assert!(report.guest_signal.is_none());
        assert!(!report.verified);
        assert!(!report.bitwise_parity);
        assert!(report.compared_outputs.is_none());
        assert!(report.compared_log_messages.is_none());
        assert!(report.require_canonical_match().is_err());
        assert!(
            stdout.is_empty(),
            "verification does not publish rejected output"
        );
    } else {
        // EXIT-CLASS: guest
        assert_eq!(status.code(), Some(0), "{stderr}");
        report.require_canonical_match().unwrap();
        report.require_exact_output_match().unwrap();
        assert_eq!(report.guest_exit_code, Some(0));
        assert!(report.guest_signal.is_none());
        assert!(stdout.ends_with("failed-exec-preserved\nfailed-exec-control-ok\n"));
        let counts = report.compared_log_messages.as_ref().unwrap();
        assert_eq!(counts.left, counts.right);
        for operand in [
            &report.compared_outputs.as_ref().unwrap().left,
            &report.compared_outputs.as_ref().unwrap().right,
        ] {
            assert_eq!(operand.exit_code, Some(0));
            assert!(operand.signal.is_none());
            assert_eq!(operand.stdout_bytes, stdout.len() as u64);
            assert_eq!(
                operand.stdout_sha256,
                Digest::new(stdout.as_bytes()).to_string()
            );
            assert_eq!(operand.stderr_bytes, 0);
        }
    }
    assert!(!stdout.contains("replacement-image-ran"));
    assert!(
        !Path::new(&format!("{}.ran", target.display())).exists(),
        "replacement code must not run, even when policy shutdown discards captured stdout"
    );
    let identity = Regex::new(r"(?m)^prefix sample=0 pid=(\d+) worker=(\d+) nanos=").unwrap();
    let identity = identity.captures(&stdout);
    let (leader, worker) = if refused {
        expected_identity.expect("refusal must use the positive control's process and worker")
    } else {
        let identity = identity
            .as_ref()
            .expect("worker completed the shared prefix");
        let actual = (&identity[1], &identity[2]);
        if let Some(expected) = expected_identity {
            assert_eq!(actual, expected);
        }
        actual
    };
    let mut diagnostic = stderr.contains(PREEMPTION_REFUSAL);
    for prefix in ["run1_log_", "run2_log_"] {
        let paths: Vec<_> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect();
        let expected = usize::from(!refused || prefix == "run1_log_");
        assert_eq!(
            paths.len(),
            expected,
            "refusal cannot produce a second verified run"
        );
        for path in paths {
            let log = String::from_utf8(bounded_read(&path, 64 * MIB)).unwrap();
            let exec = log
                .find(&format!(
                    "[detcore, dtid {worker}] inbound syscall: execve("
                ))
                .expect("the identified worker attempted the same exec path");
            if refused {
                assert!(
                    !log[exec..].contains(&format!("[detcore, dtid {leader}] inbound syscall:")),
                    "the replacement must not enter ordinary syscall handling"
                );
            }
            assert!(
                log[..exec].contains(&format!(
                    "[detcore, dtid {worker}] inbound timer preemption event"
                )),
                "the recorded and replayed prefix contains actual PMU preemption"
            );
            if let Some(path) = artifact_option.strip_prefix("--replay-preemptions-from=") {
                let history: PreemptionRecord =
                    serde_json::from_slice(&bounded_read(Path::new(path), 4 * MIB)).unwrap();
                history.validate().unwrap();
                let (_, history) = history
                    .extract_all()
                    .into_iter()
                    .find(|(tid, _)| tid.to_string() == worker)
                    .expect("replay contains the actual worker");
                let mut expected = history.into_iter();
                let consumed: Vec<_> = log[..exec]
                    .lines()
                    .filter(|line| {
                        line.contains(&format!("[dtid {worker}] next timeslice (T"))
                            && line.contains("set by recording to ")
                    })
                    .collect();
                assert!(
                    !consumed.is_empty(),
                    "replay must actually consume worker history"
                );
                for line in consumed {
                    let (deadline, priority, rcbs) = expected
                        .next_with_rcbs()
                        .expect("each consumed boundary belongs to the recording");
                    assert!(rcbs.is_some(), "the recording contains exact PMU targets");
                    assert!(
                        line.contains(&format!("set by recording to {deadline:?} ")),
                        "{line}"
                    );
                    assert!(line.ends_with(&format!(", priority {priority}")), "{line}");
                }
            }
            diagnostic |= log.contains(PREEMPTION_REFUSAL);
        }
    }
    assert_eq!(
        diagnostic, refused,
        "specific transfer refusal only on successful exec"
    );
    stdout
}

pub(super) fn run_preemption_artifacts() {
    let _lock = super::hermit_run_guard();
    let start = Instant::now();
    let remaining = || {
        Duration::from_secs(55)
            .checked_sub(start.elapsed())
            .expect("record/replay controls must fit the existing 57-second budget")
    };
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = tempfile::Builder::new()
        .prefix("ptrace-nonleader-exec-preemption-artifacts-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .keep();
    eprintln!("nonleader exec preemption artifacts: {}", root.display());
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nonleader_exec_preemptions.c");
    fs::copy(&fixture, root.join("guest.c")).unwrap();
    let guest = root.join("program");
    let status = bounded_command_with_timeout(
        Command::new("cc")
            .args([
                "-std=gnu11",
                "-O0",
                "-g",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-pthread",
            ])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest),
        &root.join("compile"),
        remaining(),
    );
    assert!(status.success(), "fixture compilation");
    let target = root.join("replacement");
    assert!(!target.exists());
    let recording = root.join("failed-exec-preemptions.json");
    let record_option = format!("--record-preemptions-to={}", recording.display());
    let replay_option = format!("--replay-preemptions-from={}", recording.display());
    let recorded_stdout = preemption_artifact_case(
        &root.join("record-failed-exec"),
        &guest,
        &target,
        &record_option,
        false,
        None,
        remaining(),
    );
    let recorded_bytes = bounded_read(&recording, 4 * MIB);
    let history: PreemptionRecord = serde_json::from_slice(&recorded_bytes).unwrap();
    history
        .validate()
        .expect("real completed recording is well formed");
    let identity = Regex::new(r"(?m)^prefix sample=0 pid=(\d+) worker=(\d+) nanos=").unwrap();
    let identity = identity.captures(&recorded_stdout).unwrap();
    let worker = &identity[2];
    let expected_identity = Some((&identity[1], worker));
    assert!(
        history
            .as_vecs()
            .iter()
            .any(|(tid, points)| { tid.to_string() == worker && points.len() > 1 }),
        "the actual exec worker has a nonempty recorded preemption history"
    );
    let replayed_stdout = preemption_artifact_case(
        &root.join("replay-failed-exec"),
        &guest,
        &target,
        &replay_option,
        false,
        expected_identity,
        remaining(),
    );
    assert_eq!(
        recorded_stdout, replayed_stdout,
        "full prefix clocks survive replay"
    );

    // This is an intentional filesystem-compatibility negative: the exact
    // recorded prefix now reaches successful exec at the same pathname/argv.
    // It must refuse; it is not a claim of parity across changed filesystem state.
    fs::copy(&guest, &target).unwrap();
    let marker = root.join("replacement.ran");
    let native = root.join("native-replacement-marker");
    let status = bounded_command_with_timeout(
        Command::new(&target).args(["replacement", "image"]),
        &native,
        remaining(),
    );
    assert_eq!(status.code(), Some(0));
    assert_eq!(bounded_read(&marker, 1), b"1");
    assert_eq!(
        bounded_read(&native.join("stdout"), MIB),
        b"replacement-image-ran\n"
    );
    fs::remove_file(&marker).unwrap();
    preemption_artifact_case(
        &root.join("refuse-replay-transfer"),
        &guest,
        &target,
        &replay_option,
        true,
        expected_identity,
        remaining(),
    );
    let refused_recording = root.join("refused-preemptions.json");
    let option = format!("--record-preemptions-to={}", refused_recording.display());
    preemption_artifact_case(
        &root.join("refuse-record-transfer"),
        &guest,
        &target,
        &option,
        true,
        expected_identity,
        remaining(),
    );
    assert!(
        !refused_recording.exists(),
        "refused transfer cannot publish a replay artifact"
    );
    preemption_artifact_case(
        &root.join("refuse-memory-record-transfer"),
        &guest,
        &target,
        "--record-preemptions",
        true,
        expected_identity,
        remaining(),
    );
    assert_eq!(
        bounded_read(&recording, 4 * MIB),
        recorded_bytes,
        "replay retains its input"
    );
}
