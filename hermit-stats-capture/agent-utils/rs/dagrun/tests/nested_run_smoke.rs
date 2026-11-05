//! A run marks every command it starts, and a dagrun below that command refuses by default.
//!
//! The override is deliberately explicit. It permits a reviewed temporary exception, and the
//! permitted run replaces the inherited marker with its own identity for any deeper descendant.

use std::process::{Command, Output};
use std::time::{Duration, Instant};

const OUTER_RUN_ENV: &str = "DAGRUN_OUTER_RUN";
const DELEGATED_CGROUP_ENV: &str = "DAGRUN_DELEGATED_CGROUP";
const DELEGATED_UNBOXED_ENV: &str = "DAGRUN_DELEGATED_UNBOXED";

fn write_dag(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dag = dir.join(format!("{name}.json"));
    let marker = dir.join(format!("{name}.marker"));
    let document = serde_json::json!({
        "steps": [{
            "group": "g",
            "job": "j",
            "cmd": format!("printf '%s' \"$DAGRUN_OUTER_RUN\" > {}", marker.display()),
            "env": {"DAGRUN_OUTER_RUN": "forged"},
        }]
    });
    std::fs::write(&dag, serde_json::to_vec(&document).unwrap()).unwrap();
    (dag, marker)
}

fn run(bin: &str, dag: &std::path::Path, outer: Option<&str>, allow_nested: bool) -> Output {
    let mut command = Command::new(bin);
    command.args([
        "run",
        "--dag",
        dag.to_str().unwrap(),
        "--unsafe-no-cgroups",
        "--no-profile",
        "--no-profile-feedback",
        "-q",
    ]);
    if allow_nested {
        command.arg("--allow-unwise-nest-dagruns");
    }
    // The cargo-test process may itself be a delegated validation child. Each scenario in this
    // test supplies its own authority state while remaining physically inside that parent cgroup.
    command.env_remove(DELEGATED_CGROUP_ENV);
    command.env_remove(DELEGATED_UNBOXED_ENV);
    match outer {
        Some(value) => {
            command.env(OUTER_RUN_ENV, value);
        }
        None => {
            command.env_remove(OUTER_RUN_ENV);
        }
    }
    command.output().expect("failed to spawn dagrun")
}

#[test]
fn nested_run_refuses_by_outer_run_and_override_is_explicit() {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let dir = std::env::temp_dir().join(format!("dagrun_nested_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (dag, marker) = write_dag(&dir, "inner");

    let refused = run(bin, &dag, Some("--dag outer.json"), false);
    let refused_text = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "{refused_text}");
    assert!(
        refused_text.contains("refusing nested invocation"),
        "{refused_text}"
    );
    assert!(refused_text.contains("--dag outer.json"), "{refused_text}");
    assert!(
        refused_text.contains("--allow-unwise-nest-dagruns"),
        "{refused_text}"
    );
    assert!(!marker.exists(), "a refused nested run launched its step");

    let forged = Command::new(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ])
        .env(OUTER_RUN_ENV, "outer.forged")
        .env(DELEGATED_CGROUP_ENV, dir.join("not-a-cgroup"))
        .env(DELEGATED_UNBOXED_ENV, "1")
        .output()
        .expect("failed to spawn forged delegation check");
    let forged_text = String::from_utf8_lossy(&forged.stderr);
    assert_eq!(forged.status.code(), Some(2), "{forged_text}");
    assert!(
        forged_text.contains("was present but invalid"),
        "{forged_text}"
    );

    let malformed_unboxed = Command::new(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ])
        .env(OUTER_RUN_ENV, "outer.malformed")
        .env_remove(DELEGATED_CGROUP_ENV)
        .env(DELEGATED_UNBOXED_ENV, "not-authorized")
        .output()
        .expect("failed to spawn malformed unboxed delegation check");
    let malformed_text = String::from_utf8_lossy(&malformed_unboxed.stderr);
    assert_eq!(malformed_unboxed.status.code(), Some(2), "{malformed_text}");
    assert!(
        malformed_text.contains("must be exactly '1'"),
        "{malformed_text}"
    );

    let allowed = run(bin, &dag, Some("--dag outer.json"), true);
    let allowed_text = String::from_utf8_lossy(&allowed.stderr);
    assert_eq!(allowed.status.code(), Some(0), "{allowed_text}");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "g.j",
        "the allowed inner run must identify itself to deeper descendants"
    );

    std::fs::remove_file(&marker).unwrap();
    let top_level = run(bin, &dag, None, false);
    let top_level_text = String::from_utf8_lossy(&top_level.stderr);
    assert_eq!(top_level.status.code(), Some(0), "{top_level_text}");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "g.j",
        "an ordinary top-level run must execute and mark its descendants"
    );

    let outer = dir.join("outer.json");
    let outer_document = serde_json::json!({
        "steps": [{
            "group": "g",
            "job": "nested",
            "cmd": format!(
                "{} run --dag {} --unsafe-no-cgroups --no-profile --no-profile-feedback",
                bin,
                dag.display()
            ),
        }]
    });
    std::fs::write(&outer, serde_json::to_vec(&outer_document).unwrap()).unwrap();
    let nested = Command::new(bin)
        .args([
            "run",
            "--dag",
            outer.to_str().unwrap(),
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
            "-v",
        ])
        .env_remove(OUTER_RUN_ENV)
        .env_remove(DELEGATED_CGROUP_ENV)
        .env_remove(DELEGATED_UNBOXED_ENV)
        .output()
        .expect("failed to spawn outer dagrun");
    let nested_text = format!(
        "{}{}",
        String::from_utf8_lossy(&nested.stdout),
        String::from_utf8_lossy(&nested.stderr)
    );
    assert_eq!(nested.status.code(), Some(1), "{nested_text}");
    assert!(
        nested_text.contains("refusing nested invocation"),
        "the actual child dagrun did not refuse:\n{nested_text}"
    );
    assert!(
        nested_text.contains("g.nested"),
        "the refusal did not name the outer run step:\n{nested_text}"
    );

    std::fs::remove_file(&marker).unwrap();
    let delegated_outer = dir.join("delegated-unboxed-outer.json");
    let delegated_outer_document = serde_json::json!({
        "steps": [{
            "group": "g",
            "job": "nested",
            "delegated_children": true,
            "cmd": format!(
                "{} run --dag {} --no-profile --no-profile-feedback -q",
                bin,
                dag.display()
            ),
        }]
    });
    std::fs::write(
        &delegated_outer,
        serde_json::to_vec(&delegated_outer_document).unwrap(),
    )
    .unwrap();
    let delegated = Command::new(bin)
        .args([
            "run",
            "--dag",
            delegated_outer.to_str().unwrap(),
            "--allow-cgroup-failure",
            "--no-profile",
            "--no-profile-feedback",
            "-v",
        ])
        .env_remove(OUTER_RUN_ENV)
        .env_remove(DELEGATED_CGROUP_ENV)
        .env_remove(DELEGATED_UNBOXED_ENV)
        .env("CI", "1")
        .output()
        .expect("failed to spawn explicitly delegated unboxed outer dagrun");
    let delegated_text = format!(
        "{}{}",
        String::from_utf8_lossy(&delegated.stdout),
        String::from_utf8_lossy(&delegated.stderr)
    );
    assert_eq!(delegated.status.code(), Some(0), "{delegated_text}");
    assert!(
        delegated_text.contains("reviewed uncontained nested execution"),
        "the unboxed nested run did not name its degraded lane:\n{delegated_text}"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "g.j");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delegated_nested_run_keeps_descendants_in_the_outer_owned_subtree() {
    if matches!(std::env::var(DELEGATED_UNBOXED_ENV).as_deref(), Ok("1"))
        && std::env::var_os(OUTER_RUN_ENV).is_some()
    {
        eprintln!(
            "SKIP delegated_nested_run_keeps_descendants_in_the_outer_owned_subtree: parent \
             validation is explicitly unboxed; no kernel subtree exists to verify"
        );
        return;
    }
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let dir = std::env::temp_dir().join(format!("dagrun_delegated_nested_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pid_marker = dir.join("descendant.pid");
    let cgroup_marker = dir.join("descendant.cgroup");
    let coordinator_cgroup_marker = dir.join("coordinator.cgroup");
    let inner = dir.join("inner.json");
    let inner_document = serde_json::json!({
        "steps": [{
            "group": "inner",
            "job": "worker",
            "cmd": format!(
                "cat /proc/$PPID/cgroup > {} && cat /proc/self/cgroup > {} && exec setsid --wait sh -c 'echo $$ > {}; exec sleep 60'",
                coordinator_cgroup_marker.display(),
                cgroup_marker.display(),
                pid_marker.display(),
            ),
            "timeout": 60,
        }]
    });
    std::fs::write(&inner, serde_json::to_vec(&inner_document).unwrap()).unwrap();

    let outer = dir.join("outer.json");
    let outer_document = serde_json::json!({
        "steps": [{
            "group": "outer",
            "job": "delegating",
            "cmd": format!(
                "{} run --dag {} --max-cpus 1 --no-profile --no-profile-feedback -q",
                bin,
                inner.display()
            ),
            "delegated_children": true,
            "timeout": 3,
            "cpu_timeout": 60,
            "hint": {
                "preferred_inner_jobs": 1,
                "rss_baseline_bytes": 268435456,
                // Keep this smoke below the canonical validation shard's own delegated cap.
                "hard_mem_max_bytes": 1073741824_i64,
            },
        }]
    });
    std::fs::write(&outer, serde_json::to_vec(&outer_document).unwrap()).unwrap();

    let output = Command::new(bin)
        .args([
            "run",
            "--dag",
            outer.to_str().unwrap(),
            "--max-cpus",
            "1",
            "--no-profile",
            "--no-profile-feedback",
            "-q",
        ])
        .output()
        .expect("failed to spawn outer dagrun");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.code() == Some(3)
        && (text.contains("cgroup boxing could not be established")
            || text.contains("cgroup setup failed")
            || text.contains("cgroup boxing was NOT ESTABLISHED"))
    {
        eprintln!("SKIP delegated nested cgroup smoke: {text}");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains("TIMEOUT"), "{text}");
    let cgroup = std::fs::read_to_string(&cgroup_marker)
        .unwrap_or_else(|error| panic!("inner worker never published cgroup: {error}\n{text}"));
    assert!(
        cgroup.contains("step-outer.delegating/nested-run-")
            && cgroup.contains("/step-inner.worker"),
        "inner worker was not placed below the outer-owned step root: {cgroup}\n{text}"
    );
    let coordinator_cgroup =
        std::fs::read_to_string(&coordinator_cgroup_marker).unwrap_or_else(|error| {
            panic!("inner scheduler cgroup was never published: {error}\n{text}")
        });
    assert!(
        coordinator_cgroup.contains("step-outer.delegating/nested-run-")
            && coordinator_cgroup.trim_end().ends_with("/supervisor"),
        "inner scheduler was not placed inside its capped aggregate root: \
         {coordinator_cgroup}\n{text}"
    );
    let pid: u32 = std::fs::read_to_string(&pid_marker)
        .unwrap_or_else(|error| panic!("setsid descendant never published pid: {error}\n{text}"))
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::path::Path::new("/proc").join(pid.to_string()).exists() && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !std::path::Path::new("/proc").join(pid.to_string()).exists(),
        "outer timeout left delegated setsid descendant {pid} alive\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
