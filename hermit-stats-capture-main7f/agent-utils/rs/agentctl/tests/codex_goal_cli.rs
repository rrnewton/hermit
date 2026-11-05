//! `agentctl codex-goal` as a process: one JSON line on success, and on failure nothing on
//! stdout, `native goal: REASON` on stderr and exit status 1, as the Python edition's
//! `python -m agentctl.codex_goal` behaves.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Minimal JSON-RPC goal server: answers `initialize`, then answers the goal request
/// according to its mode.
const SERVER: &str = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method": "initialized"'*) continue ;;
    *'"method": "initialize"'*) printf '%s\n' '{"id": 1, "result": {}}'; continue ;;
  esac
  case "$1" in
    error) printf '%s\n' '{"id": 2, "error": {"code": -32601, "message": "unknown goal method"}}' ;;
    *) printf '%s\n' '{"id": 2, "result": {"goal": {"threadId": "thread-fixture", "objective": "Observe progress π", "status": "active", "tokenBudget": null, "tokensUsed": 10, "timeUsedSeconds": 2, "createdAt": 100, "updatedAt": 101}}}' ;;
  esac
done
"#;

fn transport(mode: &str) -> String {
    serde_json::to_string(&["/bin/sh", "-c", SERVER, "goal-fixture", mode]).unwrap()
}

fn agentctl(arguments: &[&str]) -> Output {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "agentctl-codex-goal-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&scratch).unwrap();
    let output = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_agentctl")))
        .args(arguments)
        .current_dir(&scratch)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&scratch);
    output
}

#[test]
fn cli_writes_one_json_result() {
    let output = agentctl(&[
        "codex-goal",
        "--command-json",
        &transport("normal"),
        "get",
        "thread-fixture",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "{\"threadId\": \"thread-fixture\", \"objective\": \"Observe progress \u{3c0}\", \"status\": \"active\", \"tokenBudget\": null, \"tokensUsed\": 10, \"timeUsedSeconds\": 2, \"createdAt\": 100, \"updatedAt\": 101}\n"
    );
}

#[test]
fn cli_failure_is_nonzero_and_has_no_fake_success_json() {
    let output = agentctl(&[
        "codex-goal",
        "--command-json",
        &transport("error"),
        "get",
        "thread-fixture",
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "native goal: Codex goal RPC refused: unknown goal method\n"
    );
}

#[test]
fn cli_usage_errors_exit_two_and_registry_options_do_not_apply() {
    for arguments in [
        &["codex-goal", "get"][..],
        &["codex-goal", "get", "thread", "--timeout", "5"],
        &[
            "codex-goal",
            "--stdio",
            "--command-json",
            "[\"x\"]",
            "get",
            "thread",
        ],
    ] {
        let output = agentctl(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
    }
    let output = agentctl(&["--registry", "elsewhere", "codex-goal", "get", "thread"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("do not apply to codex-goal"));
}

#[test]
fn cli_help_documents_the_command() {
    let output = agentctl(&["codex-goal", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    for text in [
        "Usage: agentctl codex-goal [OPTIONS] <COMMAND>",
        "agentctl codex-goal get THREAD_ID",
        "native goal: REASON",
        "[default: 30]",
    ] {
        assert!(help.contains(text), "{text}: {help}");
    }
    let output = agentctl(&["--help"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("codex-goal"));
}
