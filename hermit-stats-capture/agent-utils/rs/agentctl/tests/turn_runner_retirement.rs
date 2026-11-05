//! `agentctl stop` on a headless turn-runner record written by the Python edition, as a process:
//! a live runner refuses with exit 69 and doctor advice and changes nothing (the Python edition's
//! "headless stop with unavailable control keeps the record and prints doctor"), a dead runner
//! without `--expected-token` gets the generation-bound retry, and the retry archives the record.

// The complete Python-shaped record below is one `json!` literal.
#![recursion_limit = "256"]

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const HERDR: &str = "/nonexistent/herdr";

fn private_directory(path: &Path) {
    DirBuilder::new().mode(0o700).create(path).unwrap();
}

/// A flat schema-1 turn-runner record with every field the Python edition writes.
fn python_record(home: &Path) -> Value {
    json!({
        "name": "worker", "token": TOKEN, "harness": "codex", "cwd": "/work/project",
        "created_at": 1_760_000_000.5, "schema": 1, "lifecycle": "running",
        "workspace_id": null, "tab_id": null, "pane_id": null, "session_agent": null,
        "session_value": "thread", "model": null, "resume": null, "arguments": [],
        "startup_warning": null, "effective_reasoning_effort": null, "error": null, "goal": null,
        "goal_delivery": null, "goal_session_id": null, "goal_command": null, "goal_messages": {},
        "goal_message_id": null, "adapter": "turn-runner", "mode": "headless", "backend": "tmux",
        "paused": false, "runtime_home": home, "pane_reported_by_agentctl": false,
        "custom_process_identity": null, "foreign_shell_identity": null, "terminal_id": null,
        "harness_identity": null, "name_history": [], "anchor_rule": null, "former_names": [],
        "native_session": null, "profile": null, "reasoning_effort": null, "slot": null,
        "slot_project": null, "slot_isolation": null, "environment_names": [],
    })
}

struct Registry(PathBuf);

impl Registry {
    fn new(runner_pid: u32, started_at: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agentctl-turn-runner-{}-{runner_pid}",
            std::process::id()
        ));
        private_directory(&root);
        let registry = root.join("registry");
        private_directory(&registry);
        let worker = registry.join("worker");
        private_directory(&worker);
        let home = worker.join("runtime");
        fs::create_dir_all(home.join("state/worker")).unwrap();
        let record = worker.join("agent.json");
        fs::write(
            &record,
            serde_json::to_vec_pretty(&python_record(&home)).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&record, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            home.join("registry.json"),
            serde_json::to_string_pretty(&json!([{
                "name": "worker", "harness": "codex", "backend": "tmux",
                "tmux_target": "subagents:worker", "cwd": "/work/project", "model": null,
                "session_id": "thread", "status": "idle", "runner_pid": runner_pid,
                "runner_started_at": started_at, "next_seq": 1,
                "created_at": "2026-10-01T00:00:00+00:00", "last_turn_at": null,
                "mode": "headless", "presentation_pane": null,
                "codex_bypass_permissions": false, "harness_args": [],
            }]))
            .unwrap(),
        )
        .unwrap();
        Self(root)
    }

    fn registry(&self) -> PathBuf {
        self.0.join("registry")
    }

    fn stop(&self, token: Option<&str>) -> Output {
        let registry = self.registry();
        let mut arguments = vec![
            format!("--registry={}", registry.display()),
            format!("--herdr-bin={HERDR}"),
            "stop".to_owned(),
            "worker".to_owned(),
        ];
        if let Some(token) = token {
            arguments.push(format!("--expected-token={token}"));
        }
        Command::new(env!("CARGO_BIN_EXE_agentctl"))
            .args(arguments)
            .output()
            .unwrap()
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn last_line(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .last()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn stop_refuses_a_live_runner_and_archives_a_dead_one() {
    let mut runner = Command::new("sleep").arg("30").spawn().unwrap();
    let stat = fs::read_to_string(format!("/proc/{}/stat", runner.id())).unwrap();
    let started_at = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_owned();
    let registry = Registry::new(runner.id(), &started_at);
    let record = registry.registry().join("worker/agent.json");
    let before = fs::read(&record).unwrap();
    let doctor = format!(
        "Recovery command: agentctl --registry={} --herdr-bin={HERDR} doctor",
        registry.registry().display()
    );

    let refused = registry.stop(Some(TOKEN));
    runner.kill().unwrap();
    runner.wait().unwrap();
    assert_eq!(refused.status.code(), Some(69), "{refused:?}");
    assert!(refused.stdout.is_empty(), "{refused:?}");
    assert_eq!(last_line(&refused), doctor);
    assert_eq!(fs::read(&record).unwrap(), before);
    let nested = fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .map(|pids| pids.split_whitespace().count())
        != Some(1);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    if nested {
        assert!(stderr.contains("nested PID namespace"), "{stderr}");
        return;
    }
    assert!(
        stderr.starts_with("agentctl: refusing to retire headless worker \"worker\": its runner"),
        "{stderr}"
    );
    assert!(stderr.contains("is still running"), "{stderr}");

    let unbound = registry.stop(None);
    assert_eq!(unbound.status.code(), Some(75), "{unbound:?}");
    assert_eq!(
        last_line(&unbound),
        format!(
            "Recovery command: agentctl --registry={} --herdr-bin={HERDR} stop worker --expected-token={TOKEN}",
            registry.registry().display()
        )
    );
    assert_eq!(fs::read(&record).unwrap(), before);

    let retired = registry.stop(Some(TOKEN));
    assert_eq!(retired.status.code(), Some(0), "{retired:?}");
    let receipt: Value = serde_json::from_slice(&retired.stdout).unwrap();
    let archive = registry.registry().join(format!("archive/worker-{TOKEN}"));
    assert_eq!(receipt["archive"], json!(archive));
    assert_eq!(receipt["turn_runner_dead"], true);
    assert_eq!(
        receipt["runtime"]["runtime_home"],
        json!(archive.join("runtime"))
    );
    assert_eq!(
        receipt["runtime"]["presentation_left_in_place"],
        json!({"backend": "tmux", "target": "subagents:worker", "pane": null})
    );
    assert!(!registry.registry().join("worker").exists());
    let archived: Value =
        serde_json::from_slice(&fs::read(archive.join("agent.json")).unwrap()).unwrap();
    assert_eq!(archived["lifecycle"], "stopped");
}
