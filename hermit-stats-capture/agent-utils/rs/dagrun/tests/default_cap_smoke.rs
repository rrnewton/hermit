//! Integration smoke test: prove the SMALL DEFAULT CAP (the "forcing function") is applied and
//! enforced for a step that DECLARES NOTHING.
//!
//! Boxing-on-by-default plus a deliberately tight default (1 core / 1 GiB / 10 s CPU) is the
//! mechanism that makes an undeclared node hit the cap immediately and DECLARE its real needs, so
//! per-node resource metadata is generated EMPIRICALLY. This test plants BOTH directions plus a
//! control so a one-sided "kills everything" cap cannot pass:
//!
//!   A (breach)    : NO hint, allocate ~1.4 GiB  -> OOM-killed at the 1 GiB default (exit 1)
//!   B (compliant) : NO hint, allocate ~0.3 GiB  -> passes (the default does NOT kill everything)
//!   C (control)   : SAME ~1.4 GiB but DECLARES hard_mem_max 2 GiB -> passes, proving it was the
//!                   1 GiB *default* (not an ambient/outer limit) that killed A.
//!   D (opt-out)   : SAME ~1.4 GiB with --unsafe-no-cgroups -> passes unboxed.
//!   E (CPU pass)  : NO hint, read back a 1-core cpu.max and burn 1 CPU-second -> passes.
//!   F (CPU breach): NO hint, read back a 1-core cpu.max and burn >10 CPU-seconds -> CPU-TIMEOUT.
//!
//! Enforcement is GUARANTEED reached: default `run` requires boxing (no `--allow-cgroup-failure`),
//! so an environment that cannot box exits 3 and this test skips LOUDLY rather than asserting on a
//! non-boxing host. A non-3 exit means boxing was ACTIVE and the child-cgroup memory.max fired.

use std::io::Write;
use std::process::Command;

// Undeclared steps (no `hint`) — the ONLY inner memory.max they get is the small default.
const BREACH_DAG: &str = r#"{"steps": [{"group": "mem", "job": "breach-default",
  "desc": "undeclared step allocates 1.4GiB",
  "cmd": "python3 -c 'b=bytearray(1400*1024*1024); print(len(b))'"}]}"#;
const COMPLIANT_DAG: &str = r#"{"steps": [{"group": "mem", "job": "compliant-default",
  "desc": "undeclared step allocates 300MiB",
  "cmd": "python3 -c 'b=bytearray(300*1024*1024); print(len(b))'"}]}"#;
// Same allocation as BREACH but declares a 2 GiB cap: escapes the 1 GiB default while fitting
// inside the delegated validation harness's own memory envelope on a four-core runner.
const DECLARED_DAG: &str = r#"{"steps": [{"group": "mem", "job": "breach-but-declared",
  "desc": "1.4GiB alloc but declares hard_mem_max 2GiB",
  "cmd": "python3 -c 'b=bytearray(1400*1024*1024); print(len(b))'",
  "hint": {"hard_mem_max_bytes": 2147483648}}]}"#;
const CPU_COMPLIANT_DAG: &str = r#"{"steps": [{"group": "cpu", "job": "compliant-default",
  "desc": "undeclared step reads one-core quota and burns 1 CPU-second",
  "cmd": "python3 -c 'import pathlib,time; cg=next(x.split(\":\",2)[2].strip() for x in open(\"/proc/self/cgroup\") if x.startswith(\"0::\")); quota=pathlib.Path(\"/sys/fs/cgroup\"+cg+\"/cpu.max\").read_text().strip(); assert quota==\"100000 100000\",quota; start=time.process_time(); exec(\"while time.process_time()-start < 1.0: pass\")'"}]}"#;
const CPU_BREACH_DAG: &str = r#"{"steps": [{"group": "cpu", "job": "breach-default",
  "desc": "undeclared step reads one-core quota and exceeds 10 CPU-seconds",
  "cmd": "python3 -c 'import pathlib,time; cg=next(x.split(\":\",2)[2].strip() for x in open(\"/proc/self/cgroup\") if x.startswith(\"0::\")); quota=pathlib.Path(\"/sys/fs/cgroup\"+cg+\"/cpu.max\").read_text().strip(); assert quota==\"100000 100000\",quota; start=time.process_time(); exec(\"while time.process_time()-start < 12.0: pass\")'"}]}"#;

fn parent_only_offers_unboxed_delegation() -> bool {
    matches!(
        std::env::var("DAGRUN_DELEGATED_UNBOXED").as_deref(),
        Ok("1")
    ) && std::env::var_os("DAGRUN_OUTER_RUN").is_some()
}

/// Run `dag_json` through the built binary under default (boxing-required) `run`; returns
/// `(exit_code, combined_stdout_stderr)`.
fn run_dag(label: &str, dag_json: &str, extra_args: &[&str]) -> (Option<i32>, String) {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let dir = std::env::temp_dir().join(format!("dagrun_defcap_{}_{}", label, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("dag.json");
    let mut f = std::fs::File::create(&dag).unwrap();
    f.write_all(dag_json.as_bytes()).unwrap();
    drop(f);
    let mut args = vec!["run", "--dag", dag.to_str().unwrap()];
    args.extend_from_slice(extra_args);
    args.extend_from_slice(&["-q", "--no-profile"]);
    let output = Command::new(bin)
        .args(&args)
        .output()
        .expect("failed to spawn the built binary");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
    (output.status.code(), combined)
}

#[test]
fn default_small_cap_boxes_an_undeclared_step() {
    if parent_only_offers_unboxed_delegation() {
        eprintln!(
            "SKIP default_small_cap_boxes_an_undeclared_step: parent validation is explicitly \
             unboxed; refusing to run the breach workload without the default cgroup cap"
        );
        return;
    }
    // Probe with the breach DAG; a code-3 means boxing is unavailable here -> skip loudly.
    let (breach_code, breach_out) = run_dag("breach", BREACH_DAG, &[]);
    if breach_code == Some(3) {
        eprintln!(
            "SKIP default_small_cap_boxes_an_undeclared_step: cgroup boxing unavailable (need \
             cgroup-v2 + a working systemd --user scope). Details:\n{breach_out}"
        );
        return;
    }

    // A: an UNDECLARED step over the 1 GiB default is OOM-killed. No hint is present, so the only
    // possible inner memory cap is the small default -> this proves the default reached enforcement.
    assert_eq!(
        breach_code,
        Some(1),
        "undeclared 1.4GiB step should be OOM-killed at the 1 GiB default (exit 1); got \
         {breach_code:?}\n{breach_out}"
    );
    assert!(
        breach_out.contains("OOM-KILLED") || breach_out.contains("MEMORY CAP HIT"),
        "expected the inner memory cap to fire on the undeclared step:\n{breach_out}"
    );

    // B: an UNDECLARED step UNDER the default passes -> the cap does not kill everything.
    let (compliant_code, compliant_out) = run_dag("compliant", COMPLIANT_DAG, &[]);
    assert_eq!(
        compliant_code,
        Some(0),
        "undeclared 300MiB step is under the 1 GiB default and must PASS; got \
         {compliant_code:?}\n{compliant_out}"
    );

    // C: the SAME 1.4GiB allocation passes when the step declares a 2 GiB cap -> it was the 1 GiB
    // *default* (not an ambient/outer limit) that killed A.
    let (declared_code, declared_out) = run_dag("declared", DECLARED_DAG, &[]);
    assert_eq!(
        declared_code,
        Some(0),
        "1.4GiB step declaring hard_mem_max 2GiB must escape the default and PASS; got \
         {declared_code:?}\n{declared_out}"
    );
}

#[test]
fn default_cpu_caps_fire_and_allow_compliant_work() {
    if parent_only_offers_unboxed_delegation() {
        eprintln!(
            "SKIP default_cpu_caps_fire_and_allow_compliant_work: parent validation is explicitly \
             unboxed; the default cgroup CPU caps cannot be tested"
        );
        return;
    }
    // E: the command reads the kernel control before doing work, so a pass proves the one-core
    // cpu.max was applied and that a workload comfortably below 10 CPU-seconds is untouched.
    let (compliant_code, compliant_out) = run_dag("cpu-compliant", CPU_COMPLIANT_DAG, &[]);
    if compliant_code == Some(3) {
        eprintln!(
            "SKIP default_cpu_caps_fire_and_allow_compliant_work: cgroup boxing unavailable. \
             Details:\n{compliant_out}"
        );
        return;
    }
    assert_eq!(
        compliant_code,
        Some(0),
        "undeclared one-core workload burning 1 CPU-second must PASS; got \
         {compliant_code:?}\n{compliant_out}"
    );

    // F: the same quota readback followed by >10 CPU-seconds must trip the CPU-time forcing cap.
    // A generic process failure is insufficient: require the named CPU-TIMEOUT classification.
    let (breach_code, breach_out) = run_dag("cpu-breach", CPU_BREACH_DAG, &[]);
    assert_eq!(
        breach_code,
        Some(1),
        "undeclared workload exceeding 10 CPU-seconds must fail; got \
         {breach_code:?}\n{breach_out}"
    );
    assert!(
        breach_out.contains("CPU-TIMEOUT >10s cpu"),
        "expected the named 10-second CPU cap to fire:\n{breach_out}"
    );
}

/// The deliberate opt-out is the only way an undeclared step escapes the default cap.
#[test]
fn unsafe_no_cgroups_is_the_explicit_opt_out() {
    let (code, out) = run_dag("unsafe-optout", BREACH_DAG, &["--unsafe-no-cgroups"]);
    assert_eq!(
        code,
        Some(0),
        "--unsafe-no-cgroups must deliberately run the undeclared 1.4GiB step unboxed; got \
         {code:?}\n{out}"
    );
    assert!(
        !out.contains("OOM-KILLED") && !out.contains("MEMORY CAP HIT"),
        "no inner memory cap must fire in the deliberate unboxed mode:\n{out}"
    );
    assert!(
        out.contains("DELIBERATELY UNBOXED via --unsafe-no-cgroups"),
        "the opt-out must remain loud and reviewable:\n{out}"
    );
}
