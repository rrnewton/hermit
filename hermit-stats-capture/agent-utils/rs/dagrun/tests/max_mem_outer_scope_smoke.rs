//! Integration smoke test: `--max-mem` reaches the OUTER SCOPE, not only the sizing model.
//!
//! `--max-mem 20G` used to feed one thing — the modelled active-step ceiling. The outer systemd
//! scope was still created with 90% of `MemAvailable`, so on a large host a run that announced a
//! 20 GiB budget could grow to most of the machine before anything stopped it, and "two validates
//! with 20 GiB each" was a property of the arithmetic rather than of the host.
//!
//! The unit tests pin the ceiling RULE (smallest of availability, environment, request). Two
//! separate things are pinned here, and the difference between them is the whole point:
//!
//! * [`max_mem_is_the_outer_scope_ceiling_the_run_announces`] pins the ANNOUNCEMENT — the stderr
//!   sentence that names the binding ceiling. A sentence is not enforcement; on its own it is
//!   satisfied by a run that prints the budget and then creates a 90%-of-host scope anyway.
//! * [`max_mem_is_the_memory_max_handed_to_systemd_run`] pins the ENFORCEMENT: the actual
//!   `MemoryMax=` property in the argument vector this binary hands to `systemd-run`. That is
//!   the mutation "scope bring-up recomputes its own ceiling and ignores the flag", which leaves
//!   both ends of the wiring correct, the announcement intact, and the containment absent.
//!
//! The first test does not need a scope to exist: the ceiling is chosen and named BEFORE the
//! re-exec is attempted. The second does not need one either, because it puts a recording
//! `systemd-run` on `PATH` and reads back exactly what the real binary asked systemd for.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const OK_DAG: &str = r#"{"steps": [{"group": "g", "job": "ok", "cmd": "true"}]}"#;

/// 1 MiB: far below 90% of MemAvailable on any host that can run this test, so the request is
/// unambiguously the binding ceiling and the expected line is deterministic.
const TINY_BUDGET_BYTES: i64 = 1024 * 1024;

/// Build a controlled fresh-top-level invocation. The cargo-test process may itself run below a
/// parent dagrun, so scrub inherited scope identity, one-step delegation, and ambient route knobs
/// rather than accidentally selecting a parent-owned or operator-forced route.
fn top_level_command(bin: &str) -> Command {
    let mut command = Command::new(bin);
    for name in [
        "DAGRUN_IN_SCOPE",
        "DAGRUN_OUTER_RUN",
        "DAGRUN_DELEGATED_CGROUP",
        "DAGRUN_DELEGATED_UNBOXED",
        "DAGRUN_DIRECT_CGROUP",
        "DAGRUN_FORCE_SCOPE_ATTEMPT",
        "DAGRUN_SCOPE_UNIT",
        "DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES",
        "DAGRUN_EXPECTED_OUTER_CPU_COUNT",
    ] {
        command.env_remove(name);
    }
    command
}

#[test]
fn max_mem_is_the_outer_scope_ceiling_the_run_announces() {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let dir = std::env::temp_dir().join(format!("dagrun_maxmem_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("ok.json");
    let mut f = std::fs::File::create(&dag).unwrap();
    f.write_all(OK_DAG.as_bytes()).unwrap();
    drop(f);

    let with_budget = top_level_command(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-q",
            "--no-profile",
            "--max-mem",
            "1M",
        ])
        // The announcement precedes scope bring-up. Select the explicit policy-skip route so this
        // assertion cannot migrate out of an aggregate validator's delegated cgroup.
        .env("CI", "1")
        .env_remove("DAGRUN_OUTER_MEMORY_MAX_BYTES")
        .output()
        .expect("failed to spawn the built binary");
    let with_stderr = String::from_utf8_lossy(&with_budget.stderr).to_string();

    let without_budget = top_level_command(bin)
        .args(["run", "--dag", dag.to_str().unwrap(), "-q", "--no-profile"])
        .env("CI", "1")
        .env_remove("DAGRUN_OUTER_MEMORY_MAX_BYTES")
        .output()
        .expect("failed to spawn the built binary");
    let without_stderr = String::from_utf8_lossy(&without_budget.stderr).to_string();

    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        with_stderr.contains(&format!(
            "--max-mem is the outer scope ceiling: MemoryMax={TINY_BUDGET_BYTES} bytes."
        )),
        "the requested budget must become the scope's MemoryMax and be named:\n{with_stderr}"
    );
    // Bracketed the other way: without the flag the run says nothing about a requested ceiling,
    // so the line is evidence of the flag rather than boilerplate printed on every run.
    assert!(
        !without_stderr.contains("--max-mem is the outer scope ceiling"),
        "a run with no --max-mem must not claim a requested ceiling:\n{without_stderr}"
    );
}

#[test]
fn a_nonpositive_max_mem_is_refused_by_name_and_no_step_runs() {
    // WHAT `--max-mem 0` ACTUALLY DOES, end to end. The ceiling helper refuses a non-positive
    // request, but the CLI never hands it one: the flag is dropped at scope bring-up and refused
    // by `select_max_steps` instead, so that one bad spec has ONE exit path. That is worth an
    // executable check rather than a comment, because the comment that used to describe this
    // said the opposite ("a non-positive request is REFUSED, not dropped" — at the CLI it is
    // dropped, and then the run is refused for a different reason).
    //
    // The two properties that actually matter to a caller are asserted: the run is REFUSED by
    // name, and it refuses BEFORE running anything — a budget of 0 must never be quietly
    // upgraded to 90% of the host and then executed.
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let dir = std::env::temp_dir().join(format!("dagrun_maxmem_zero_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("ok.json");
    let marker = dir.join("ran");
    let mut f = std::fs::File::create(&dag).unwrap();
    f.write_all(
        format!(
            r#"{{"steps": [{{"group": "g", "job": "ok", "cmd": "touch {}"}}]}}"#,
            marker.to_str().unwrap()
        )
        .as_bytes(),
    )
    .unwrap();
    drop(f);

    let out = top_level_command(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-q",
            "--no-profile",
            "--allow-cgroup-failure",
            "--max-mem",
            "0",
        ])
        .env_remove("DAGRUN_OUTER_MEMORY_MAX_BYTES")
        .output()
        .expect("failed to spawn the built binary");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let ran = marker.exists();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        stderr.contains("--max-mem 0: REFUSED"),
        "a non-positive budget must be refused BY NAME, not silently dropped:\n{stderr}"
    );
    assert!(!ran, "the run must refuse before any step executes");
}

/// Write an executable `name` in `dir` that appends each of its arguments to `$FAKE_ARGV_LOG`.
///
/// The runner reaches systemd through `Command::new("systemd-run")`, i.e. through `PATH`, so a
/// recording stand-in earlier on `PATH` observes the EXACT argument vector the real binary built
/// — including the `-p MemoryMax=…` property, which is the containment itself rather than a
/// sentence about it. `exec` replaces the runner with this script, so the run ends here.
fn write_recorder(dir: &std::path::Path, name: &str, exit_code: i32) {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> \"$FAKE_ARGV_LOG\"; done\n\
             exit {exit_code}\n"
        )
        .as_bytes(),
    )
    .unwrap();
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn max_mem_is_the_memory_max_handed_to_systemd_run() {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let dir = std::env::temp_dir().join(format!("dagrun_maxmem_argv_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("ok.json");
    let mut f = std::fs::File::create(&dag).unwrap();
    f.write_all(OK_DAG.as_bytes()).unwrap();
    drop(f);

    // A recording `systemd-run` (succeeds, so the capability probe says the route is available)
    // and a refusing `systemctl` (so the aggregate-slice setup touches nothing on the real user
    // session and the recorded vector is the same on a host with and without one).
    let shim = dir.join("bin");
    std::fs::create_dir_all(&shim).unwrap();
    write_recorder(&shim, "systemd-run", 0);
    write_recorder(&shim, "systemctl", 1);
    let argv_log = dir.join("argv.txt");
    let path = format!(
        "{}:{}",
        shim.to_str().unwrap(),
        std::env::var("PATH").unwrap_or_default()
    );

    let out = top_level_command(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-q",
            "--no-profile",
            "--max-mem",
            "1M",
        ])
        .env("PATH", &path)
        .env("FAKE_ARGV_LOG", &argv_log)
        // Force the scope attempt: under CI/GITHUB_ACTIONS the re-exec is skipped by policy and
        // no argument vector would ever be built.
        .env("DAGRUN_FORCE_SCOPE_ATTEMPT", "1")
        .env_remove("DAGRUN_OUTER_MEMORY_MAX_BYTES")
        .output()
        .expect("failed to spawn the built binary");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let recorded = std::fs::read_to_string(&argv_log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    let memory_max: Vec<&str> = recorded
        .lines()
        .filter(|line| line.starts_with("MemoryMax="))
        .collect();
    assert_eq!(
        memory_max,
        vec![format!("MemoryMax={TINY_BUDGET_BYTES}")],
        "the scope must be created with the REQUESTED ceiling, not a recomputed one.\n\
         recorded argv:\n{recorded}\nstderr:\n{stderr}"
    );
    // The value carried into the re-exec'd child, which the post-re-exec readback then compares
    // against the live unit, must be the same number — otherwise the readback would confirm a
    // ceiling nobody asked for.
    assert!(
        recorded.lines().any(|line| line
            == format!("--setenv=DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES={TINY_BUDGET_BYTES}")),
        "the expected-ceiling handoff must carry the same number:\n{recorded}"
    );
}
