//! Integration smoke test for the deliberate `--unsafe-no-cgroups` opt-out.
//!
//! The flag skips scope setup even on hosts where containment is available. It must run
//! successfully, emit an explicit warning naming the flag, and never claim boxing is active.

use std::io::Write;
use std::process::Command;

const OK_DAG: &str = r#"{"steps": [{"group": "g", "job": "ok", "cmd": "echo hello-from-step"}]}"#;

#[test]
fn unsafe_no_cgroups_deliberately_skips_boxing() {
    let bin = env!("CARGO_BIN_EXE_dagrun");

    let dir = std::env::temp_dir().join(format!("dagrun_unsafe_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("ok.json");
    let mut f = std::fs::File::create(&dag).unwrap();
    f.write_all(OK_DAG.as_bytes()).unwrap();
    drop(f);

    // The deliberate opt-out short-circuits regardless of whether boxing could be established, so
    // (unlike boxing_smoke) this needs no environment-availability skip. --no-profile keeps the
    // default auto-logging profile store from writing into the test CWD.
    let output = Command::new(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-q",
            "--no-profile",
            "--unsafe-no-cgroups",
        ])
        .output()
        .expect("failed to spawn the built binary");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        code,
        Some(0),
        "--unsafe-no-cgroups run should exit 0 (runs unboxed); got {code:?}\n{stderr}"
    );
    assert!(
        stderr.contains("DELIBERATELY UNBOXED via --unsafe-no-cgroups"),
        "expected a LOUD reviewable opt-out warning naming the flag:\n{stderr}"
    );
    assert!(
        !stderr.contains("cgroup boxing ACTIVE"),
        "the deliberate opt-out must never claim boxing is ACTIVE:\n{stderr}"
    );
}
