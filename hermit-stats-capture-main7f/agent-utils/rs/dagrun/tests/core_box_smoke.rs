//! Regression test for fail-closed CPU pinning outside an owned runner cgroup.

use std::io::Write;
use std::process::Command;

const DAG: &str = r#"{"steps":[{"group":"box","job":"one","cmd":"echo SHOULD_NOT_RUN"}]}"#;

#[test]
fn cores_flag_refuses_unboxed_soft_affinity() {
    let dir = std::env::temp_dir().join(format!("dagrun_corebox_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("dag.json");
    std::fs::File::create(&dag)
        .unwrap()
        .write_all(DAG.as_bytes())
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dagrun"))
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "--cores",
            "1",
            "--no-profile",
            // This test is specifically about refusal outside an owned cgroup. A valid
            // delegation inherited from a parent validation step must not change its route.
            "--unsafe-no-cgroups",
        ])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3), "{combined}");
    assert!(
        combined.contains("hard cgroup cpuset unavailable; refusing to run"),
        "{combined}"
    );
    assert!(!combined.contains("SHOULD_NOT_RUN"), "{combined}");
    let _ = std::fs::remove_file(dag);
    let _ = std::fs::remove_dir(dir);
}
