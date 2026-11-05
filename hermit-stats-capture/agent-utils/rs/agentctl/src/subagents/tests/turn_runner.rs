use super::*;
use crate::subagents::turn_runner::with_proc_root;
use std::os::unix::fs::symlink;

const RUNNER: i64 = 4242;
const STARTED: u64 = 555;

fn stat(pid: i64, starttime: u64) -> String {
    format!("{pid} (agent (runner)) S 1 {pid} {pid} 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 {starttime} 1000 10\n")
}

/// A turn-runner record with every flat schema-1 field, its runtime home with one registry
/// row, and a fake `/proc` in which the recorded runner does not exist.
struct Worker {
    fixture: Fixture,
    token: String,
    home: PathBuf,
    proc: PathBuf,
}

impl Worker {
    fn new() -> Self {
        let fixture = Fixture::new();
        let token = fixture.start(None)["token"].as_str().unwrap().to_owned();
        let path = fixture.root.join("registry/worker/agent.json");
        let home = fixture.root.join("registry/worker/runtime");
        let mut document = agent::read_private_json(&path).unwrap();
        document["adapter"] = json!("turn-runner");
        document["mode"] = json!("headless");
        document["backend"] = json!("tmux");
        document["runtime_home"] = json!(home);
        agent::atomic_json(&path, &document).unwrap();
        fs::create_dir_all(home.join("state/worker")).unwrap();
        let proc = fixture.root.join("proc");
        fs::create_dir_all(proc.join("self")).unwrap();
        fs::write(proc.join("self/status"), "Name:\tagentctl\nNSpid:\t100\n").unwrap();
        let worker = Self {
            fixture,
            token,
            home,
            proc,
        };
        worker.write_registry(json!([Self::row(Some(RUNNER), Some("555"))]));
        worker
    }

    /// A runtime registry row with all 17 worker-record keys.
    fn row(pid: Option<i64>, started_at: Option<&str>) -> Value {
        json!({
            "name": "worker",
            "harness": "codex",
            "backend": "tmux",
            "tmux_target": "subagents:worker",
            "cwd": "/work/project",
            "model": null,
            "session_id": "thread",
            "status": "idle",
            "runner_pid": pid,
            "runner_started_at": started_at,
            "next_seq": 3,
            "created_at": "2026-10-01T00:00:00+00:00",
            "last_turn_at": null,
            "mode": "headless",
            "presentation_pane": null,
            "codex_bypass_permissions": false,
            "harness_args": [],
        })
    }

    fn write_registry(&self, rows: Value) {
        fs::write(
            self.home.join("registry.json"),
            serde_json::to_string_pretty(&rows).unwrap() + "\n",
        )
        .unwrap();
    }

    fn state(&self) -> PathBuf {
        self.home.join("state/worker")
    }

    fn process(&self, pid: i64, starttime: u64) {
        fs::create_dir_all(self.proc.join(pid.to_string())).unwrap();
        fs::write(self.proc.join(format!("{pid}/stat")), stat(pid, starttime)).unwrap();
    }

    fn record(&self) -> Vec<u8> {
        fs::read(self.fixture.root.join("registry/worker/agent.json")).unwrap()
    }

    fn archive(&self) -> PathBuf {
        self.fixture
            .root
            .join(format!("registry/archive/worker-{}", self.token))
    }

    fn stop_with(&self, options: StopOptions) -> std::result::Result<Value, StopFailure> {
        with_proc_root(&self.proc, || {
            self.fixture
                .manager()
                .advised_stop_with_options("worker", options)
        })
    }

    fn stop(&self, token: Option<&str>) -> std::result::Result<Value, StopFailure> {
        self.stop_with(StopOptions {
            expected_token: token.map(str::to_owned),
            ..StopOptions::default()
        })
    }
}

#[test]
fn live_runner_refuses_with_exit_69_and_doctor_advice() {
    let worker = Worker::new();
    worker.process(RUNNER, STARTED);
    let before = worker.record();
    for token in [None, Some(worker.token.as_str())] {
        let failure = worker.stop(token).unwrap_err();
        assert_eq!(failure.error.exit_code(), 69, "{failure}");
        assert_eq!(failure.recovery, RecoveryAction::Doctor);
        let message = failure.to_string();
        assert!(
            message.contains("its runner (pid 4242, start tick 555, state S) is still running"),
            "{message}"
        );
        assert!(message.contains("herdr-subagents down worker"), "{message}");
        assert!(message.contains("are unchanged"), "{message}");
    }
    assert_eq!(worker.record(), before);
    assert!(!worker.archive().exists());
    assert!(worker.home.join("registry.json").is_file());
    assert!(worker.fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn recycled_runner_pid_is_proved_dead_and_the_record_is_archived() {
    let worker = Worker::new();
    worker.process(RUNNER, 999);
    let receipt = worker.stop(Some(&worker.token)).unwrap();
    let archive = worker.archive();
    assert_eq!(
        receipt,
        json!({
            "name": "worker",
            "archive": archive,
            "pane_closed": false,
            "tab_closed": false,
            "turn_runner_dead": true,
            "runtime": {
                "runtime_home": archive.join("runtime"),
                "state_path": archive.join("runtime/state/worker"),
                "runner_pid": 4242,
                "runner_started_at": "555",
                "evidence": "every recorded process has exited or its PID now names a newer process: its runner (pid 4242)",
                "presentation_left_in_place": {
                    "backend": "tmux",
                    "target": "subagents:worker",
                    "pane": null,
                },
            },
        })
    );
    assert!(!worker.fixture.root.join("registry/worker").exists());
    let archived = agent::read_private_json(&archive.join("agent.json")).unwrap();
    assert_eq!(archived["lifecycle"], "stopped");
    assert_eq!(archived["adapter"], "turn-runner");
    assert_eq!(archived["token"], worker.token.as_str());
    // The runtime home moved with the record and was not rewritten.
    let rows: Value =
        serde_json::from_slice(&fs::read(archive.join("runtime/registry.json")).unwrap()).unwrap();
    assert_eq!(rows, json!([Worker::row(Some(RUNNER), Some("555"))]));
    assert!(worker.fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn absent_runner_without_a_token_names_the_generation_bound_retry() {
    let worker = Worker::new();
    let before = worker.record();
    let failure = worker.stop(None).unwrap_err();
    assert_eq!(failure.error.exit_code(), 75, "{failure}");
    assert!(
        failure.to_string().contains(
            "retiring headless worker \"worker\", whose runner has exited, requires --expected-token"
        ),
        "{failure}"
    );
    assert_eq!(
        failure.recovery,
        RecoveryAction::Stop {
            name: "worker".to_owned(),
            token: worker.token.clone(),
            record_sha256: None,
            skip_cloud_halt: false,
        }
    );
    assert_eq!(worker.record(), before);
    let receipt = worker.stop(Some(&worker.token)).unwrap();
    assert_eq!(
        receipt["runtime"]["evidence"],
        "every recorded process has exited or its PID now names a newer process: its runner (pid 4242)"
    );
    assert!(worker.archive().join("agent.json").is_file());
}

#[test]
fn a_worker_without_any_recorded_runner_process_is_retired() {
    for registry in [Some(json!([])), None] {
        let worker = Worker::new();
        match registry {
            Some(rows) => worker.write_registry(rows),
            None => fs::remove_file(worker.home.join("registry.json")).unwrap(),
        }
        let receipt = worker.stop(Some(&worker.token)).unwrap();
        let evidence = receipt["runtime"]["evidence"].as_str().unwrap();
        assert!(
            evidence.ends_with("has no row for \"worker\" and no runner recorded a process ID"),
            "{evidence}"
        );
        assert_eq!(
            receipt["runtime"]["presentation_left_in_place"],
            Value::Null
        );
        assert_eq!(receipt["runtime"]["runner_pid"], Value::Null);
    }
    let worker = Worker::new();
    worker.write_registry(json!([Worker::row(None, None)]));
    fs::write(worker.state().join("runner.pid"), "777\n").unwrap();
    let receipt = worker.stop(Some(&worker.token)).unwrap();
    assert_eq!(
        receipt["runtime"]["evidence"],
        "every recorded process has exited or its PID now names a newer process: the runner named in runner.pid (pid 777)"
    );
}

#[test]
fn every_recorded_process_must_be_gone() {
    let worker = Worker::new();
    fs::write(worker.state().join("runner.pid"), "4242\n").unwrap();
    fs::write(
        worker.state().join("active-harness.json"),
        r#"{"runner_pid": 4242, "runner_started_at": "555", "pid": 6000, "started_at": "77"}"#,
    )
    .unwrap();
    fs::write(
        worker.state().join("MIGRATION_STAGED_abc.json"),
        r#"{"pid": 7000, "started_at": "88", "recorded_at": "2026-10-01T00:00:00+00:00"}"#,
    )
    .unwrap();
    // The harness PID now names a newer process; the staged runner is gone.
    worker.process(6000, 78);
    let receipt = worker.stop(Some(&worker.token)).unwrap();
    assert_eq!(
        receipt["runtime"]["evidence"],
        "every recorded process has exited or its PID now names a newer process: its runner (pid 4242), its harness (pid 6000), a staged replacement runner (pid 7000)"
    );
}

#[test]
fn inconclusive_or_live_evidence_refuses_with_exit_69_and_changes_nothing() {
    type Setup = fn(&Worker);
    let cases: Vec<(&str, Setup, &str)> = vec![
        (
            "legacy row without a start time",
            |worker| {
                worker.write_registry(json!([Worker::row(Some(RUNNER), None)]));
                worker.process(RUNNER, 1);
            },
            "its runner (pid 4242) exists and has no recorded start time",
        ),
        (
            "unreadable process identity",
            |worker| fs::create_dir_all(worker.proc.join("4242/stat")).unwrap(),
            "cannot read process 4242 identity",
        ),
        (
            "malformed process identity",
            |worker| {
                fs::create_dir_all(worker.proc.join("4242")).unwrap();
                fs::write(worker.proc.join("4242/stat"), "garbage").unwrap();
            },
            "cannot parse process 4242 identity",
        ),
        (
            "process identity for another pid",
            |worker| {
                fs::create_dir_all(worker.proc.join("4242")).unwrap();
                fs::write(worker.proc.join("4242/stat"), stat(4243, STARTED)).unwrap();
            },
            "cannot parse process 4242 identity",
        ),
        (
            "unreadable start time",
            |worker| {
                worker.write_registry(json!([Worker::row(Some(RUNNER), Some("0555"))]));
                worker.process(RUNNER, STARTED);
            },
            "its runner has an unreadable start time \"0555\"",
        ),
        (
            "row without a runner PID that may still be starting",
            |worker| {
                let mut row = Worker::row(None, None);
                row["status"] = json!("starting");
                row["created_at"] = json!("2999-01-01T00:00:00+00:00");
                worker.write_registry(json!([row]));
            },
            "its runner may still be starting",
        ),
        (
            "row without a runner PID and an unreadable creation time",
            |worker| {
                let mut row = Worker::row(None, None);
                row["created_at"] = json!("yesterday");
                worker.write_registry(json!([row]));
            },
            "a runner that is still starting cannot be ruled out",
        ),
        (
            "row without a runner PID and no creation time",
            |worker| {
                let mut row = Worker::row(None, None);
                row.as_object_mut().unwrap().remove("created_at");
                worker.write_registry(json!([row]));
            },
            "a runner that is still starting cannot be ruled out",
        ),
        (
            "corrupt runtime registry",
            |worker| fs::write(worker.home.join("registry.json"), "{").unwrap(),
            "is not valid JSON",
        ),
        (
            "runtime registry that is not an array",
            |worker| worker.write_registry(json!({"worker": {}})),
            "is not a JSON array of objects",
        ),
        (
            "duplicate rows",
            |worker| {
                let row = Worker::row(Some(RUNNER), Some("555"));
                worker.write_registry(json!([row, row]));
            },
            "has more than one row for \"worker\"",
        ),
        (
            "TUI row",
            |worker| {
                let mut row = Worker::row(None, None);
                row["mode"] = json!("tui");
                worker.write_registry(json!([row]));
            },
            "interactive TUI",
        ),
        (
            "row without a mode beside a presentation sidecar",
            |worker| {
                let mut row = Worker::row(Some(RUNNER), Some("555"));
                row.as_object_mut().unwrap().remove("mode");
                worker.write_registry(json!([row]));
                fs::write(worker.state().join("presentation.json"), "{}").unwrap();
            },
            "possible TUI",
        ),
        (
            "invalid runner pid",
            |worker| {
                let mut row = Worker::row(None, Some("555"));
                row["runner_pid"] = json!("4242");
                worker.write_registry(json!([row]));
            },
            "the runtime row has an invalid runner_pid",
        ),
        (
            "missing runtime home",
            |worker| fs::remove_dir_all(&worker.home).unwrap(),
            "does not exist, so its runner cannot be identified",
        ),
        (
            "symlinked runtime home",
            |worker| {
                let real = worker.fixture.root.join("runtime-real");
                fs::rename(&worker.home, &real).unwrap();
                symlink(&real, &worker.home).unwrap();
            },
            "is not a directory",
        ),
        (
            "nested PID namespace",
            |worker| fs::write(worker.proc.join("self/status"), "NSpid:\t100\t1\n").unwrap(),
            "nested PID namespace",
        ),
        (
            "unknown PID namespace",
            |worker| fs::write(worker.proc.join("self/status"), "Name:\tagentctl\n").unwrap(),
            "reports no PID namespace",
        ),
        (
            "live pid in runner.pid",
            |worker| {
                fs::write(worker.state().join("runner.pid"), "5000\n").unwrap();
                worker.process(5000, 1);
            },
            "the runner named in runner.pid (pid 5000) exists and has no recorded start time",
        ),
        (
            "malformed runner.pid",
            |worker| fs::write(worker.state().join("runner.pid"), "soon").unwrap(),
            "runner.pid is malformed",
        ),
        (
            "malformed harness identity",
            |worker| fs::write(worker.state().join("active-harness.json"), "{").unwrap(),
            "active-harness.json is malformed",
        ),
        (
            "harness identity without a usable pid",
            |worker| {
                fs::write(
                    worker.state().join("active-harness.json"),
                    r#"{"pid": 1, "started_at": "5"}"#,
                )
                .unwrap();
            },
            "active-harness.json has no valid pid",
        ),
        (
            "live harness",
            |worker| {
                fs::write(
                    worker.state().join("active-harness.json"),
                    r#"{"runner_pid": 4242, "runner_started_at": "555", "pid": 6000, "started_at": "77"}"#,
                )
                .unwrap();
                worker.process(6000, 77);
            },
            "its harness (pid 6000, start tick 77, state S) is still running",
        ),
        (
            "live staged replacement runner",
            |worker| {
                fs::write(
                    worker.state().join("MIGRATION_STAGED_abc.json"),
                    r#"{"pid": 7000, "started_at": "88", "recorded_at": "2026-10-01T00:00:00+00:00"}"#,
                )
                .unwrap();
                worker.process(7000, 88);
            },
            "a staged replacement runner (pid 7000, start tick 88, state S) is still running",
        ),
    ];
    for (label, setup, message) in cases {
        let worker = Worker::new();
        setup(&worker);
        let before = worker.record();
        let failure = worker.stop(Some(&worker.token)).unwrap_err();
        assert_eq!(failure.error.exit_code(), 69, "{label}: {failure}");
        assert_eq!(failure.recovery, RecoveryAction::Doctor, "{label}");
        assert!(failure.to_string().contains(message), "{label}: {failure}");
        assert_eq!(worker.record(), before, "{label}");
        assert!(!worker.archive().exists(), "{label}");
    }
}

#[test]
fn an_exclusively_locked_runtime_registry_is_inconclusive_until_released() {
    let worker = Worker::new();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(worker.home.join(".registry.lock"))
        .unwrap();
    FileExt::lock_exclusive(&lock).unwrap();
    let failure = worker.stop(Some(&worker.token)).unwrap_err();
    assert_eq!(failure.error.exit_code(), 69, "{failure}");
    assert!(failure.to_string().contains("stayed held"), "{failure}");
    drop(lock);
    assert_eq!(
        worker.stop(Some(&worker.token)).unwrap()["turn_runner_dead"],
        true
    );
}

#[test]
fn stale_tokens_and_adoption_recovery_flags_are_refused() {
    let worker = Worker::new();
    let before = worker.record();
    let token = Some(worker.token.clone());
    let digest = Some("a".repeat(64));
    for (options, message) in [
        (
            StopOptions {
                expected_token: Some("replacement".to_owned()),
                ..StopOptions::default()
            },
            "agent \"worker\" was replaced before this operation",
        ),
        (
            StopOptions {
                expected_token: token.clone(),
                retire_dead_adoption: true,
                expected_record_sha256: digest.clone(),
                ..StopOptions::default()
            },
            "dead-adoption retirement is supported only for interactive herdr-foreign records",
        ),
        (
            StopOptions {
                expected_token: token.clone(),
                recover_legacy_adoption: true,
                expected_record_sha256: digest,
                ..StopOptions::default()
            },
            "legacy adoption recovery is supported only for interactive herdr-foreign records",
        ),
        (
            StopOptions {
                expected_token: token,
                skip_cloud_halt: true,
                ..StopOptions::default()
            },
            "--skip-cloud-halt applies only to agentcloud agents",
        ),
    ] {
        let failure = worker.stop_with(options).unwrap_err();
        assert_eq!(failure.error.exit_code(), 75, "{failure}");
        assert_eq!(failure.to_string(), message);
        assert_eq!(failure.recovery, RecoveryAction::Doctor);
    }
    assert_eq!(worker.record(), before);
}

#[test]
fn status_names_the_retirement_command_instead_of_another_edition() {
    let worker = Worker::new();
    let status = worker.fixture.manager().status("worker").unwrap();
    let probe = status["probe_error"].as_str().unwrap();
    assert_eq!(
        probe,
        format!(
            "agent \"worker\" is a headless turn-runner worker (mode \"headless\", backend \"tmux\"); this agentctl does not drive turn runners. Once its runner has exited, retire it with: agentctl stop worker --expected-token={}",
            worker.token
        )
    );
    assert!(!probe.contains("Python"));
}

#[test]
fn real_process_liveness_decides_retirement() {
    let worker = Worker::new();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = i64::from(child.id());
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let starttime = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_owned();
    worker.write_registry(json!([Worker::row(Some(pid), Some(&starttime))]));
    let stop = || {
        worker.fixture.manager().advised_stop_with_options(
            "worker",
            StopOptions {
                expected_token: Some(worker.token.clone()),
                ..StopOptions::default()
            },
        )
    };
    let nested = fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .map(|pids| pids.split_whitespace().count())
        != Some(1);
    let failure = stop().unwrap_err();
    assert_eq!(failure.error.exit_code(), 69, "{failure}");
    child.kill().unwrap();
    child.wait().unwrap();
    if nested {
        // Inside a nested PID namespace absence proves nothing, so retirement stays refused.
        assert!(
            failure.to_string().contains("nested PID namespace"),
            "{failure}"
        );
        assert_eq!(stop().unwrap_err().error.exit_code(), 69);
        return;
    }
    assert!(
        failure.to_string().contains("is still running"),
        "{failure}"
    );
    assert_eq!(stop().unwrap()["turn_runner_dead"], true);
}
