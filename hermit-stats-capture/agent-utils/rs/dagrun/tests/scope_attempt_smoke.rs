//! End-to-end bracket: a caller can tell boxed / skipped-by-policy / probe-failed apart.
//!
//! WHAT WENT WRONG WITHOUT THIS. `reexec_in_scope` returned `true` for BOTH "already boxed,
//! proceed" and "policy said skip, we never asked whether boxing was possible", and the caller
//! folded that bool into a single error whose wording, on the skip path, was "boxing was skipped
//! (e.g. CI without a systemd --user scope)". That sentence asserts a cause the code had not
//! tested. Four separate capability probes were then run against it — measuring a branch that
//! never executes in the environment under investigation.
//!
//! THE POLICY IS UNCHANGED HERE and these cases pin that down: every exit code below is what it
//! was before. What changed is that the run now says which of the four things happened, and offers
//! the instrument (`DAGRUN_FORCE_SCOPE_ATTEMPT=1`) to answer the capability question on a runner
//! population nobody has measured.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DAG: &str =
    r#"{"steps":[{"group":"g","job":"j","cmd":"echo hi","timeout":30,"cpu_timeout":600}]}"#;

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// `name` MUST be unique per test: cargo runs these concurrently in ONE process, so a
    /// pid-only directory name has every case deleting the fixture the others are reading. Two of
    /// these tests failed that way, intermittently, with a missing-DAG error that looked nothing
    /// like the thing under test.
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("dagrun_scope_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one.json"), DAG).unwrap();
        Fixture { dir }
    }
    fn dag(&self) -> PathBuf {
        self.dir.join("one.json")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Out {
    code: Option<i32>,
    text: String,
}

/// Whether this test binary itself entered below an outer dagrun containment boundary.
///
/// `runner_command` deliberately removes the outer scope identity and any one-hop delegation
/// because most cases below model a fresh top-level invocation. A real
/// `systemd-run --user --scope` is different: systemd cannot place a scope below the caller's
/// scope, so creating one here can move the probe or boxed runner into a sibling unit outside the
/// outer validator's ownership. Read the test process's original environment before constructing
/// that scrubbed child command and retain those live cases only when this integration test is run
/// standalone.
fn inherited_outer_run() -> bool {
    std::env::var_os("DAGRUN_DELEGATED_CGROUP").is_some()
        || std::env::var_os("DAGRUN_DELEGATED_UNBOXED").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        || std::env::var_os("DAGRUN_IN_SCOPE").as_deref() == Some(std::ffi::OsStr::new("1"))
        || std::env::var_os("DAGRUN_OUTER_RUN").is_some()
}

fn skip_top_level_scope_under_outer_run(case: &str) -> bool {
    if !inherited_outer_run() {
        return false;
    }
    eprintln!(
        "skipping {case}: a top-level systemd user scope could escape the parent-owned validation \
         boundary; run this integration test standalone for live scope coverage; tracked by #29 \
         delegated-scope-smokes"
    );
    true
}

/// Build a runner command without inheriting the scope policy/control knobs that these tests vary.
///
/// In particular, GitHub Actions exports `GITHUB_ACTIONS=1` to the test process. Letting that leak
/// into a child which explicitly sets `CI=1` changes the selected policy reason, while letting it
/// leak into a child intended to exercise boxing changes the entire path into a policy skip. Keep
/// the ordinary process environment (notably `PATH`) but make these inputs explicit per case.
fn runner_command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dagrun"));
    for key in [
        "CI",
        "GITHUB_ACTIONS",
        "DAGRUN_FORCE_SCOPE_ATTEMPT",
        "DAGRUN_IN_SCOPE",
        "DAGRUN_SCOPE_UNIT",
        "DAGRUN_DIRECT_CGROUP",
        // The test binary can itself be a command inside an explicitly delegated validation
        // step. These cases exercise fresh top-level scope policy, so inherited one-step nesting
        // authority would select a different (valid, but irrelevant) route before the branch
        // under test.
        "DAGRUN_OUTER_RUN",
        "DAGRUN_DELEGATED_CGROUP",
        "DAGRUN_DELEGATED_UNBOXED",
        "DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES",
        "DAGRUN_EXPECTED_OUTER_CPU_COUNT",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

/// Run the built binary with a controlled environment.
///
/// `hide_session` removes the two variables that make a systemd `--user` bus reachable, which is
/// how a runner with no user manager looks from inside the process.
fn run(fx: &Fixture, envs: &[(&str, &str)], extra: &[&str], hide_session: bool) -> Out {
    let mut cmd = runner_command();
    cmd.args(["run", "--dag", fx.dag().to_str().unwrap(), "--no-profile"])
        .args(extra)
        // Keep the evidence writer out of this test's way; it is covered elsewhere.
        .env("DAGRUN_NO_STEP_LOGS", "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if hide_session {
        cmd.env_remove("XDG_RUNTIME_DIR")
            .env_remove("DBUS_SESSION_BUS_ADDRESS");
    }
    let out = cmd.output().expect("failed to spawn the built binary");
    Out {
        code: out.status.code(),
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

/// A: `CI` set, boxing required. The historical behaviour (exit 3) with an honest reason.
#[test]
fn policy_skip_is_named_as_untested_not_as_unavailable() {
    let fx = Fixture::new("policy_skip");
    let r = run(&fx, &[("CI", "1")], &[], false);

    assert_eq!(r.code, Some(3), "exit code must be unchanged:\n{}", r.text);
    assert!(
        r.text.contains("SKIPPED BY POLICY") && r.text.contains("$CI"),
        "the skip must name itself and the variable that selected it:\n{}",
        r.text
    );
    assert!(
        r.text.contains("NOT TESTED") || r.text.contains("NOT tested"),
        "the skip must say the capability question was never asked:\n{}",
        r.text
    );
    assert!(
        r.text.contains("DAGRUN_FORCE_SCOPE_ATTEMPT"),
        "and it must point at the instrument that would answer it:\n{}",
        r.text
    );
    // THE REGRESSION GUARD. This exact sentence sent an investigation after a capability the code
    // had not measured; a skip must never speak for the probe.
    assert!(
        !r.text
            .contains("boxing was skipped (e.g. CI without a systemd --user scope)"),
        "a policy skip must not assert a cause it never tested:\n{}",
        r.text
    );
}

/// B: forced attempt on a host with no reachable user bus — a REAL probe failure, distinct from A.
#[test]
fn a_forced_attempt_that_fails_reports_a_probe_failure_not_a_skip() {
    let fx = Fixture::new("forced_fail");
    // Make the failure deterministic and local. Removing the session-bus variables is not a
    // containment boundary: a usable default user bus can still let the real systemd manager move
    // this child into a sibling scope outside an aggregate validator's delegated subtree.
    let shim_dir = fx.dir.join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let systemd_run = shim_dir.join("systemd-run");
    // It records each call's arguments and refuses with a recognisable reason, so the test can
    // see that the reason reaches the log and how many probes were made, under which unit names.
    let calls = fx.dir.join("systemd-run.calls");
    std::fs::write(
        &systemd_run,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\necho 'fake refusal: no user bus here' >&2\nexit 1\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&systemd_run, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        shim_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let r = run(
        &fx,
        &[
            ("CI", "1"),
            ("DAGRUN_FORCE_SCOPE_ATTEMPT", "1"),
            ("PATH", &path),
        ],
        &[],
        false,
    );

    assert_eq!(
        r.code,
        Some(3),
        "an unavailable scope still refuses:\n{}",
        r.text
    );
    assert!(
        r.text.contains("attempted and failed"),
        "a probe that ran and failed must say so:\n{}",
        r.text
    );
    assert!(
        !r.text.contains("SKIPPED BY POLICY"),
        "forcing the attempt must actually lift the skip:\n{}",
        r.text
    );
    // A refusal is retried exactly once, and each attempt's reason reaches the log.
    for attempt in 1..=2 {
        assert!(
            r.text.lines().any(|line| {
                line.contains(&format!("probe attempt {attempt} of 2 failed"))
                    && line.contains("fake refusal: no user bus here")
            }),
            "attempt {attempt} must be logged with systemd-run's own stderr:\n{}",
            r.text
        );
    }
    let calls = std::fs::read_to_string(&calls).unwrap_or_default();
    let units: Vec<&str> = calls
        .split_whitespace()
        .filter(|arg| arg.starts_with("--unit=dagrun-probe-"))
        .collect();
    assert_eq!(
        units.len(),
        2,
        "a refused probe is tried twice, never more:\n{calls}"
    );
    assert!(
        units[0].ends_with("-1") && units[1].ends_with("-2"),
        "each attempt uses its own unit name:\n{calls}"
    );
    assert!(
        calls
            .lines()
            .all(|call| call.split_whitespace().any(|arg| arg == "--collect")),
        "each probe asks systemd to unload its unit even if it fails:\n{calls}"
    );
}

/// B2: a probe whose `systemd-run` never answers is killed at its timeout, attempt by attempt.
///
/// The fake writes a recognisable line to stderr and then stops itself, which is how a call blocked
/// on an unresponsive user bus looks from outside. Two attempts of 8 seconds plus the 1-second
/// backoff make about 17 seconds; the case gives the binary 60 before it kills it and fails, so a
/// probe that waits without a bound fails here instead of hanging the suite.
#[test]
fn a_probe_that_never_answers_is_killed_at_its_timeout() {
    let fx = Fixture::new("forced_hang");
    let shim_dir = fx.dir.join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let systemd_run = shim_dir.join("systemd-run");
    let calls = fx.dir.join("systemd-run.calls");
    std::fs::write(
        &systemd_run,
        format!(
            "#!/bin/sh\necho \"$$ $*\" >> '{}'\necho 'fake: bus not answering' >&2\nkill -STOP $$\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&systemd_run, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        shim_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = runner_command();
    cmd.args(["run", "--dag", fx.dag().to_str().unwrap(), "--no-profile"])
        .env("DAGRUN_NO_STEP_LOGS", "1")
        .env("CI", "1")
        .env("DAGRUN_FORCE_SCOPE_ATTEMPT", "1")
        .env("PATH", &path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let started = std::time::Instant::now();
    let mut child = cmd.spawn().expect("failed to spawn the built binary");
    let limit = std::time::Duration::from_secs(60);
    let exited = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if started.elapsed() >= limit {
            let _ = child.kill();
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let took = started.elapsed();
    // A fake the binary failed to kill is still stopped and may hold the
    // inherited output pipes open, so kill it before reading to EOF. Only a
    // stopped process is killed: a pid the binary already reaped may be reused.
    let calls = std::fs::read_to_string(&calls).unwrap_or_default();
    for pid in calls
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let state = stat.rsplit_once(") ").map(|(_, rest)| rest.chars().next());
        if state == Some(Some('T')) {
            let _ = Command::new("kill").args(["-KILL", pid]).status();
        }
    }
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        exited,
        "the probe must not wait on systemd-run without a bound; killed after {took:?}:\n{text}"
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "a scope that never answers still refuses:\n{text}"
    );
    for attempt in 1..=2 {
        assert!(
            text.lines().any(|line| {
                line.contains(&format!("probe attempt {attempt} of 2 failed"))
                    && line.contains("timed out after 8s")
                    && line.contains("fake: bus not answering")
            }),
            "attempt {attempt} must report its timeout and the stderr written before it:\n{text}"
        );
    }
    assert!(
        took >= std::time::Duration::from_secs(16) && took < std::time::Duration::from_secs(40),
        "two 8-second attempts and a 1-second backoff, not {took:?}:\n{text}"
    );
}

/// C (control): `--allow-cgroup-failure` is unaffected — the sanctioned opt-out still runs unboxed.
///
/// This is the path every consuming CI lane actually takes, and it short-circuits ahead of the scope
/// logic entirely. If this changed, the finding would have cost a green CI to report.
#[test]
fn the_sanctioned_opt_out_is_untouched() {
    let fx = Fixture::new("opt_out");
    let r = run(&fx, &[("CI", "1")], &["--allow-cgroup-failure"], false);

    assert_eq!(r.code, Some(0), "the opt-out must still pass:\n{}", r.text);
    assert!(
        r.text.contains("UNBOXED"),
        "and must still say it is unboxed:\n{}",
        r.text
    );
    assert!(
        !r.text.contains("SKIPPED BY POLICY"),
        "the opt-out returns before the scope logic, so the skip must not even be reached:\n{}",
        r.text
    );
}

/// D: on a host that CAN box, forcing the attempt under `CI` actually boxes.
///
/// Skipped where a `--user` scope is unavailable, so the test says nothing false on a runner. This
/// is the positive half: without it, A and B are satisfiable by a build that never boxes at all.
#[test]
fn forcing_the_attempt_boxes_where_boxing_is_possible() {
    if skip_top_level_scope_under_outer_run("forced-box live scope") {
        return;
    }
    let probe = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            &format!("--unit=dagrun-attempt-probe-{}", std::process::id()),
            "true",
        ])
        .output();
    let available = probe.map(|o| o.status.success()).unwrap_or(false);
    if !available {
        eprintln!("skipping: no usable systemd --user scope on this host");
        return;
    }

    let fx = Fixture::new("forced_box");
    let r = run(
        &fx,
        &[("CI", "1"), ("DAGRUN_FORCE_SCOPE_ATTEMPT", "1")],
        &[],
        false,
    );

    assert_eq!(
        r.code,
        Some(0),
        "a forced attempt on a capable host should box and pass:\n{}",
        r.text
    );
    assert!(
        r.text.contains("cgroup boxing ACTIVE"),
        "containment must actually be established, not merely claimed possible:\n{}",
        r.text
    );
}

/// PROOF, NOT DECLARATION: the boxed path must SEE the live pid inside the promised cgroup.
///
/// Skipped where a `--user` scope is unavailable, so this says nothing false on a runner.
#[test]
fn the_boxed_path_observes_the_live_pid_in_the_promised_cgroup() {
    if skip_top_level_scope_under_outer_run("observed-box live scope") {
        return;
    }
    let probe = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            &format!("--unit=dagrun-proof-probe-{}", std::process::id()),
            "true",
        ])
        .output();
    if !probe.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skipping: no usable systemd --user scope on this host");
        return;
    }
    let fx = Fixture::new("proof");
    let r = run(&fx, &[], &[], false);

    assert_eq!(r.code, Some(0), "a boxed run should pass:\n{}", r.text);
    assert!(
        r.text.contains("containment OBSERVED: pid "),
        "the run must report an OBSERVED pid, not merely that boxing is ACTIVE:\n{}",
        r.text
    );
    // The observation must be bound to the cgroup that was actually arranged.
    assert!(
        r.text.contains("promised unit dagrun-") && r.text.contains("/sys/fs/cgroup/"),
        "the proof must name the promised unit and a real cgroup path:\n{}",
        r.text
    );
}

/// NEGATIVE: a FORGED in-scope sentinel must never be believed.
///
/// This is the exact lie the old code accepted — `is_in_scope()` read an environment variable, and
/// every consumer printed "cgroup boxing ACTIVE" on the strength of it. Anything can export it.
#[test]
fn a_forged_in_scope_sentinel_is_refused_not_believed() {
    let fx = Fixture::new("forged");
    let r = run(&fx, &[("DAGRUN_IN_SCOPE", "1")], &[], false);

    assert_eq!(
        r.code,
        Some(3),
        "a sentinel with no observable containment must refuse:\n{}",
        r.text
    );
    assert!(
        r.text.contains("could NOT be observed"),
        "the refusal must say the observation failed:\n{}",
        r.text
    );
    assert!(
        !r.text.contains("boxing ACTIVE"),
        "containment must NEVER be claimed on the strength of an environment variable:\n{}",
        r.text
    );
}

/// NEGATIVE: a sentinel pointing at a unit this process is not in must be refused, and the
/// refusal must name the cgroup actually observed — a mismatch is more useful than a bare "no".
#[test]
fn a_sentinel_naming_the_wrong_unit_is_refused_and_names_what_it_found() {
    let fx = Fixture::new("wrongunit");
    let r = run(
        &fx,
        &[
            ("DAGRUN_IN_SCOPE", "1"),
            ("DAGRUN_SCOPE_UNIT", "totally-not-our-unit.scope"),
        ],
        &[],
        false,
    );

    assert_eq!(
        r.code,
        Some(3),
        "a wrong-unit claim must refuse:\n{}",
        r.text
    );
    assert!(
        r.text
            .contains("the promised unit was totally-not-our-unit.scope"),
        "the refusal must name the promised unit it could not confirm:\n{}",
        r.text
    );
    assert!(
        r.text.contains("/sys/fs/cgroup/"),
        "and the cgroup it actually observed, so the mismatch is actionable:\n{}",
        r.text
    );
}

/// CONTROL: the forged sentinel plus the sanctioned opt-out degrades to UNBOXED and still passes.
///
/// Without this, the three refusals above are satisfiable by a build that refuses everything.
#[test]
fn a_forged_sentinel_with_the_opt_out_degrades_rather_than_claiming() {
    let fx = Fixture::new("forged_optout");
    let r = run(
        &fx,
        &[("DAGRUN_IN_SCOPE", "1")],
        &["--allow-cgroup-failure"],
        false,
    );

    assert_eq!(r.code, Some(0), "the opt-out must still pass:\n{}", r.text);
    assert!(
        r.text.contains("UNBOXED") && r.text.contains("could NOT be observed"),
        "it must degrade AND say why:\n{}",
        r.text
    );
    assert!(
        !r.text.contains("boxing ACTIVE"),
        "degrading must never print a containment claim:\n{}",
        r.text
    );
}

/// Read the containment record out of a run's durable journal.
fn journal_containment(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("journal.jsonl")).ok()?;
    text.lines()
        .find(|l| l.contains("\"event\":\"containment\""))
        .map(str::to_string)
}

/// Run once with an explicit evidence directory, returning (output, journal line).
fn run_recording(name: &str, envs: &[(&str, &str)], extra: &[&str]) -> (String, Option<String>) {
    let dir = std::env::temp_dir().join(format!("dagrun_rec_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("one.json");
    std::fs::write(&dag, DAG).unwrap();
    let ev = dir.join("evidence");
    let mut cmd = runner_command();
    cmd.args(["run", "--dag", dag.to_str().unwrap(), "--no-profile"])
        .args(extra)
        .env("DAGRUN_LOG_DIR", &ev);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to spawn the built binary");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let line = journal_containment(&ev);
    let _ = std::fs::remove_dir_all(&dir);
    (text, line)
}

/// THE RECORD, not a warning that scrolls past.
///
/// A run used to say ONCE, on stderr, that it was unboxed, and then no artifact carried it — so a
/// run reviewed later was indistinguishable from a boxed one. Establishing whether CI boxes at all
/// then cost four probes and three confident wrong answers, because the evidence had never
/// existed. Every state must reach the durable journal AND the banner line.
#[test]
fn every_containment_state_reaches_the_banner_and_the_durable_journal() {
    // 1) UNBOXED. The state that used to vanish.
    let (text, line) = run_recording("unboxed", &[("CI", "1")], &["--allow-cgroup-failure"]);
    assert!(
        text.contains("CONTAINMENT: unboxed"),
        "the banner must name the state:\n{text}"
    );
    let line = line.expect("an unboxed run must still write a containment record");
    assert!(
        line.contains("\"state\":\"unboxed\"")
            && line.contains("\"caps_enforced\":\"false\"")
            && line.contains("\"subtree_killable\":\"false\""),
        "the journal must carry the state and what it did NOT enforce:\n{line}"
    );

    // 2) STEPS-CONTAINED-ONLY must be its own state. Recording the direct route as "boxed" would
    //    be the overclaim relocated into an artifact that outlives the run.
    let (text, line) = run_recording("direct", &[("CI", "1"), ("DAGRUN_DIRECT_CGROUP", "1")], &[]);
    // GATE ON WHETHER THE ROUTE WAS TAKEN, NOT ON THE ANSWER IT GAVE. Keying the skip off the
    // state label means a build that mislabels the direct route as run-boxed simply skips this
    // case and passes — measured: it did. The route announces itself independently of the label,
    // so that announcement is what decides whether the assertion applies.
    let route_taken = text.contains("teardown ACTIVE via direct cgroupfs");
    if route_taken {
        assert!(
            text.contains("CONTAINMENT: steps-contained-only"),
            "the direct cgroupfs route must record steps-contained-only, never run-boxed:\n{text}"
        );
        let line = line.expect("the direct route must write a containment record");
        assert!(
            line.contains("\"state\":\"steps-contained-only\"")
                && line.contains("\"caps_enforced\":\"false\"")
                && line.contains("\"subtree_killable\":\"true\""),
            "steps-contained-only must record killable-but-uncapped, not boxed:\n{line}"
        );
        assert!(
            !line.contains("\"state\":\"run-boxed\""),
            "the direct route must never record itself as run-boxed:\n{line}"
        );
    } else {
        eprintln!("skipping direct-route case: cgroupfs route unavailable here");
    }

    // 3) RUN-BOXED, where the host can box. Carries the observation so the claim is recheckable.
    // A delegated aggregate runner cannot safely create this fresh top-level systemd scope, but
    // the unboxed and direct-route journal assertions above remain valuable and still ran.
    if skip_top_level_scope_under_outer_run("run-boxed containment-record leg") {
        return;
    }
    let probe = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            &format!("--unit=dagrun-rec-probe-{}", std::process::id()),
            "true",
        ])
        .output();
    if probe.map(|o| o.status.success()).unwrap_or(false) {
        let (text, line) = run_recording("boxed", &[], &[]);
        assert!(
            text.contains("CONTAINMENT: run-boxed"),
            "a boxed run must say so in the banner:\n{text}"
        );
        let line = line.expect("a boxed run must write a containment record");
        assert!(
            line.contains("\"state\":\"run-boxed\"")
                && line.contains("\"caps_enforced\":\"true\"")
                && line.contains("observed in /sys/fs/cgroup/"),
            "the boxed record must carry the OBSERVED cgroup, not just the word boxed:\n{line}"
        );
    } else {
        eprintln!("skipping run-boxed case: no usable systemd --user scope on this host");
    }
}
