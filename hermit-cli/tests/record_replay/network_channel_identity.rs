/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Endpoint identity must survive actual socket creation-order changes. The
//! same-endpoint case preserves logical connect order and consumes successive
//! trace occurrences; it does not claim that swapped application protocols match.

use std::fs;
use std::io::Write;
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const DISTINCT: &[(&str, &str)] = &[("ab", "ab"), ("ba", "ab"), ("ba", "ba")];
const SAME: &[(&str, &str)] = &[("ab", "ab"), ("ba", "ab")];
const CONTROLLER_REPORT: &str = "accepted=2\nrequests_by_peer=A!,B!\nresponses=one,two\n";

struct Controller {
    child: Option<Child>,
    report: PathBuf,
    directory: PathBuf,
}

impl Controller {
    fn start(fixture: &Path, directory: &Path, scenario: &str) -> (Self, [String; 2]) {
        fs::create_dir(directory).expect("new controller evidence directory");
        let ports = directory.join("ports");
        let report = directory.join("report");
        let child = Command::new("timeout")
            .args(["--kill-after=1s", "10s"])
            .arg(fixture)
            .arg("controller")
            .arg(scenario)
            .arg(&ports)
            .arg(&report)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch bounded controller in its own process group");
        let mut controller = Self {
            child: Some(child),
            report,
            directory: directory.to_owned(),
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(text) = fs::read_to_string(&ports) {
                let fields: Vec<_> = text.split_whitespace().map(str::to_owned).collect();
                assert_eq!(fields.len(), 2, "{text}");
                for field in &fields {
                    assert!(field.parse::<u16>().is_ok_and(|port| port != 0));
                }
                assert_eq!(fields[0] == fields[1], scenario != "distinct");
                return (controller, fields.try_into().unwrap());
            }
            assert!(
                controller
                    .child
                    .as_mut()
                    .unwrap()
                    .try_wait()
                    .unwrap()
                    .is_none(),
                "controller terminated before publishing ports"
            );
            assert!(
                Instant::now() < deadline,
                "controller startup exceeded two seconds"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn collect(&mut self) -> Output {
        let output = self.child.take().unwrap().wait_with_output().unwrap();
        fs::write(self.directory.join("stdout"), &output.stdout).unwrap();
        fs::write(self.directory.join("stderr"), &output.stderr).unwrap();
        fs::write(self.directory.join("status"), output.status.to_string()).unwrap();
        output
    }

    fn finish(mut self) {
        assert_success(&self.collect(), "controller");
        assert_eq!(fs::read_to_string(&self.report).unwrap(), CONTROLLER_REPORT);
    }

    fn stop_without_contact(mut self) {
        self.child
            .as_mut()
            .unwrap()
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"S")
            .expect("send watch stop after guest terminal");
        assert_success(&self.collect(), "no-contact controller acknowledgement");
        assert_eq!(
            fs::read_to_string(&self.report).unwrap(),
            "contact=none\n",
            "replay contacted the wrong-peer controller"
        );
    }

    fn finish_contact(mut self) {
        assert_success(&self.collect(), "contact-positive controller");
        assert_eq!(
            fs::read_to_string(&self.report).unwrap(),
            "contact=accepted\n"
        );
    }

    fn kill_group(&mut self) {
        if let Some(child) = &self.child {
            // The still-owned, unreaped child leads the group created above.
            // Kill both timeout and its fixture, including on assertion unwind.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.kill_group();
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label}: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_client(output: &Output, creation: &str, connection: &str) {
    assert_success(output, "identity client");
    assert_eq!(
        String::from_utf8(output.stdout.clone()).unwrap(),
        format!(
            "created={creation} connected={connection} payloads=one,two eof=2 requests_by_peer=A!,B!\n"
        )
    );
}

fn run_arguments() -> Vec<String> {
    [
        "--log=info",
        "run",
        "--backend=ptrace",
        "--strict",
        "--chaos",
        "--seed=0",
        "--rng-seed=0",
        "--fuzz-seed=0",
        "--sched-seed=0",
        "--sched-heuristic=random",
        "--max-timeslice=1000000",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn safehermit(
    evidence: &Path,
    label: &str,
    arguments: &[String],
    fixture: &Path,
    client: &[&str],
) -> Output {
    let wrapper = std::env::var_os("HERMIT_SAFEHERMIT")
        .expect("HERMIT_SAFEHERMIT must name the parent bounded wrapper");
    let report = evidence.join(format!("{label}.safehermit"));
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=5s", "45s"])
        .arg(wrapper)
        .arg("--sh-report")
        .arg(&report)
        .args([
            "--sh-deadline",
            "30",
            "--sh-max-log-bytes",
            "4194304",
            "--sh-disk-limit",
            "1G",
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(arguments)
        .arg("--")
        .arg(fixture)
        .args(client)
        .env("SAFEHERMIT_LOG_ROOT", evidence.join("safehermit-logs"));
    fs::write(
        evidence.join(format!("{label}.command")),
        format!("{command:?}\n"),
    )
    .unwrap();
    let started = Instant::now();
    let output = command.output().expect("launch required bounded Hermit");
    fs::write(evidence.join(format!("{label}.stdout")), &output.stdout).unwrap();
    fs::write(evidence.join(format!("{label}.stderr")), &output.stderr).unwrap();
    fs::write(evidence.join(format!("{label}.result.json")), serde_json::to_vec_pretty(&serde_json::json!({"exit_code": output.status.code(), "elapsed_seconds": started.elapsed().as_secs_f64()})).unwrap()).unwrap();
    let guard = fs::read_to_string(report).expect("safehermit omitted guard receipt");
    for required in [
        "safehermit: bound.wall=APPLIED:30s\n",
        "safehermit: bound.cgroup=APPLIED:MemoryMax=16G MemorySwapMax=0\n",
        "safehermit: bound.disk=APPLIED:1G ",
        "safehermit: bound.bytes=APPLIED:4194304 ",
        "safehermit: truncated=false\n",
    ] {
        assert!(
            guard.contains(required),
            "{label} guard omitted {required:?}:\n{guard}"
        );
    }
    assert!(
        !matches!(output.status.code(), None | Some(124 | 137)),
        "{label} hit a bound: {guard}"
    );
    output
}

fn assert_strict_report(path: &Path) {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(value["verdict"], "matched", "{value}");
    assert_eq!(value["verified"], true, "{value}");
    assert_eq!(value["bitwise_parity"], true, "{value}");
    assert_eq!(value["comparison"]["strictness"], "canonical", "{value}");
    for field in [
        "compare_logs",
        "compare_io_buffers",
        "full_trace",
        "exact_remainder",
    ] {
        assert_eq!(value["comparison"][field], true, "{field}: {value}");
    }
    for field in ["strip_lines", "ignore_lines", "skip_commit", "skip_detlog"] {
        assert_eq!(value["comparison"][field], false, "{field}: {value}");
    }
    for side in ["left", "right"] {
        assert!(
            value["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|n| n > 0),
            "{value}"
        );
    }
}

#[test]
fn native_endpoint_identity_fixture_changes_actual_socket_creation_order() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_channel_identity").path;
    let evidence = tempfile::tempdir().unwrap();
    let (watcher, _) = Controller::start(fixture, &evidence.path().join("watch-none"), "watch");
    watcher.stop_without_contact();
    let (watcher, ports) =
        Controller::start(fixture, &evidence.path().join("watch-contact"), "watch");
    let contact = TcpStream::connect(("127.0.0.1", ports[0].parse::<u16>().unwrap())).unwrap();
    watcher.finish_contact();
    drop(contact);
    for (scenario, cells) in [("distinct", DISTINCT), ("same", SAME)] {
        for (creation, connection) in cells {
            let (controller, ports) = Controller::start(
                fixture,
                &evidence
                    .path()
                    .join(format!("{scenario}-{creation}-{connection}")),
                scenario,
            );
            let output = Command::new("timeout")
                .args(["--kill-after=1s", "10s"])
                .arg(fixture)
                .args(["client", &ports[0], &ports[1], creation, connection, "0"])
                .output()
                .unwrap();
            assert_client(&output, creation, connection);
            controller.finish();
        }
    }
}

#[test]
#[ignore = "requires explicit bounded ptrace execution and a private quota-enabled evidence directory"]
fn endpoint_identity_replays_creation_orders_and_refuses_wrong_peers() {
    let _guard = super::hermit_record_lock();
    let fixture = &super::workload("c_network_channel_identity").path;
    let temporary;
    let evidence = if let Some(path) = std::env::var_os("HERMIT_NETWORK_IDENTITY_EVIDENCE") {
        let path = PathBuf::from(path);
        assert!(!path.exists(), "identity evidence must be a new path");
        fs::create_dir_all(&path).unwrap();
        path
    } else {
        temporary = tempfile::tempdir().unwrap();
        temporary.path().to_owned()
    };
    for (scenario, cells) in [("distinct", DISTINCT), ("same", SAME)] {
        let (controller, ports) = Controller::start(
            fixture,
            &evidence.join(format!("{scenario}-controller")),
            scenario,
        );
        let trace = evidence.join(format!("{scenario}.trace"));
        let mut arguments = run_arguments();
        arguments.push(format!("--record-networking={}", trace.display()));
        let recorded = safehermit(
            &evidence,
            &format!("{scenario}-record"),
            &arguments,
            fixture,
            &["client", &ports[0], &ports[1], "ab", "ab", "0"],
        );
        assert_client(&recorded, "ab", "ab");
        controller.finish();
        let trace_bytes = fs::read(&trace).expect("recording omitted the network trace");
        assert!(!trace_bytes.is_empty());
        for (creation, connection) in cells {
            let label = format!("{scenario}-replay-{creation}-{connection}");
            let verify = evidence.join(format!("{label}.verify.json"));
            let mut arguments = run_arguments();
            arguments.extend([
                format!("--replay-networking={}", trace.display()),
                "--verify".into(),
                "--verify-strict".into(),
                "--keep-logs".into(),
                format!("--verify-json={}", verify.display()),
            ]);
            let output = safehermit(
                &evidence,
                &label,
                &arguments,
                fixture,
                &["client", &ports[0], &ports[1], creation, connection, "0"],
            );
            assert_client(&output, creation, connection);
            assert_strict_report(&verify);
            assert_eq!(
                fs::read(&trace).unwrap(),
                trace_bytes,
                "replay mutated its input trace"
            );
        }
        if scenario == "distinct" {
            for (label, already_bound, diagnostic) in [
                ("fresh-wrong-peer", false, "NoMatchingChannel"),
                ("bound-wrong-peer", true, "ChannelEndpointMismatch"),
            ] {
                let (watcher, wrong_ports) = Controller::start(
                    fixture,
                    &evidence.join(format!("{label}-controller")),
                    "watch",
                );
                assert!(
                    !ports.contains(&wrong_ports[0]),
                    "wrong-peer control reused a trace endpoint"
                );
                let first = if already_bound {
                    &ports[0]
                } else {
                    &wrong_ports[0]
                };
                let wrong_bound = if already_bound {
                    wrong_ports[0].as_str()
                } else {
                    "0"
                };
                let mut arguments = run_arguments();
                arguments.push(format!("--replay-networking={}", trace.display()));
                let output = safehermit(
                    &evidence,
                    label,
                    &arguments,
                    fixture,
                    &["client", first, &ports[1], "ba", "ab", wrong_bound],
                );
                assert!(!output.status.success(), "wrong peer was admitted");
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains(diagnostic),
                    "wrong failure: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    fs::read(&trace).unwrap(),
                    trace_bytes,
                    "refusal mutated its input trace"
                );
                watcher.stop_without_contact();
            }
        }
    }
}
