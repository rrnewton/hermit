//! Integration smoke test: prove the Rust `run`'s cgroup boxing actually CAPS memory.
//!
//! Cgroup boxing is the tool's primary purpose (`ds-4wldrc`): an unboxed runner is useless. This
//! test runs the built binary on a DAG whose single step grows a bash string past a tiny per-step
//! `hard_mem_max_bytes` cap; under real two-level cgroup-v2 boxing the kernel OOM-kills the step
//! at its cap, so the run fails with an `OOM-KILLED` reason.
//!
//! Cgroup boxing is environment-dependent (a CI container may have no delegated cgroup or systemd
//! `--user` scope). When boxing genuinely cannot be established the default `run` exits 3; this
//! test then prints a LOUD, explicit skip notice (never a silent skip) and returns — the boxing
//! assertion runs wherever a working cgroup-v2 + systemd `--user` scope is available.

use std::io::Write;
use std::process::Command;

/// A step that grows a bash string until it exceeds its cgroup memory cap (bash itself is the
/// process holding the memory, so bash is the one OOM-killed -> the step's leader exits non-zero).
const OOM_DAG: &str = r#"{"steps": [{"group": "mem", "job": "hog", "desc": "allocate past cap",
  "cmd": "s=x; while true; do s=\"$s$s\"; done",
  "hint": {"hard_mem_max_bytes": 67108864}}]}"#;

fn parent_only_offers_unboxed_delegation() -> bool {
    matches!(
        std::env::var("DAGRUN_DELEGATED_UNBOXED").as_deref(),
        Ok("1")
    ) && std::env::var_os("DAGRUN_OUTER_RUN").is_some()
}

#[test]
fn boxing_oom_kills_a_step_past_its_cap() {
    if parent_only_offers_unboxed_delegation() {
        eprintln!(
            "SKIP boxing_oom_kills_a_step_past_its_cap: parent validation is explicitly unboxed; \
             refusing to run an unbounded allocator without the cgroup this test verifies"
        );
        return;
    }
    let bin = env!("CARGO_BIN_EXE_dagrun");

    let dir = std::env::temp_dir().join(format!("dagrun_smoke_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("oom.json");
    let mut f = std::fs::File::create(&dag).unwrap();
    f.write_all(OOM_DAG.as_bytes()).unwrap();
    drop(f);

    // Default `run` = boxing REQUIRED. No --allow-cgroup-failure, so if boxing is unavailable the
    // binary exits 3 and we skip loudly rather than asserting on an environment that cannot box.
    // --no-profile keeps the default auto-logging profile store from writing into the test CWD.
    let output = Command::new(bin)
        .args(["run", "--dag", dag.to_str().unwrap(), "-q", "--no-profile"])
        .output()
        .expect("failed to spawn the built binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code();
    let _ = std::fs::remove_dir_all(&dir);

    if code == Some(3) {
        eprintln!(
            "SKIP boxing_oom_kills_a_step_past_its_cap: cgroup boxing is unavailable in this \
             environment (need cgroup-v2 + a working systemd --user scope). Details:\n{stderr}"
        );
        return;
    }

    let combined = format!("{stdout}{stderr}");
    assert_eq!(
        code,
        Some(1),
        "boxed run should FAIL (exit 1) when the step is OOM-killed; got {code:?}\n{combined}"
    );
    assert!(
        combined.contains("OOM-KILLED") || combined.contains("MEMORY CAP HIT"),
        "expected an OOM-KILLED / MEMORY CAP HIT report proving the inner memory cap fired:\n{combined}"
    );
}

#[test]
fn boxed_oom_does_not_truncate_a_neighbour_artifact() {
    if parent_only_offers_unboxed_delegation() {
        eprintln!(
            "SKIP boxed_oom_does_not_truncate_a_neighbour_artifact: parent validation is \
             explicitly unboxed; the boxed isolation claim cannot be tested"
        );
        return;
    }
    const NEIGHBOUR_COUNT: usize = 4;

    let bin = env!("CARGO_BIN_EXE_dagrun");
    let dir = std::env::temp_dir().join(format!("dagrun_oom_artifact_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("oom-neighbour.json");

    let mut artifacts = Vec::new();
    let mut steps = Vec::new();
    for index in 0..NEIGHBOUR_COUNT {
        let source = dir.join(format!("neighbour-{index}.c"));
        let object = dir.join(format!("neighbour-{index}.o"));
        let executable = dir.join(format!("neighbour-{index}"));
        let description = format!("compile neighbour-{index} through sibling OOM");

        // Every neighbour first creates the exact persistent failure shape
        // observed in the DynamoRIO tree: a zero-byte .o with a valid mtime. It
        // then sleeps across the offender's OOM, compiles over that placeholder,
        // links, and executes. A misattributed kill leaves at least one zero.
        let command = format!(
            "printf 'int main(void) {{ return 0; }}\\n' > {source}; \
             : > {object}; sleep 1; \
             cc -c {source} -o {object}; test -s {object}; \
             cc {object} -o {executable}; {executable}",
            source = source.display(),
            object = object.display(),
            executable = executable.display(),
        );
        steps.push(format!(
            r#"{{"group": "artifact", "job": "neighbour-{index}",
                "desc": {description:?}, "cmd": {command:?},
                "hint": {{"hard_mem_max_bytes": 268435456}}}}"#
        ));
        artifacts.push((index, description, object, executable));
    }
    steps.push(
        r#"{"group": "mem", "job": "offender", "desc": "allocate past cap",
            "cmd": "s=x; while true; do s=\"$s$s\"; done",
            "hint": {"hard_mem_max_bytes": 67108864}}"#
            .to_string(),
    );
    let dag_json = format!(r#"{{"steps": [{}]}}"#, steps.join(","));
    std::fs::write(&dag, dag_json).unwrap();

    let max_cpus = (NEIGHBOUR_COUNT + 1).to_string();
    let output = Command::new(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "--max-steps",
            &max_cpus,
            "--max-cpus",
            &max_cpus,
            "--keep-going",
            "--no-profile",
        ])
        .output()
        .expect("failed to spawn the built binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    let code = output.status.code();
    // Both reasons a boxed run can refuse: the probe ran and said no, or policy skipped without
    // asking. They are now DIFFERENT sentences, and this skip guard must recognize each — folding
    // them into one string is how the distinction was lost in the first place.
    let boxing_unavailable = combined.contains("systemd --user scope is unavailable")
        || combined.contains("SKIPPED BY POLICY");
    if code == Some(3) && boxing_unavailable {
        eprintln!(
            "SKIP boxed_oom_does_not_truncate_a_neighbour_artifact: cgroup boxing is unavailable \
             in this environment (need cgroup-v2 + a working systemd --user scope). Details:\n{stderr}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    assert_ne!(
        code,
        Some(3),
        "boxing was available but cgroup control readback failed:\n{combined}"
    );

    assert_eq!(
        code,
        Some(1),
        "the offender should fail the overall run while all neighbours complete; got \
         {code:?}\n{combined}"
    );
    assert!(
        combined.contains("mem.offender")
            && (combined.contains("OOM-KILLED") || combined.contains("MEMORY CAP HIT")),
        "failure attribution must name the OOM offender:\n{combined}"
    );
    let owns_outer_scope = combined.contains("containment OBSERVED");
    let uses_parent_delegation =
        combined.contains("cgroup boxing ACTIVE in parent-owned delegated step root");
    assert!(
        owns_outer_scope || uses_parent_delegation,
        "the run must identify its actual cgroup ownership route:\n{combined}"
    );
    if owns_outer_scope {
        // Only a top-level invocation creates and audits a systemd scope. Under the validation
        // graph this test is itself inside a delegated parent step, so the nested invocation must
        // instead identify that route above; its successful OOM attribution proves the child cap.
        assert!(
            combined.contains("memory.max=")
                && combined.contains("(bound)")
                && combined.contains("memory.swap.max=0 (disabled)")
                && combined.contains("memory.oom.group=1 (enabled)"),
            "top-level outer cgroup controls must be read back before the run starts:\n{combined}"
        );
    }
    for (index, description, object, executable) in artifacts {
        assert!(
            combined.contains(&format!("[artifact.neighbour-{index}]"))
                && combined.contains(&format!("PASS   {description}")),
            "neighbour {index} must complete after the sibling OOM:\n{combined}"
        );
        let object_len = std::fs::metadata(&object)
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert!(
            object_len > 0,
            "neighbour {index} survived as a process but left a truncated zero-byte object"
        );
        assert!(
            executable.is_file(),
            "neighbour {index} object was not linkable into an executable"
        );
    }
    eprintln!(
        "boxed OOM artifact integrity: {NEIGHBOUR_COUNT}/{NEIGHBOUR_COUNT} concurrent neighbours \
         produced nonzero, linkable, executable artifacts"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
