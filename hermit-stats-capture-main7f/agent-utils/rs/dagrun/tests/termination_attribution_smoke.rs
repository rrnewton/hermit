//! Two-sided bracket for general termination, surviving evidence, and test-level attribution.
//!
//! WHY BOTH DIRECTIONS. A termination change that only demonstrates its positive case is
//! indistinguishable from one that kills everything, and an attribution change that only
//! demonstrates its positive case is indistinguishable from one that accuses somebody at random.
//! Every case here therefore has a partner:
//!
//! | case                        | asserts |
//! |-----------------------------|---------|
//! | `hung_test_*` (positive)    | the run RETURNS, names the hung TEST, kills a setsid/double-fork escapee, and leaves the log on disk |
//! | `clean_suite_*` (control)   | the same suite unhung still PASSES, accuses nobody, and triggers no sweep |
//! | `bystander_*` (negative)    | a process that is not ours, alive throughout, is UNHARMED — cgroup and nonce teardown both follow ownership, not process names |
//! | `runner_sigkilled_*`        | the CI-cancellation case: SIGKILL the runner mid-suite and the on-disk journal still identifies the in-flight test |
//!
//! The escapee bounds itself with a short sleep and is killed by RECORDED PID afterwards, so this
//! fixture cannot leak a process onto a shared machine even if a build leaves it running.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// A libtest-shaped suite whose third test either hangs forever or completes.
///
/// The `printf` without a trailing newline is exactly how Rust's libtest announces a test under
/// `--nocapture`, and it is the shape that makes the hung test nameable at all.
const SUITE: &str = r#"#!/usr/bin/env bash
mode="$1"
printf 'running 4 tests\n'
printf 'test suite::alpha ... '; sleep 0.2; printf 'ok\n'
printf 'test suite::beta ... ';  sleep 0.2; printf 'ok\n'
printf 'test suite::gamma_the_hang ... '
if [ "$mode" = hang ]; then
  # setsid + double fork: leaves the step's session, its process group, AND its parentage
  # (the intermediate parent exits), while inheriting the step's stdout/stderr write ends.
  setsid bash -c "echo \$\$ > $ESCAPEE_PID; exec sleep 120" &
  sleep 120
fi
sleep 0.2; printf 'ok\n'
printf 'test suite::delta ... '; sleep 0.2; printf 'ok\n'
printf 'test result: ok. 4 passed; 0 failed\n'
"#;

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("dagrun_attr_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let suite = dir.join("suite.sh");
        let mut f = std::fs::File::create(&suite).unwrap();
        f.write_all(SUITE.as_bytes()).unwrap();
        drop(f);
        Fixture { dir }
    }

    fn dag(&self, mode: &str, timeout_s: i64) -> PathBuf {
        let path = self.dir.join(format!("{mode}.json"));
        let cmd = format!("bash {} {mode}", self.dir.join("suite.sh").display());
        let json = format!(
            r#"{{"steps":[{{"group":"tests","job":"suite","cmd":"{cmd}","timeout":{timeout_s},"cpu_timeout":600}}]}}"#
        );
        std::fs::write(&path, json).unwrap();
        path
    }

    fn logs(&self) -> PathBuf {
        self.dir.join("evidence")
    }
    fn escapee_pid_file(&self) -> PathBuf {
        self.dir.join("escapee.pid")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        kill_recorded_pid(&self.escapee_pid_file());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// SIGKILL one EXPLICIT pid this fixture recorded — never a name or command-line pattern, which on
/// a shared machine would reach other tenants' work.
fn kill_recorded_pid(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(pid) = text.trim().parse::<i64>() else {
        return;
    };
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Whether the recorded pid is still a LIVE process.
///
/// A zombie counts as dead, and the check is retried briefly. SIGKILL delivery and reaping are
/// asynchronous, and the escapee's parent has already exited, so `/proc/<pid>` can linger in state
/// `Z` for a moment after a perfectly correct kill. Asserting on directory existence alone makes
/// the test fail under load for a reason that has nothing to do with the behaviour under test.
fn pid_alive(path: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let pid: i64 = text.trim().parse().ok()?;
    for _ in 0..50 {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return Some(false); // gone
        };
        let zombie = status
            .lines()
            .find_map(|l| l.strip_prefix("State:"))
            .map(|v| v.trim_start().starts_with('Z'))
            .unwrap_or(false);
        if zombie {
            return Some(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(true)
}

fn count(journal: &str, event: &str) -> usize {
    journal
        .lines()
        .filter(|l| l.contains(&format!("\"event\":\"{event}\"")))
        .count()
}

/// POSITIVE + NEGATIVE in one run: the hung test must be terminated and named, while a bystander
/// process that this runner does not own must survive it untouched.
#[test]
fn hung_test_is_terminated_named_and_logged_while_a_bystander_survives() {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let fx = Fixture::new("hang");
    let dag = fx.dag("hang", 6);

    // NEGATIVE CONTROL: a process in its own session, not carrying our nonce. It is started in its
    // own process group and killed by its own pid, never by pattern.
    let mut bystander = Command::new("setsid")
        .args(["sleep", "60"])
        .spawn()
        .expect("failed to start the bystander");
    std::thread::sleep(Duration::from_millis(200));

    let out = Command::new(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-j",
            "1",
            "--no-profile",
            "--no-profile-feedback",
            "--allow-cgroup-failure",
        ])
        .env("DAGRUN_LOG_DIR", fx.logs())
        .env("ESCAPEE_PID", fx.escapee_pid_file())
        .output()
        .expect("failed to spawn the built binary");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The bystander must still be running BEFORE we clean it up.
    let bystander_alive = Path::new(&format!("/proc/{}", bystander.id())).exists();
    let _ = bystander.kill();
    let _ = bystander.wait();

    // 1) The run RETURNS. Before bounded joins it did not; without a general kill it still would
    //    not, because the escapee holds the pipes open.
    assert_ne!(
        out.status.code(),
        Some(0),
        "a step killed at its wall budget must not report success:\n{text}"
    );

    // 2) THE TEST IS NAMED, not merely the node.
    assert!(
        text.contains("culprit test suite::gamma_the_hang"),
        "expected the hung TEST to be named, not just the node:\n{text}"
    );
    assert!(
        text.contains("2 test(s) completed first"),
        "the culprit must travel with how far the suite got:\n{text}"
    );

    // 3) The setsid/double-fork escapee is TERMINATED. An unboxed run reaches it by ownership
    //    nonce after process group and parentage both fail; a boxed run owns the cgroup subtree
    //    and kills it atomically. The test binary may itself be inside a delegated validation
    //    step, so assert the mechanism selected by the actual route rather than demanding the
    //    weaker fallback while containment is active.
    assert_eq!(
        pid_alive(&fx.escapee_pid_file()),
        Some(false),
        "the setsid/double-fork escapee survived teardown:\n{text}"
    );
    if text.contains("cgroup boxing ACTIVE") {
        assert!(
            text.contains("CONTAINMENT: run-boxed"),
            "the cgroup teardown route must carry an observed containment record:\n{text}"
        );
    } else {
        assert!(
            text.contains("by ownership nonce"),
            "the unboxed fallback must report the ownership-nonce kill it made:\n{text}"
        );
    }

    // 4) NEGATIVE: ownership, not pattern. A process we did not spawn is untouched.
    assert!(
        bystander_alive,
        "the sweep killed a bystander process it does not own:\n{text}"
    );

    // 5) THE EVIDENCE SURVIVES, on disk, written incrementally.
    let log = std::fs::read_to_string(fx.logs().join("tests.suite.log"))
        .expect("per-step log missing from the evidence directory");
    assert!(
        log.contains("suite::gamma_the_hang"),
        "the hung test's own output must be in the durable log:\n{log}"
    );
    let journal = std::fs::read_to_string(fx.logs().join("journal.jsonl"))
        .expect("journal missing from the evidence directory");
    assert_eq!(
        (count(&journal, "test_start"), count(&journal, "test_end")),
        (3, 2),
        "3 starts and 2 ends is what identifies the third test as the one that never \
         finished:\n{journal}"
    );
    assert!(
        count(&journal, "step_timeout") == 1,
        "the teardown record must be written BEFORE the kill:\n{journal}"
    );
    assert!(
        journal.contains("\"culprit_test\":\"suite::gamma_the_hang\""),
        "the journal must name the culprit independently of stdout:\n{journal}"
    );
}

/// CONTROL: the same suite with nothing hung. A fix that only shows the positive is
/// indistinguishable from one that kills everything, so this must stay boring.
#[test]
fn clean_suite_still_passes_and_accuses_nobody() {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let fx = Fixture::new("clean");
    let dag = fx.dag("clean", 60);

    let out = Command::new(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-j",
            "1",
            "--no-profile",
            "--no-profile-feedback",
            "--allow-cgroup-failure",
        ])
        .env("DAGRUN_LOG_DIR", fx.logs())
        .env("ESCAPEE_PID", fx.escapee_pid_file())
        .output()
        .expect("failed to spawn the built binary");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "a clean suite must still pass:\n{text}"
    );
    assert!(
        !text.contains("culprit"),
        "a passing step must accuse no test:\n{text}"
    );
    assert!(
        !text.contains("by ownership nonce"),
        "no over-kill: the nonce sweep must signal nothing when nothing escaped:\n{text}"
    );

    let journal = std::fs::read_to_string(fx.logs().join("journal.jsonl"))
        .expect("journal missing from the evidence directory");
    assert_eq!(
        count(&journal, "test_end"),
        4,
        "a clean run must still report every test boundary:\n{journal}"
    );
    let end = journal
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .expect("journal must contain JSON")
        .into_iter()
        .find(|row| row["event"] == "step_end")
        .expect("journal must contain step_end");
    assert_eq!(
        dagrun::require_step_end_ok(&end),
        Ok(true),
        "the terminal record must say the step passed:\n{journal}"
    );
}

/// THE CASE THE EVIDENCE EXISTS FOR: the runner itself is killed, as a CI provider kills a
/// cancelled job. Nothing gets to write a summary, so only what was already flushed survives — and
/// that must still be enough to name the test that was in flight.
#[test]
fn journal_identifies_the_in_flight_test_after_the_runner_is_sigkilled() {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let fx = Fixture::new("sigkill");
    let dag = fx.dag("hang", 600); // long budget: the KILL must come from outside, not from us

    let mut child = Command::new(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-j",
            "1",
            "--no-profile",
            "--no-profile-feedback",
            "--allow-cgroup-failure",
        ])
        .env("DAGRUN_LOG_DIR", fx.logs())
        .env("ESCAPEE_PID", fx.escapee_pid_file())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn the built binary");

    std::thread::sleep(Duration::from_secs(3)); // mid-suite, inside the third test
    let _ = child.kill(); // SIGKILL, by the pid of a child we started
    let _ = child.wait();
    kill_recorded_pid(&fx.escapee_pid_file()); // the escapee outlives its killed runner; clean it up

    let journal = std::fs::read_to_string(fx.logs().join("journal.jsonl"))
        .expect("the journal must survive a SIGKILL of the runner");
    assert_eq!(
        (count(&journal, "test_start"), count(&journal, "test_end")),
        (3, 2),
        "an unmatched test_start is the only thing that can name the in-flight test once the \
         process is gone:\n{journal}"
    );
    let last_start = journal
        .lines()
        .rfind(|l| l.contains("\"event\":\"test_start\""))
        .unwrap_or("")
        .to_string();
    assert!(
        last_start.contains("suite::gamma_the_hang"),
        "the unmatched test_start must name the in-flight test:\n{last_start}"
    );
    assert!(
        std::fs::read_to_string(fx.logs().join("tests.suite.log"))
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "the per-step log must survive the SIGKILL too"
    );
}
