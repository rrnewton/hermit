//! The compatibility `herdr-agent` entry point must keep the agentcloud session context it
//! inherited: a cloud send from inside session A is attributed to A, never journaled as the
//! human with that context silently removed.
//!
//! This drives the real binary as a subprocess, so `AGENTCLOUD_SESSION_ID` is genuinely
//! inherited from the environment rather than supplied through any constructor.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SESSION: &str = "0b7c9a2e-1f3d-4c5b-8a69-2d4e6f8a0b1c";
const AMBIENT: &str = "c0ffee00-0000-4000-8000-00000000000a";

/// Herdr stand-in with real process semantics for the viewer: `pane run` starts the pinned
/// executable in its own session and `pane process-info` reports that live process.
const FAKE_HERDR: &str = r#"#!/bin/sh
dir=$(cd "$(dirname "$0")" && pwd)
case "$1 $2" in
  "workspace get")
    printf '{"result":{"workspace":{"workspace_id":"%s","label":"subagents"}}}\n' "$3" ;;
  "tab create")
    printf '{"result":{"tab":{"tab_id":"w1:t9"},"root_pane":{"pane_id":"w1:p9","tab_id":"w1:t9","workspace_id":"w1"}}}\n' ;;
  "pane run")
    executable=${4%% *}
    setsid "$executable" 60 </dev/null >/dev/null 2>&1 &
    pid=$!
    # Return only once the viewer leads its own session: the caller kills this command's
    # process group when it exits, which must not include the viewer.
    tries=0
    while [ "$(ps -o sid= -p "$pid" | tr -d ' ')" != "$pid" ] && [ "$tries" -lt 500 ]; do
      sleep 0.01; tries=$((tries + 1))
    done
    echo "$pid" > "$dir/viewer.pid"
    echo "$executable" > "$dir/viewer.exe"
    printf '{"result":{}}\n' ;;
  "pane process-info")
    pid=$(cat "$dir/viewer.pid")
    group=$(ps -o pgid= -p "$pid" | tr -d ' ')
    printf '{"result":{"process_info":{"pane_id":"w1:p9","shell_pid":%s,"foreground_process_group_id":%s,"foreground_processes":[{"pid":%s,"argv":["%s"]}]}}}\n' \
      "$$" "$group" "$pid" "$(cat "$dir/viewer.exe")" ;;
  "pane list")
    printf '{"result":{"panes":[{"pane_id":"w1:p9","tab_id":"w1:t9","workspace_id":"w1"}]}}\n' ;;
  *)
    echo "fake herdr: unsupported $*" >&2; exit 2 ;;
esac
"#;

/// agentcloudctl stand-in that records argv and the inherited session context, and enforces
/// agentcloudctl's real guard: inside a session, human `send` may target only that session.
const FAKE_AGENTCLOUDCTL: &str = r#"#!/bin/sh
dir=$(cd "$(dirname "$0")" && pwd)
verb=$1
n=$(ls "$dir/calls" | wc -l | tr -d ' ')
for arg in "$@"; do printf '%s\0' "$arg"; done > "$dir/calls/$(printf %04d "$n")-$verb"
printf '%s' "${AGENTCLOUD_SESSION_ID-<unset>}" > "$dir/env/$(printf %04d "$n")-$verb"
if [ "$verb" = send ] && [ -n "${AGENTCLOUD_SESSION_ID:-}" ]; then
  target=""; previous=""
  for arg in "$@"; do [ "$previous" = --session ] && target=$arg; previous=$arg; done
  if [ "$target" != "$AGENTCLOUD_SESSION_ID" ]; then
    echo "agentcloudctl: refusing this send: it runs inside session $AGENTCLOUD_SESSION_ID" >&2
    exit 1
  fi
fi
case "$verb" in
  create) echo "0b7c9a2e-1f3d-4c5b-8a69-2d4e6f8a0b1c" ;;
  list) echo "[]" ;;
  send-message) echo "m-1" ;;
  send) echo "42" ;;
esac
exit 0
"#;

struct Sandbox {
    root: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(self.root.join("tools/viewer.pid")) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn install(path: &Path, body: &[u8]) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn recorded(directory: &Path) -> Vec<(String, String)> {
    let mut names = fs::read_dir(directory.join("calls"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
        .iter()
        .map(|name| {
            let argv = fs::read(directory.join("calls").join(name)).unwrap();
            let argv = String::from_utf8(argv).unwrap().replace('\0', " ");
            let session = fs::read_to_string(directory.join("env").join(name)).unwrap();
            (argv, session)
        })
        .collect()
}

#[test]
fn legacy_start_keeps_the_inherited_session_for_its_goal_brief() {
    let root = std::env::temp_dir().join(format!(
        "agentctl-legacy-attribution-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let sandbox = Sandbox { root: root.clone() };
    let tools = root.join("tools");
    let project = root.join("project");
    for directory in [tools.join("calls"), tools.join("env"), project.clone()] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    install(&tools.join("herdr"), FAKE_HERDR.as_bytes());
    install(&tools.join("agentcloudctl"), FAKE_AGENTCLOUDCTL.as_bytes());
    // The viewer must be a real ELF so its identity can be pinned; sleep stands in for it.
    install(
        &tools.join("agentterm"),
        &fs::read("/usr/bin/sleep").unwrap(),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_herdr-agent"))
        .args(["start", "worker", "--harness", "agentcloud", "--cwd"])
        .arg(&project)
        .args(["--workspace-id", "w1", "--brief", "/goal do work"])
        .arg("--herdr-bin")
        .arg(tools.join("herdr"))
        .arg("--registry")
        .arg(root.join("registry"))
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .env("AGENTCLOUD_SESSION_ID", AMBIENT)
        .env(
            "AGENTCLOUD_ORCHESTRATOR_URL",
            "wss://orchestrator.test/ws/chat",
        )
        .env_remove("HERDR_WORKSPACE_ID")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let calls = recorded(&tools);

    let delivery = calls
        .iter()
        .find(|(argv, _)| argv.contains("do work"))
        .unwrap_or_else(|| panic!("no goal delivery recorded; calls {calls:?}; stderr {stderr}"));
    assert!(
        delivery.0.starts_with("send-message "),
        "the goal brief was sent as the human instead of from {AMBIENT}: {delivery:?}"
    );
    assert!(
        delivery
            .0
            .contains(&format!("--session {AMBIENT} --to {SESSION}")),
        "{delivery:?}"
    );
    assert_eq!(
        delivery.1, AMBIENT,
        "the child lost the inherited session context"
    );
    for (argv, session) in &calls {
        assert_eq!(
            session, AMBIENT,
            "{argv} ran without the inherited session context"
        );
        assert!(
            argv.contains("--ws-url wss://orchestrator.test/ws/chat"),
            "{argv} ignored the inherited AGENTCLOUD_ORCHESTRATOR_URL"
        );
    }
    assert!(output.status.success(), "stderr: {stderr}");
    drop(sandbox);
}
