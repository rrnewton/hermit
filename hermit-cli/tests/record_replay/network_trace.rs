/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Network record/replay acceptance: `hermit run --record-networking` against
//! a loopback controller, then offline `--replay-networking` under several
//! schedules, each verified at L2; and a real curl fetch from a loopback HTTP
//! server, recorded, then replayed offline with the record and replay logs
//! compared by `hermit log-diff`. Ad-hoc runs set `HERMIT_SAFEHERMIT` to the
//! dev-hermit `bin/safehermit` wrapper; without it Hermit runs under the same
//! `timeout` bound as the rest of this file.

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

const CONTROLLER_WALL_SECONDS: u64 = 32;
const NATIVE_CLIENT_WALL_SECONDS: u64 = 10;
const HERMIT_WALL_SECONDS: u64 = 30;
const OUTER_WALL_SECONDS: u64 = 45;

// A small, predeclared population: one scheduler seed at a time, crossing a
// timeslice boundary, without turning this into a broad schedule campaign.
const REPLAY_CELLS: &[(u64, u64)] = &[
    (0, 1_000_000),
    (1, 1_000_000),
    (2, 4_000_000),
    (3, 4_000_000),
];

const OUTBOUND_HEX: &str = "726571756573740a6e6578740a646f6e650a";

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(14_695_981_039_346_656_037, |digest, byte| {
            (digest ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
        })
}

fn expected_invariant() -> String {
    format!(
        "aggregate=abcdef readiness=pollin,pollin eof=1 outbound_hex={OUTBOUND_HEX} outbound_fnv1a64={:016x}",
        fnv1a64(b"request\nnext\ndone\n")
    )
}

/// Checks the schedule-independent output and returns the reader marker,
/// which names the thread that won the race for the first input.
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
        "{label} must publish one competing-reader marker:\n{text}"
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

/// The fixture's loopback TCP server, which checks the exact outbound stream.
struct Controller {
    child: Option<Child>,
    contact_path: PathBuf,
    report_path: PathBuf,
}

impl Controller {
    fn start(program: &Path, directory: &Path) -> (Self, String) {
        let port_path = directory.join("controller.port");
        let contact_path = directory.join("controller.contact");
        let report_path = directory.join("controller.report");
        let child = Command::new("timeout")
            .args(["--kill-after=1s", &format!("{CONTROLLER_WALL_SECONDS}s")])
            .arg(program)
            .arg("controller")
            .arg(&port_path)
            .arg(&report_path)
            .arg(&contact_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start bounded TCP controller");
        let mut controller = Self {
            child: Some(child),
            contact_path,
            report_path,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(port) = fs::read_to_string(&port_path) {
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

    /// Waits for the protocol to complete and returns the controller's report.
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

    /// Stops a controller that no client may have reached.
    fn stop_without_connection(mut self) {
        let child = self.child.as_mut().expect("controller child exists");
        Self::request_stop(child).expect("failed to stop unused TCP controller");
        let output = self.finish_child();
        assert!(
            !output.status.success(),
            "an unused controller must be stopped rather than complete successfully"
        );
        assert!(
            !self.contact_path.exists(),
            "the guest reached the controller's accept boundary"
        );
    }

    fn request_stop(child: &mut Child) -> std::io::Result<()> {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        // TERM the unreaped timeout supervisor, which forwards it and then
        // kills the controller after --kill-after. Never signal a PID after
        // Child has reaped it.
        let pid = i32::try_from(child.id()).map_err(std::io::Error::other)?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .map_err(std::io::Error::from)
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
            let _ = Self::request_stop(child);
            let _ = child.wait();
        }
    }
}

fn assert_controller_report(report: &str) {
    let expected = format!(
        "controller=complete\noutbound_hex={OUTBOUND_HEX}\noutbound_fnv1a64={:016x}\n",
        fnv1a64(b"request\nnext\ndone\n")
    );
    assert_eq!(
        report, expected,
        "controller observed a changed outbound stream"
    );
}

/// Runs Hermit with `arguments`, through `HERMIT_SAFEHERMIT` when it is set,
/// and keeps its stdout and stderr in `evidence`.
fn hermit_command(
    evidence: &Path,
    label: &str,
    arguments: &[String],
    guest: &Path,
    guest_arguments: &[&str],
) -> Output {
    let mut command = Command::new("timeout");
    command.args(["--kill-after=5s", &format!("{OUTER_WALL_SECONDS}s")]);
    if let Some(safehermit) = std::env::var_os("HERMIT_SAFEHERMIT") {
        command
            .arg(safehermit)
            .args(["--sh-deadline", &HERMIT_WALL_SECONDS.to_string()]);
    }
    command
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(arguments)
        .arg("--")
        .arg(guest)
        .args(guest_arguments);
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {error}"));
    fs::write(evidence.join(format!("{label}.stdout")), &output.stdout)
        .unwrap_or_else(|error| panic!("failed to retain {label} stdout: {error}"));
    fs::write(evidence.join(format!("{label}.stderr")), &output.stderr)
        .unwrap_or_else(|error| panic!("failed to retain {label} stderr: {error}"));
    output
}

fn run_arguments(seed: u64, max_timeslice: u64) -> Vec<String> {
    vec![
        "--backend=ptrace".into(),
        "--log=info".into(),
        "run".into(),
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

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with {}\n{}",
        output.status,
        output_text(output)
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

/// Requires a prompt failure whose output contains `required`. Exit 124 or
/// 137 means a `timeout` bound fired instead.
fn assert_fails_naming(output: &Output, label: &str, required: &str) {
    let text = output_text(output);
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded\n{text}"
    );
    assert!(
        !matches!(output.status.code(), Some(124 | 137)),
        "{label} reached a wall-clock bound instead of failing promptly\n{text}"
    );
    assert!(
        text.contains(required),
        "{label} did not report {required:?}:\n{text}"
    );
}

#[test]
fn network_replay_tcp_fixture_has_the_exact_native_contract() {
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let evidence = tempfile::tempdir().expect("create native network bracket directory");
    let (controller, port) = Controller::start(fixture, evidence.path());
    let output = bounded_command(
        fixture,
        &[OsStr::new("client"), OsStr::new(&port), OsStr::new("match")],
        NATIVE_CLIENT_WALL_SECONDS,
    );
    assert_success(&output, "native TCP bracket client");
    assert_guest_invariants(&output.stdout, "native TCP bracket client");
    assert_controller_report(&controller.finish());
}

#[test]
fn tcp_recording_replays_offline_across_schedules_and_refuses_mismatch() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let directory = tempfile::tempdir().expect("create network replay evidence directory");
    let evidence = directory.path();
    let trace = evidence.join("network.trace");

    // With no networking flag, run behaves as it always has: the guest's
    // private network namespace cannot reach the host's loopback listener.
    let unused = evidence.join("no-flag-controller");
    fs::create_dir(&unused).expect("create no-flag controller directory");
    let (unused_controller, unused_port) = Controller::start(fixture, &unused);
    let no_flag = hermit_command(
        evidence,
        "no-networking-flag",
        &run_arguments(0, 1_000_000),
        fixture,
        &["client", &unused_port, "match"],
    );
    assert_fails_naming(&no_flag, "run without a networking flag", "connect client");
    unused_controller.stop_without_connection();

    // Record against the live controller, which checks the exact stream.
    let record_directory = evidence.join("record-controller");
    fs::create_dir(&record_directory).expect("create record controller directory");
    let (controller, port) = Controller::start(fixture, &record_directory);
    let mut record_arguments = run_arguments(0, 1_000_000);
    record_arguments.push(format!("--record-networking={}", trace.display()));
    let recorded = hermit_command(
        evidence,
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

    // Replay with no server. Each cell verifies twice under one fixed seed;
    // across cells the race winner must change while the input does not.
    let mut reader_markers = BTreeSet::new();
    for (seed, max_timeslice) in REPLAY_CELLS {
        let label = format!("replay-seed-{seed}-timeslice-{max_timeslice}");
        let report = evidence.join(format!("{label}.verify.json"));
        let mut arguments = run_arguments(*seed, *max_timeslice);
        arguments.extend([
            "--verify".into(),
            "--verify-strict".into(),
            format!("--verify-json={}", report.display()),
            format!("--replay-networking={}", trace.display()),
        ]);
        let replayed = hermit_command(
            evidence,
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
            "{label} changed the network trace"
        );
    }
    assert!(
        reader_markers.len() >= 2,
        "the replay seeds and timeslices exposed no schedule variation: {reader_markers:?}"
    );

    // A changed outbound byte ends replay rather than accepting a different
    // request stream.
    let mut mismatch_arguments = run_arguments(0, 1_000_000);
    mismatch_arguments.push(format!("--replay-networking={}", trace.display()));
    let mismatch = hermit_command(
        evidence,
        "outbound-mismatch",
        &mismatch_arguments,
        fixture,
        &["client", &port, "mismatch"],
    );
    assert_fails_naming(
        &mismatch,
        "outbound stream mismatch",
        "network outbound mismatch",
    );

    // A replay that ends with part of the recording unsent, or that sends
    // through a call the engine does not model, is refused with a remedy.
    let replay_refusals = [
        (
            "truncated",
            "network replay ended after",
            "diverged from the recording",
        ),
        (
            "sendmsg",
            "does not model sendmsg on a recorded socket",
            "--network=host",
        ),
        (
            "sendfile",
            "does not model sendfile on a recorded socket",
            "--network=host",
        ),
    ];
    for (mode, reason, remedy) in replay_refusals {
        let label = format!("replay-refuses-{mode}");
        let mut arguments = run_arguments(0, 1_000_000);
        arguments.push(format!("--replay-networking={}", trace.display()));
        let refused = hermit_command(
            evidence,
            &label,
            &arguments,
            fixture,
            &["client", &port, mode],
        );
        assert_fails_naming(&refused, &label, reason);
        assert_fails_naming(&refused, &label, remedy);
    }

    // Record refuses, before touching the host, anything that would reach
    // the network outside an outbound TCP channel. No server is listening.
    let record_refusals = [
        (
            "udp",
            "does not model sendto on an IPv4 or IPv6 socket that is not a connected TCP client",
            "--network=host",
        ),
        (
            "listen",
            "does not model bind on an IPv4 or IPv6 socket",
            "--network=host",
        ),
        ("unspecified", "names no single host", "nonzero port"),
    ];
    for (mode, reason, remedy) in record_refusals {
        let label = format!("record-refuses-{mode}");
        let mut arguments = run_arguments(0, 1_000_000);
        arguments.push(format!(
            "--record-networking={}",
            evidence.join(format!("{label}.trace")).display()
        ));
        let refused = hermit_command(
            evidence,
            &label,
            &arguments,
            fixture,
            &["client", &port, mode],
        );
        assert_fails_naming(&refused, &label, reason);
        assert_fails_naming(&refused, &label, remedy);
    }

    // A missing trace is refused before the guest starts.
    let mut missing_arguments = run_arguments(0, 1_000_000);
    missing_arguments.push(format!(
        "--replay-networking={}",
        evidence.join("missing.trace").display()
    ));
    let missing = hermit_command(
        evidence,
        "missing-trace",
        &missing_arguments,
        fixture,
        &["client", &port, "match"],
    );
    assert_fails_naming(
        &missing,
        "missing network trace",
        "cannot open network trace",
    );
    assert!(
        !String::from_utf8_lossy(&missing.stdout).contains("aggregate="),
        "the guest ran without its network trace"
    );
}

const HTTP_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nhello world\n";

/// The host's curl, which this test runs unmodified as the guest.
fn curl() -> PathBuf {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join("curl"))
        .find(|candidate| candidate.is_file())
        .expect("curl is not on PATH; install curl to run the curl network record/replay test")
}

/// Starts the one-shot loopback HTTP server and returns it with its port.
fn start_http_server(directory: &Path) -> (Child, String) {
    let server = &super::workload("c_localhost_http_server").path;
    let port_path = directory.join("http.port");
    let response_path = directory.join("http.response");
    fs::write(&response_path, HTTP_RESPONSE).expect("write HTTP response");
    let mut child = Command::new("timeout")
        .args(["--kill-after=1s", &format!("{CONTROLLER_WALL_SECONDS}s")])
        .arg(server)
        .arg(&port_path)
        .arg(&response_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start bounded HTTP server");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(port) = fs::read_to_string(&port_path)
            && let Ok(number) = port.trim().parse::<u16>()
            && number != 0
        {
            return (child, number.to_string());
        }
        if child
            .try_wait()
            .expect("failed to inspect HTTP server")
            .is_some()
        {
            let output = child.wait_with_output().expect("collect HTTP server");
            panic!(
                "HTTP server exited before publishing its port: {}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            Instant::now() < deadline,
            "HTTP server did not publish its port within two seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn curl_arguments(log: &Path, network_flag: String) -> Vec<String> {
    vec![
        "--backend=ptrace".into(),
        "--log=info".into(),
        format!("--log-file={}", log.display()),
        "run".into(),
        "--strict".into(),
        network_flag,
    ]
}

#[test]
fn curl_recording_replays_offline_with_identical_logs() {
    let _guard = super::hermit_record_lock();
    let curl = curl();
    let directory = tempfile::tempdir().expect("create curl network replay directory");
    let evidence = directory.path();
    let trace = evidence.join("curl.trace");
    let record_log = evidence.join("record.log");
    let replay_log = evidence.join("replay.log");

    let (server, port) = start_http_server(evidence);
    let url = format!("http://127.0.0.1:{port}/");
    let curl_arguments_for =
        |log: &Path, flag: &str| curl_arguments(log, format!("{flag}={}", trace.display()));
    let recorded = hermit_command(
        evidence,
        "curl-record",
        &curl_arguments_for(&record_log, "--record-networking"),
        &curl,
        &["-sS", &url],
    );
    let server = server
        .wait_with_output()
        .expect("collect bounded HTTP server");
    assert_success(&recorded, "curl recording");
    assert!(
        server.status.success(),
        "HTTP server failed with {}\nstderr:\n{}",
        server.status,
        String::from_utf8_lossy(&server.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&recorded.stdout),
        "hello world\n",
        "curl recording did not print the served body"
    );

    // The server has served its one connection and exited, so replay can
    // only succeed from the trace.
    let replayed = hermit_command(
        evidence,
        "curl-replay",
        &curl_arguments_for(&replay_log, "--replay-networking"),
        &curl,
        &["-sS", &url],
    );
    assert_success(&replayed, "curl replay");
    assert_eq!(
        replayed.stdout, recorded.stdout,
        "curl replay printed different output from the recording"
    );

    let diff_report = evidence.join("record-vs-replay.json");
    let diff = Command::new("timeout")
        .args(["--kill-after=1s", &format!("{NATIVE_CLIENT_WALL_SECONDS}s")])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .arg("log-diff")
        .arg(format!("--json={}", diff_report.display()))
        .arg(&record_log)
        .arg(&replay_log)
        .output()
        .expect("failed to start hermit log-diff");
    assert_success(&diff, "log-diff of the curl record and replay logs");
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&diff_report).expect("log-diff omitted its JSON report"))
            .expect("log-diff JSON was malformed");
    assert_eq!(report["verdict"], "matched", "log-diff report: {report}");
    for side in ["left", "right"] {
        assert!(
            report["selected_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "log-diff compared no INFO messages on {side}: {report}"
        );
    }

    let verify_report = evidence.join("curl-replay.verify.json");
    let mut verify_arguments =
        curl_arguments_for(&evidence.join("verify.log"), "--replay-networking");
    verify_arguments.extend([
        "--verify".into(),
        "--verify-strict".into(),
        format!("--verify-json={}", verify_report.display()),
    ]);
    let verified = hermit_command(
        evidence,
        "curl-replay-verify",
        &verify_arguments,
        &curl,
        &["-sS", &url],
    );
    assert_success(&verified, "curl replay under --verify-strict");
    assert_eq!(verified.stdout, recorded.stdout);
    assert_l2_report(&verify_report, "curl replay under --verify-strict");
}
