//! End-to-end coverage for opt-in cgroup CPU/thread time-series persistence.

use std::collections::BTreeMap;
use std::path::PathBuf;
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
            "dagrun-profile-timeseries-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn csv_rows(text: &str) -> Vec<BTreeMap<&str, &str>> {
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    lines
        .map(|line| header.iter().copied().zip(line.split(',')).collect())
        .collect()
}

#[test]
fn boxed_run_writes_start_periodic_and_final_trace_rows() {
    let fixture = Fixture::new();
    let dag = fixture.dir.join("dag.json");
    let profiles = fixture.dir.join("profiles");
    std::fs::write(
        &dag,
        r#"{"steps":[{"group":"g","job":"trace","cmd":"sleep 0.2","timeout":30,"cpu_timeout":30,"hint":{"hard_mem_max_bytes":268435456}}]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dagrun"))
        .current_dir(&fixture.dir)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "--max-steps",
            "1",
            "--max-cpus",
            "1",
            "--no-profile-feedback",
            "--profile-timeseries",
            "50ms",
            "--perf-dir",
            profiles.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(3) {
        eprintln!(
            "SKIP boxed_run_writes_start_periodic_and_final_trace_rows: cgroup boxing is unavailable: {stderr}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "time-series run failed\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );

    let traces = profiles.join("traces");
    let paths: Vec<PathBuf> = std::fs::read_dir(&traces)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("csv"))
        .collect();
    assert_eq!(
        paths.len(),
        1,
        "expected one run-scoped trace in {traces:?}"
    );
    assert!(stderr.contains(paths[0].to_str().unwrap()));
    let text = std::fs::read_to_string(&paths[0]).unwrap();
    let header = text.lines().next().unwrap();
    assert_eq!(
        header,
        dagrun::STEP_TIMESERIES_COLUMNS.join(","),
        "unexpected trace schema"
    );
    let rows = csv_rows(&text);
    assert!(rows.len() >= 3, "expected start, periodic, and final rows");
    assert_eq!(rows[0]["sample_kind"], "start");
    assert_eq!(rows.last().unwrap()["sample_kind"], "final");
    assert!(rows.iter().any(|row| row["sample_kind"] == "periodic"));
    assert!(rows.iter().all(|row| row["step"] == "g.trace"));
    assert!(rows
        .iter()
        .all(|row| row["enforcement_kind"] == "cgroup-v2"));
    assert!(rows.iter().all(|row| row["runner_name"] == "run"));
    let run_id = rows[0]["run_id"];
    assert!(rows.iter().all(|row| row["run_id"] == run_id));
    assert_eq!(
        paths[0].file_stem().and_then(|value| value.to_str()),
        Some(run_id)
    );
}

#[test]
fn boxed_sweep_adds_sweep_provenance_to_each_trace() {
    let fixture = Fixture::new();
    let dag = fixture.dir.join("dag.json");
    let profiles = fixture.dir.join("profiles");
    std::fs::write(
        &dag,
        r#"{"steps":[{"group":"g","job":"trace","cmd":"sleep 0.15; printf '%s' $DAGRUN_EXTRA_ARGS >/dev/null","cmdtype":"generic-with-flag","jobs_flag":"--jobs","timeout":30,"cpu_timeout":30,"hint":{"hard_mem_max_bytes":268435456}}]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dagrun"))
        .current_dir(&fixture.dir)
        .args([
            "sweep",
            "--dag",
            dag.to_str().unwrap(),
            "--step",
            "g.trace",
            "--target-time",
            "0",
            "--jobs",
            "1,2",
            "--profile-timeseries",
            "50ms",
            "--perf-dir",
            profiles.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(3) {
        eprintln!(
            "SKIP boxed_sweep_adds_sweep_provenance_to_each_trace: cgroup boxing is unavailable: {stderr}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "time-series sweep failed\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );

    let mut paths: Vec<PathBuf> = std::fs::read_dir(profiles.join("traces"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("csv"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 2);
    let expected_tail = [
        "sweep_id",
        "sweep_logical_cpus",
        "sweep_mode",
        "sweep_pass",
        "sweep_physical_cores",
        "sweep_repeat",
        "sweep_sample",
        "sweep_target_s",
        "sweep_width_source",
        "workload_digest",
    ];
    let mut sweep_id = None;
    for path in paths {
        let text = std::fs::read_to_string(&path).unwrap();
        let header: Vec<&str> = text.lines().next().unwrap().split(',').collect();
        assert_eq!(
            &header[..dagrun::STEP_TIMESERIES_COLUMNS.len()],
            dagrun::STEP_TIMESERIES_COLUMNS
        );
        assert_eq!(
            &header[dagrun::STEP_TIMESERIES_COLUMNS.len()..],
            expected_tail
        );
        let rows = csv_rows(&text);
        assert!(rows.iter().all(|row| row["sweep_mode"] == "target-time"));
        assert!(rows.iter().all(|row| row["runner_name"] == "sweep"));
        assert!(rows
            .iter()
            .all(|row| row["enforcement_kind"] == "cgroup-v2"));
        let this_sweep = rows[0]["sweep_id"];
        assert!(!this_sweep.is_empty());
        match sweep_id.as_deref() {
            Some(previous) => assert_eq!(this_sweep, previous),
            None => sweep_id = Some(this_sweep.to_string()),
        }
    }
}
