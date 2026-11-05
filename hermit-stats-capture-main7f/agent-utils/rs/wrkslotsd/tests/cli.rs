//! End-to-end contracts for the unpublished observer command.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use wrkslotsd::canonical_sha256;

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wrkslotsd-cli-test-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create CLI scratch directory");
        Self(path)
    }

    fn events(&self) -> PathBuf {
        let path = self.0.join("EVENTS.node-a");
        fs::create_dir(&path).expect("create CLI event directory");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove CLI scratch directory");
    }
}

fn write_max_revision_import(events: &Path) {
    let core = json!({
        "schema": 1,
        "machine": "node-a",
        "sequence": 1,
        "previous_sha256": "0".repeat(64),
        "recorded_at": "2026-09-22T09:00:00+00:00",
        "kind": "state-imported",
        "payload": {
            "active": {
                "schema": 2,
                "machine": "node-a",
                "revision": u64::MAX,
                "slots": [],
            },
            "archive": {
                "schema": 2,
                "machine": "node-a",
                "revision": u64::MAX,
                "records": [],
            },
            "holds": [],
        },
    });
    let mut event = core.clone();
    event["sha256"] = Value::String(canonical_sha256(&core).expect("event digest"));
    fs::write(
        events.join("00000000000000000001.json"),
        serde_json::to_vec(&event).expect("encode event"),
    )
    .expect("write event");
}

#[test]
fn binary_rebuild_status_help_and_failure_contracts() {
    let scratch = Scratch::new();
    let events = scratch.events();
    let index = scratch.0.join("observer.sqlite");
    write_max_revision_import(&events);

    let rebuilt = Command::new(env!("CARGO_BIN_EXE_wrkslotsd"))
        .args(["rebuild", "--events-dir"])
        .arg(&events)
        .arg("--index")
        .arg(&index)
        .output()
        .expect("run rebuild");
    assert!(
        rebuilt.status.success(),
        "rebuild stderr: {}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    let rebuilt_json: Value = serde_json::from_slice(&rebuilt.stdout).expect("rebuild JSON");
    assert_eq!(rebuilt_json["active_revision"], u64::MAX.to_string());
    assert_eq!(rebuilt_json["archive_revision"], u64::MAX.to_string());
    assert_eq!(rebuilt_json["replay_count"], 1);

    let status = Command::new(env!("CARGO_BIN_EXE_wrkslotsd"))
        .args(["status", "--index"])
        .arg(&index)
        .output()
        .expect("run status");
    assert!(status.status.success());
    assert_eq!(status.stdout, rebuilt.stdout);

    let explained = Command::new(env!("CARGO_BIN_EXE_wrkslotsd"))
        .args(["explain", "--index"])
        .arg(&index)
        .args(["--slot", "slot-a"])
        .output()
        .expect("run explain for unknown slot");
    assert_eq!(explained.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&explained.stderr).contains("unknown active or pending slot"));

    let plan = Command::new(env!("CARGO_BIN_EXE_wrkslotsd"))
        .args(["plan", "--index"])
        .arg(&index)
        .output()
        .expect("run plan");
    assert!(plan.status.success());
    let plan: Value = serde_json::from_slice(&plan.stdout).expect("plan JSON");
    assert_eq!(plan["eligible"], json!([]));
    assert_eq!(plan["unknown"], json!([]));

    let help = Command::new(env!("CARGO_BIN_EXE_wrkslotsd"))
        .arg("--help")
        .output()
        .expect("run help");
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("renameat2(RENAME_NOREPLACE)"));
    assert!(help.contains("cannot emit actionable eligibility"));
    assert!(help.contains("TaskGraph claim ownership"));
    for command in ["rebuild", "status", "explain", "plan"] {
        assert!(help.contains(command));
    }

    let failure = Command::new(env!("CARGO_BIN_EXE_wrkslotsd"))
        .args(["status", "--index"])
        .arg(scratch.0.join("missing.sqlite"))
        .output()
        .expect("run failing status");
    assert_eq!(failure.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&failure.stderr).starts_with("wrkslotsd: "));
}
