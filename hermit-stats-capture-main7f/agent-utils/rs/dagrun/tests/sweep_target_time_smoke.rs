//! End-to-end coverage for the graph-wide target-time scaling sweep.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "dagrun-target-sweep-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn shell_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn csv_column_values<'a>(csv: &'a str, column: &str) -> Vec<&'a str> {
    let mut lines = csv.lines();
    let header = lines.next().expect("profile CSV must have a header");
    let index = header
        .split(',')
        .position(|field| field == column)
        .unwrap_or_else(|| panic!("missing {column} from profile header: {header}"));
    lines
        .map(|line| {
            line.split(',')
                .nth(index)
                .unwrap_or_else(|| panic!("missing {column} from profile row: {line}"))
        })
        .collect()
}

#[test]
fn zero_target_finishes_one_topological_pass_and_drives_jobs_flag() {
    let fixture = Fixture::new();
    let probe = fixture.path("jobs-probe.sh");
    let log = fixture.path("runs.log");
    let dag = fixture.path("dag.json");
    let profiles = fixture.path("profiles");
    std::fs::write(
        &probe,
        r#"#!/bin/sh
log=$1
name=$2
shift 2
jobs=fixed
case "${1-}" in
  --jobs=*) jobs=${1#--jobs=} ;;
  --jobs) jobs=$2 ;;
  "") ;;
  *) exit 64 ;;
esac
printf '%s:%s\n' "$name" "$jobs" >> "$log"
"#,
    )
    .unwrap();
    let probe = shell_path(&probe);
    let log_path = shell_path(&log);
    // Deliberately reverse document/dependency order. The sweep must still run root -> fixed ->
    // leaf, with every node alone, and the fixed-width node must run exactly once.
    let document = format!(
        r#"{{
  "steps": [
    {{"group":"g","job":"leaf","cmd":"sh {probe} {log_path} leaf","deps":["g.fixed"],"cmdtype":"generic-with-flag","jobs_flag":"--jobs="}},
    {{"group":"g","job":"fixed","cmd":"sh {probe} {log_path} fixed","deps":["g.root"],"jobs_flag":""}},
    {{"group":"g","job":"root","cmd":"sh {probe} {log_path} root","cmdtype":"generic-with-flag","jobs_flag":"--jobs="}}
  ]
}}"#
    );
    std::fs::write(&dag, document).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_dagrun"));
    command.current_dir(&fixture.dir).args([
        "sweep",
        "--dag",
        dag.to_str().unwrap(),
        "--target-time",
        "0",
        "--jobs",
        "1,2",
        "--unsafe-no-cgroups",
        "--perf-dir",
        profiles.to_str().unwrap(),
    ]);
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "target sweep failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("sweep pass 1 starting")
            && stdout.contains("target-time sweep complete: 1 pass(es)")
            && stdout.contains("characterizing its configured width once"),
        "missing target-sweep reporting:\n{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "root:1\nroot:2\nfixed:fixed\nleaf:1\nleaf:2\n"
    );

    let profile = std::fs::read_dir(profiles)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("step_profiles_"))
        })
        .expect("target sweep did not write a step profile CSV");
    let csv = std::fs::read_to_string(profile).unwrap();
    let header = csv.lines().next().unwrap();
    for column in [
        "sweep_id",
        "sweep_pass",
        "sweep_sample",
        "sweep_repeat",
        "sweep_width_source",
        "sweep_target_s",
        "workload_digest",
    ] {
        assert!(
            header.split(',').any(|field| field == column),
            "missing {column} metadata from:\n{header}"
        );
    }
    assert_eq!(csv.lines().count(), 6, "expected header plus five samples");
    assert!(csv_column_values(&csv, "enforcement_kind")
        .iter()
        .all(|value| *value == "unboxed"));
    assert!(csv_column_values(&csv, "runner_name")
        .iter()
        .all(|value| *value == "sweep"));

    let model = std::fs::read_dir(fixture.path("profiles"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("scaling_model_"))
        })
        .expect("target sweep did not write a derived scaling-model sidecar");
    let model_json = std::fs::read_to_string(model).unwrap();
    assert!(model_json.contains("\"step\": \"g.root\""));
    assert!(model_json.contains("\"step\": \"g.leaf\""));
}

#[test]
fn legacy_sweep_records_sweep_runner_and_unboxed_enforcement() {
    let fixture = Fixture::new();
    let probe = fixture.path("legacy-probe.sh");
    let dag = fixture.path("dag.json");
    let profiles = fixture.path("profiles");
    std::fs::write(
        &probe,
        r#"#!/bin/sh
case "${1-}" in
  --jobs=*) exit 0 ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    let probe = shell_path(&probe);
    std::fs::write(
        &dag,
        format!(
            r#"{{"steps":[{{"group":"g","job":"j","cmd":"sh {probe}","cmdtype":"generic-with-flag","jobs_flag":"--jobs="}}]}}"#
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dagrun"))
        .current_dir(&fixture.dir)
        .args([
            "sweep",
            "--dag",
            dag.to_str().unwrap(),
            "--step",
            "g.j",
            "--jobs",
            "1..2",
            "--unsafe-no-cgroups",
            "--perf-dir",
            profiles.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "legacy sweep failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let profile = std::fs::read_dir(profiles)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("step_profiles_"))
        })
        .expect("legacy sweep did not write a step profile CSV");
    let csv = std::fs::read_to_string(profile).unwrap();
    assert_eq!(csv.lines().count(), 3, "expected header plus two samples");
    assert!(csv_column_values(&csv, "enforcement_kind")
        .iter()
        .all(|value| *value == "unboxed"));
    assert!(csv_column_values(&csv, "runner_name")
        .iter()
        .all(|value| *value == "sweep"));
}
