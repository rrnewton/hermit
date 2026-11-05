//! Build provenance: `agentctl capabilities` reports `build = {edition, source_fp}` while both
//! executables keep their exact `--version` line, which the cross-edition differential compares.

use std::process::Command;

use serde_json::{json, Value};

#[test]
fn capabilities_report_the_build_edition_and_fingerprint() {
    let home = std::env::temp_dir().join(format!("agentctl-provenance-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .arg("capabilities")
        .current_dir(&home)
        .env("HOME", &home)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        document["build"],
        json!({"edition": "rust", "source_fp": option_env!("AGENTCTL_BUILD_SOURCE_FP")})
    );
}

#[test]
fn version_lines_are_unchanged() {
    for (binary, expected) in [
        (env!("CARGO_BIN_EXE_agentctl"), "agentctl 0.1.0\n"),
        (env!("CARGO_BIN_EXE_herdr-agent"), "herdr-agent 0.1.0\n"),
    ] {
        let output = Command::new(binary).arg("--version").output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(output.stderr.is_empty(), "{binary}");
    }
}
