/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Tests-first acceptance bracket for schedule-independent network replay.
//!
//! The native fixture test is active now. The Hermit acceptance test remains
//! ignored until the three network policies and run-mode trace flags exist; an
//! explicit ignored run then exercises the real CLI and fails at the first
//! missing behavior without weakening any assertion below.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const CONTROLLER_WALL_SECONDS: u64 = 10;
const SAFEHERMIT_WALL_SECONDS: u64 = 30;
const OUTER_WALL_SECONDS: u64 = 45;
const SAFEHERMIT_MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
const SAFEHERMIT_DISK_LIMIT: &str = "1G";

// A deliberately small, predeclared population. It varies one scheduler seed
// at a time, and crosses a timeslice boundary without turning this focused
// acceptance test into a broad schedule campaign.
const REPLAY_CELLS: &[(u64, u64)] = &[
    (0, 1_000_000),
    (1, 1_000_000),
    (2, 4_000_000),
    (3, 4_000_000),
];

const OUTBOUND_HEX: &str = "726571756573740a6e6578740a";

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(14_695_981_039_346_656_037, |digest, byte| {
            (digest ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
        })
}

fn expected_invariant() -> String {
    format!(
        "aggregate=abcdef eof=1 outbound_hex={OUTBOUND_HEX} outbound_fnv1a64={:016x}",
        fnv1a64(b"request\nnext\n")
    )
}

fn assert_guest_invariants(output: &[u8], label: &str) -> String {
    let text = std::str::from_utf8(output)
        .unwrap_or_else(|error| panic!("{label} stdout was not UTF-8: {error}"));
    let invariants = text
        .lines()
        .filter(|line| line.starts_with("aggregate="))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        invariants,
        BTreeSet::from([expected_invariant().as_str()]),
        "{label} lost the fixed input, EOF, or outbound-stream invariant:\n{text}"
    );

    let markers = text
        .lines()
        .filter_map(|line| line.strip_prefix("reader-marker="))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        markers.len(),
        1,
        "{label} must publish one stable competing-reader marker:\n{text}"
    );
    let marker = markers.into_iter().next().unwrap();
    assert!(
        matches!(marker, "0:abc,1:def" | "1:abc,0:def"),
        "{label} published an impossible reader marker {marker:?}"
    );
    marker.to_owned()
}

fn bounded_command(program: &Path, arguments: &[&OsStr], seconds: u64) -> Output {
    Command::new("timeout")
        .args(["--kill-after=1s", &format!("{seconds}s")])
        .arg(program)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("failed to launch bounded command {program:?}: {error}"))
}

struct Controller {
    child: Option<Child>,
    port_path: PathBuf,
    report_path: PathBuf,
}

impl Controller {
    fn start(program: &Path, directory: &Path) -> (Self, String) {
        let port_path = directory.join("controller.port");
        let report_path = directory.join("controller.report");
        let child = Command::new("timeout")
            .args(["--kill-after=1s", &format!("{CONTROLLER_WALL_SECONDS}s")])
            .arg(program)
            .args(["controller"])
            .arg(&port_path)
            .arg(&report_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start bounded TCP controller");
        let mut controller = Self {
            child: Some(child),
            port_path,
            report_path,
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(port) = fs::read_to_string(&controller.port_path) {
                let port = port.trim().to_owned();
                assert!(
                    port.parse::<u16>().is_ok_and(|value| value != 0),
                    "controller published an invalid port {port:?}"
                );
                return (controller, port);
            }
            if let Some(status) = controller
                .child
                .as_mut()
                .expect("controller child exists")
                .try_wait()
                .expect("failed to inspect TCP controller")
            {
                let output = controller.finish_child();
                panic!(
                    "TCP controller exited before publishing its port: {status}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                Instant::now() < deadline,
                "TCP controller did not publish its port within two seconds"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(mut self) -> String {
        let output = self.finish_child();
        assert!(
            output.status.success(),
            "TCP controller failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_to_string(&self.report_path).expect("controller omitted its exact-stream report")
    }

    fn stop_without_connection(mut self) {
        let child = self.child.as_mut().expect("controller child exists");
        child.kill().expect("failed to stop unused TCP controller");
        let _ = self.finish_child();
        assert!(
            !self.report_path.exists(),
            "fail-closed run unexpectedly reached the external controller"
        );
    }

    fn finish_child(&mut self) -> Output {
        self.child
            .take()
            .expect("controller child exists")
            .wait_with_output()
            .expect("failed to collect bounded TCP controller")
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn assert_controller_report(report: &str) {
    let expected = format!(
        "controller=complete\noutbound_hex={OUTBOUND_HEX}\noutbound_fnv1a64={:016x}\n",
        fnv1a64(b"request\nnext\n")
    );
    assert_eq!(
        report, expected,
        "controller observed a changed outbound stream"
    );
}

fn safehermit_command(
    evidence: &Path,
    label: &str,
    hermit_arguments: &[String],
    guest: &Path,
    guest_arguments: &[&str],
) -> Output {
    let safehermit = std::env::var_os("HERMIT_SAFEHERMIT").unwrap_or_else(|| {
        panic!(
            "set HERMIT_SAFEHERMIT=/home/newton/work/dev-hermit/bin/safehermit; \
             the acceptance bracket refuses an unboxed ad-hoc Hermit execution"
        )
    });
    let report = evidence.join(format!("{label}.safehermit"));
    let log_root = evidence.join("safehermit-logs");
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", &format!("{OUTER_WALL_SECONDS}s")])
        .arg(safehermit)
        .arg("--sh-report")
        .arg(&report)
        .args([
            "--sh-deadline",
            &SAFEHERMIT_WALL_SECONDS.to_string(),
            "--sh-max-log-bytes",
            &SAFEHERMIT_MAX_LOG_BYTES.to_string(),
            "--sh-disk-limit",
            SAFEHERMIT_DISK_LIMIT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(hermit_arguments)
        .arg("--")
        .arg(guest)
        .args(guest_arguments)
        .env("SAFEHERMIT_LOG_ROOT", log_root);
    command.output().unwrap_or_else(|error| {
        panic!("failed to start {label} through the required safehermit wrapper: {error}")
    })
}

fn common_run_arguments(seed: u64, max_timeslice: u64) -> Vec<String> {
    vec![
        "--log=info".into(),
        "run".into(),
        "--backend=ptrace".into(),
        "--strict".into(),
        "--chaos".into(),
        "--seed=0".into(),
        "--rng-seed=0".into(),
        "--fuzz-seed=0".into(),
        format!("--sched-seed={seed}"),
        "--sched-heuristic=random".into(),
        format!("--max-timeslice={max_timeslice}"),
    ]
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_l2_report(path: &Path, label: &str) {
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("{label} omitted verify JSON: {error}")),
    )
    .unwrap_or_else(|error| panic!("{label} verify JSON was malformed: {error}"));
    assert_eq!(report["verdict"], "matched", "{label} report: {report}");
    assert_eq!(report["verified"], true, "{label} report: {report}");
    assert_eq!(report["bitwise_parity"], true, "{label} report: {report}");
    assert_eq!(
        report["comparison"]["strictness"], "canonical",
        "{label} report: {report}"
    );
    assert_eq!(
        report["comparison"]["compare_io_buffers"], true,
        "{label} report: {report}"
    );
    for side in ["left", "right"] {
        assert!(
            report["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "{label} compared no INFO records on {side}: {report}"
        );
    }
}

fn assert_fail_closed(output: &Output, label: &str, required: &[&str]) {
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !matches!(output.status.code(), Some(124..=126)),
        "{label} reached a safety-wrapper bound instead of refusing promptly"
    );
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    for word in required {
        assert!(
            diagnostic.contains(word),
            "{label} did not name {word:?}:\n{diagnostic}"
        );
    }
}

#[test]
fn network_replay_bracket_is_small_fixed_and_independently_seeded() {
    assert_eq!(REPLAY_CELLS.len(), 4);
    assert_eq!(
        REPLAY_CELLS
            .iter()
            .map(|(seed, _)| *seed)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([0, 1, 2, 3])
    );
    assert_eq!(
        REPLAY_CELLS
            .iter()
            .map(|(_, timeslice)| *timeslice)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([1_000_000, 4_000_000])
    );
    for (seed, timeslice) in REPLAY_CELLS {
        let arguments = common_run_arguments(*seed, *timeslice);
        assert!(arguments.contains(&format!("--sched-seed={seed}")));
        assert!(arguments.contains(&"--rng-seed=0".to_owned()));
        assert!(arguments.contains(&"--fuzz-seed=0".to_owned()));
    }
}

#[test]
fn network_replay_tcp_fixture_has_the_exact_native_contract() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let evidence = tempfile::tempdir().expect("create native network bracket directory");
    let (controller, port) = Controller::start(fixture, evidence.path());
    let output = bounded_command(
        fixture,
        &[OsStr::new("client"), OsStr::new(&port), OsStr::new("match")],
        CONTROLLER_WALL_SECONDS,
    );
    assert_success(&output, "native TCP bracket client");
    assert_guest_invariants(&output.stdout, "native TCP bracket client");
    assert_controller_report(&controller.finish());
}

#[test]
#[ignore = "tests-first bracket: network run policies and shared runtime engine are not implemented"]
fn external_tcp_recording_replays_offline_across_schedules_and_refuses_mismatch() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let evidence = tempfile::tempdir().expect("create network replay evidence directory");
    let trace = evidence.path().join("network.trace");

    // Policy 1: deterministic execution without a trace cannot touch even the
    // waiting loopback controller. This also proves that refusal is prompt.
    let (closed_controller, closed_port) = Controller::start(fixture, evidence.path());
    let closed = safehermit_command(
        evidence.path(),
        "default-policy-refusal",
        &common_run_arguments(0, 1_000_000),
        fixture,
        &["client", &closed_port, "match"],
    );
    assert_fail_closed(
        &closed,
        "default no-network policy",
        &["network", "disabled"],
    );
    closed_controller.stop_without_connection();

    // Policy 2 recording: the only live-network execution is explicit, bounded,
    // and backed by an external controller with an exact outbound oracle.
    let record_directory = evidence.path().join("record-controller");
    fs::create_dir(&record_directory).expect("create record controller directory");
    let (controller, port) = Controller::start(fixture, &record_directory);
    let mut record_arguments = common_run_arguments(0, 1_000_000);
    record_arguments.push("--unsafe-live-network".into());
    record_arguments.push(format!("--record-networking={}", trace.display()));
    let recorded = safehermit_command(
        evidence.path(),
        "record-networking",
        &record_arguments,
        fixture,
        &["client", &port, "match"],
    );
    assert_success(&recorded, "network recording");
    assert_guest_invariants(&recorded.stdout, "network recording");
    assert_controller_report(&controller.finish());
    let original_trace = fs::read(&trace).expect("recording omitted its network trace");
    assert!(
        !original_trace.is_empty(),
        "recording wrote an empty network trace"
    );

    // Policy 2 replay: there is no server now. Each tuple verifies twice under
    // one fixed seed, while the bounded population must expose at least two
    // reader winners across seeds/timeslices without changing external input.
    let mut reader_markers = BTreeSet::new();
    for (seed, max_timeslice) in REPLAY_CELLS {
        let label = format!("replay-seed-{seed}-timeslice-{max_timeslice}");
        let report = evidence.path().join(format!("{label}.verify.json"));
        let mut arguments = common_run_arguments(*seed, *max_timeslice);
        arguments.extend([
            "--verify".into(),
            "--verify-strict".into(),
            format!("--verify-json={}", report.display()),
            format!("--replay-networking={}", trace.display()),
        ]);
        let replayed = safehermit_command(
            evidence.path(),
            &label,
            &arguments,
            fixture,
            &["client", &port, "match"],
        );
        assert_success(&replayed, &label);
        reader_markers.insert(assert_guest_invariants(&replayed.stdout, &label));
        assert_l2_report(&report, &label);
        assert_eq!(
            fs::read(&trace).expect("replay removed its network trace"),
            original_trace,
            "{label} mutated the fixed network input"
        );
    }
    assert!(
        reader_markers.len() >= 2,
        "fixed seeds/timeslices exposed no schedule variation: {reader_markers:?}"
    );

    // A changed outbound byte must terminate replay instead of consulting the
    // network or silently accepting a different request stream.
    let mut mismatch_arguments = common_run_arguments(0, 1_000_000);
    mismatch_arguments.push(format!("--replay-networking={}", trace.display()));
    let mismatch = safehermit_command(
        evidence.path(),
        "outbound-mismatch",
        &mismatch_arguments,
        fixture,
        &["client", &port, "mismatch"],
    );
    assert_fail_closed(
        &mismatch,
        "outbound stream mismatch",
        &["network", "outbound", "mismatch"],
    );

    // Missing replay input is independently fail-closed and must fail before a
    // guest can attempt the fresh host connection.
    let missing_trace = evidence.path().join("missing.trace");
    let mut missing_arguments = common_run_arguments(0, 1_000_000);
    missing_arguments.push(format!("--replay-networking={}", missing_trace.display()));
    let missing = safehermit_command(
        evidence.path(),
        "missing-trace",
        &missing_arguments,
        fixture,
        &["client", &port, "match"],
    );
    assert_fail_closed(&missing, "missing network trace", &["network", "trace"]);
}
