/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Normal acceptance for schedule-independent network replay.
//! Official validation supplies an owned nextest cgroup capability. Explicit
//! ad-hoc execution continues to require the bounded safehermit wrapper.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
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

const OUTBOUND_HEX: &str = "726571756573740a6e6578740a646f6e650a";
const EVIDENCE_ENV: &str = "HERMIT_NETWORK_REPLAY_EVIDENCE";

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
    contact_path: PathBuf,
    report_path: PathBuf,
}

impl Controller {
    fn start(program: &Path, directory: &Path) -> (Self, String) {
        Self::start_with_mode(program, directory, "controller")
    }

    fn start_with_mode(program: &Path, directory: &Path, mode: &str) -> (Self, String) {
        let port_path = directory.join("controller.port");
        let contact_path = directory.join("controller.contact");
        let report_path = directory.join("controller.report");
        let child = Command::new("timeout")
            .args(["--kill-after=1s", &format!("{CONTROLLER_WALL_SECONDS}s")])
            .arg(program)
            .arg(mode)
            .arg(&port_path)
            .arg(&report_path)
            .arg(&contact_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start bounded TCP controller");
        let mut controller = Self {
            child: Some(child),
            port_path,
            contact_path,
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

    fn start_page_backed(program: &Path, directory: &Path) -> (Self, String) {
        Self::start_with_mode(program, directory, "page-controller")
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
        Self::request_stop(child).expect("failed to stop unused TCP controller");
        let output = self.finish_child();
        assert!(
            !output.status.success(),
            "an unused controller must be stopped rather than complete successfully"
        );
        assert!(
            !self.contact_path.exists(),
            "fail-closed run reached the external controller accept boundary"
        );
        assert!(
            !self.report_path.exists(),
            "fail-closed run unexpectedly completed the external protocol"
        );
    }

    fn request_stop(child: &mut Child) -> std::io::Result<()> {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        // This still-unreaped Child owns the timeout supervisor's PID. Send
        // TERM to that supervisor, so its existing signal forwarding and
        // --kill-after=1s also retire the real controller and inherited pipes.
        // SIGKILL here would strand that child until its own accept alarm.
        // Never signal a process group or a PID after Child has reaped it.
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

// The real GNU timeout supervisor owns a real child that keeps stdout open.
// Readiness comes from that child before cancellation; no Hermit run is needed.
fn pipe_owning_controller(directory: &Path, ignore_term: bool) -> Controller {
    use std::io::BufRead;

    let script = if ignore_term {
        "trap '' TERM; printf 'controller-ready\\n'; exec /bin/sleep 30"
    } else {
        "printf 'controller-ready\\n'; exec /bin/sleep 30"
    };
    let child = Command::new("timeout")
        .args(["--kill-after=1s", &format!("{CONTROLLER_WALL_SECONDS}s")])
        .args(["/bin/sh", "-c", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bounded pipe-owning controller");
    // Establish cancellation ownership before any fallible readiness check.
    let mut controller = Controller {
        child: Some(child),
        port_path: directory.join("controller.port"),
        contact_path: directory.join("controller.contact"),
        report_path: directory.join("controller.report"),
    };
    let mut stdout =
        std::io::BufReader::new(controller.child.as_mut().unwrap().stdout.take().unwrap());
    let mut ready = String::new();
    stdout
        .read_line(&mut ready)
        .expect("read actual child readiness");
    assert_eq!(ready, "controller-ready\n");
    controller.child.as_mut().unwrap().stdout = Some(stdout.into_inner());
    controller
}

#[test]
fn controller_cancellation_joins_supervisor_and_pipe_eof_promptly() {
    for ignore_term in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let controller = pipe_owning_controller(directory.path(), ignore_term);
        let started = Instant::now();
        controller.stop_without_connection();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "controller cancellation waited for the 30-second child (ignore_term={ignore_term})"
        );
    }
}

#[test]
fn controller_drop_joins_supervisor_and_pipe_eof_promptly() {
    use std::io::Read;

    for ignore_term in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut controller = pipe_owning_controller(directory.path(), ignore_term);
        let mut stdout = controller.child.as_mut().unwrap().stdout.take().unwrap();
        let started = Instant::now();
        drop(controller);
        let mut remainder = Vec::new();
        stdout
            .read_to_end(&mut remainder)
            .expect("join child pipe EOF");
        assert!(remainder.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "controller Drop left the 30-second pipe owner alive (ignore_term={ignore_term})"
        );
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

pub(super) fn safehermit_command(
    evidence: &Path,
    label: &str,
    hermit_arguments: &[String],
    guest: &Path,
    guest_arguments: &[&str],
) -> Output {
    if super::network_boundary::active() {
        let output =
            super::network_boundary::run(evidence, label, hermit_arguments, guest, guest_arguments);
        super::network_boundary::assert_receipt(evidence, label, None);
        return output;
    }
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
    let output = command.output().unwrap_or_else(|error| {
        panic!("failed to start {label} through the required safehermit wrapper: {error}")
    });
    fs::write(evidence.join(format!("{label}.stdout")), &output.stdout)
        .unwrap_or_else(|error| panic!("failed to retain {label} stdout: {error}"));
    fs::write(evidence.join(format!("{label}.stderr")), &output.stderr)
        .unwrap_or_else(|error| panic!("failed to retain {label} stderr: {error}"));
    output
}

pub(super) fn common_run_arguments(seed: u64, max_timeslice: u64) -> Vec<String> {
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

pub(super) fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) fn assert_l2_report(path: &Path, label: &str) {
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
        NATIVE_CLIENT_WALL_SECONDS,
    );
    assert_success(&output, "native TCP bracket client");
    assert_guest_invariants(&output.stdout, "native TCP bracket client");
    assert_controller_report(&controller.finish());
}

#[test]
fn network_replay_page_backed_fixture_has_the_exact_native_contract() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let evidence = tempfile::tempdir().expect("create native page-backed bracket directory");
    let (controller, port) = Controller::start_page_backed(fixture, evidence.path());
    let output = bounded_command(
        fixture,
        &[OsStr::new("page-client"), OsStr::new(&port)],
        NATIVE_CLIENT_WALL_SECONDS,
    );
    assert_success(&output, "native page-backed TCP bracket client");
    assert_eq!(
        std::str::from_utf8(&output.stdout).unwrap(),
        "page-bytes=65536 page-fnv1a64=8d8f5f9042a8fd22\n"
    );
    assert_eq!(
        controller.finish(),
        "page-bytes=65536\npage-fnv1a64=8d8f5f9042a8fd22\n"
    );
}

#[test]
fn tcp_controller_accept_window_outlives_provider_startup() {
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let evidence = tempfile::tempdir().expect("create delayed TCP controller directory");
    let (controller, port) = Controller::start(fixture, evidence.path());

    // The provider and dynamic loader can legitimately need more than the
    // fixture's five-second connected-socket I/O bound before the guest calls
    // connect. Cross that old listener SO_RCVTIMEO boundary explicitly: accept
    // is instead bounded by the controller's separate 30-second alarm.
    thread::sleep(Duration::from_secs(6));
    let output = bounded_command(
        fixture,
        &[OsStr::new("client"), OsStr::new(&port), OsStr::new("match")],
        NATIVE_CLIENT_WALL_SECONDS,
    );
    assert_success(&output, "delayed native TCP bracket client");
    assert_guest_invariants(&output.stdout, "delayed native TCP bracket client");
    assert_controller_report(&controller.finish());
}

#[test]
fn controller_no_contact_oracle_rejects_partial_accepted_connection() {
    use std::io::Write;
    use std::net::Shutdown;
    use std::net::TcpStream;

    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let evidence = tempfile::tempdir().expect("create controller contact directory");
    let (controller, port) = Controller::start(fixture, evidence.path());
    let mut client = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap()))
        .expect("connect violating partial client");
    client.write_all(b"x").expect("send partial contact byte");
    client.shutdown(Shutdown::Write).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !controller.contact_path.exists() {
        assert!(
            Instant::now() < deadline,
            "controller did not publish accepted-contact witness"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        controller.stop_without_connection();
    }));
    assert!(
        rejected.is_err(),
        "an accepted partial connection must invalidate the no-contact oracle"
    );
}

fn acceptance_evidence_directory() -> (Option<tempfile::TempDir>, PathBuf) {
    if let Some(path) = std::env::var_os(EVIDENCE_ENV).map(PathBuf::from) {
        fs::create_dir(&path).unwrap_or_else(|error| {
            panic!(
                "{EVIDENCE_ENV} must name a new evidence directory {}: {error}",
                path.display()
            )
        });
        return (None, path);
    }
    let temporary = tempfile::tempdir().expect("create network replay evidence directory");
    let path = temporary.path().to_owned();
    (Some(temporary), path)
}

fn accepted_recovery_argument(evidence: &Path) -> String {
    let path = evidence.join("accepted-recovery");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap_or_else(|error| {
            panic!(
                "create private accepted-provider recovery directory {}: {error}",
                path.display()
            )
        });
    format!("--network-accepted-recovery={}", path.display())
}

#[test]
fn external_tcp_recording_replays_offline_across_schedules_and_refuses_mismatch() {
    super::network_boundary::initialize("tcp");
    let _guard = super::hermit_record_lock();
    // The CLI still authenticates these roots. This test only transports an
    // explicit, complete deployment without creating or rotating its state.
    let deployment = match (
        std::env::var_os("HERMIT_NETWORK_TCP_GUARD_BPFFS"),
        std::env::var_os("HERMIT_NETWORK_TCP_GUARD_RECOVERY"),
        std::env::var_os("HERMIT_NETWORK_TCP_ACCEPTED_RECOVERY"),
    ) {
        (None, None, None) => None,
        (Some(bpffs), Some(guard_recovery), Some(accepted_recovery)) => {
            let directory = |name: &str, value: std::ffi::OsString| {
                let value = value
                    .into_string()
                    .unwrap_or_else(|_| panic!("{name} must be a UTF-8 directory path"));
                let path = Path::new(&value);
                assert!(path.is_absolute(), "{name} must be an absolute directory path");
                assert!(path.is_dir(), "{name} must name an existing directory");
                value
            };
            Some((
                directory("HERMIT_NETWORK_TCP_GUARD_BPFFS", bpffs),
                directory("HERMIT_NETWORK_TCP_GUARD_RECOVERY", guard_recovery),
                directory("HERMIT_NETWORK_TCP_ACCEPTED_RECOVERY", accepted_recovery),
            ))
        }
        _ => panic!(
            "HERMIT_NETWORK_TCP_GUARD_BPFFS, HERMIT_NETWORK_TCP_GUARD_RECOVERY, \
             and HERMIT_NETWORK_TCP_ACCEPTED_RECOVERY must be supplied together"
        ),
    };
    let run_arguments = |seed, max_timeslice| {
        let mut arguments = common_run_arguments(seed, max_timeslice);
        if let Some((bpffs, recovery, _)) = &deployment {
            arguments.extend([
                format!("--network-guard-bpffs={bpffs}"),
                format!("--network-guard-recovery={recovery}"),
            ]);
        }
        arguments
    };
    let fixture = &super::workload("c_network_replay_tcp_bracket").path;
    let (_temporary_evidence, evidence) = acceptance_evidence_directory();
    let trace = evidence.join("network.trace");
    let accepted_recovery = deployment.as_ref().map_or_else(
        || accepted_recovery_argument(&evidence),
        |(_, _, recovery)| format!("--network-accepted-recovery={recovery}"),
    );

    // Policy 1: deterministic execution without a trace cannot touch even the
    // waiting loopback controller. This also proves that refusal is prompt.
    let (closed_controller, closed_port) = Controller::start(fixture, &evidence);
    let closed = safehermit_command(
        &evidence,
        "default-policy-refusal",
        &run_arguments(0, 1_000_000),
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
    let record_directory = evidence.join("record-controller");
    fs::create_dir(&record_directory).expect("create record controller directory");
    let (controller, port) = Controller::start(fixture, &record_directory);
    let mut record_arguments = run_arguments(0, 1_000_000);
    record_arguments.push(accepted_recovery.clone());
    record_arguments.push(format!("--record-networking={}", trace.display()));
    record_arguments.push("--network-record-profile=shared-mm-v1".into());
    let recorded = safehermit_command(
        &evidence,
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
        let report = evidence.join(format!("{label}.verify.json"));
        let mut arguments = run_arguments(*seed, *max_timeslice);
        arguments.extend([
            accepted_recovery.clone(),
            "--verify".into(),
            "--verify-strict".into(),
            "--keep-logs".into(),
            format!("--verify-json={}", report.display()),
            format!("--replay-networking={}", trace.display()),
        ]);
        let replayed = safehermit_command(
            &evidence,
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
    let mut mismatch_arguments = run_arguments(0, 1_000_000);
    mismatch_arguments.push(accepted_recovery.clone());
    mismatch_arguments.push(format!("--replay-networking={}", trace.display()));
    let mismatch = safehermit_command(
        &evidence,
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
    let missing_trace = evidence.join("missing.trace");
    let mut missing_arguments = run_arguments(0, 1_000_000);
    missing_arguments.push(accepted_recovery);
    missing_arguments.push(format!("--replay-networking={}", missing_trace.display()));
    let missing = safehermit_command(
        &evidence,
        "missing-trace",
        &missing_arguments,
        fixture,
        &["client", &port, "match"],
    );
    assert_fail_closed(&missing, "missing network trace", &["network", "trace"]);
}

#[path = "network_lowat.rs"]
mod lowat;

#[test]
fn blocked_poll_replays_current_lowat_across_schedules() {
    super::network_boundary::initialize("poll");
    let _guard = super::hermit_record_lock();
    let (_temporary_evidence, evidence) = acceptance_evidence_directory();
    lowat::assert_record_replay_lowat(&evidence.join("lowat"), lowat::Case::Poll);
}

#[test]
fn blocked_receive_replays_saved_target_across_schedules() {
    super::network_boundary::initialize("recv");
    let _guard = super::hermit_record_lock();
    let (_temporary_evidence, evidence) = acceptance_evidence_directory();
    lowat::assert_record_replay_lowat(&evidence.join("lowat"), lowat::Case::Recv);
}
