//! Two-sided output-contract bracket for validation verbosity.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("dagrun_verbosity_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn dag(&self, fail: bool) -> PathBuf {
        let first = if fail {
            "printf '##TEST-START suite::failure\\nfull-error-line\\n' >&2; exit 7"
        } else {
            "printf '##TEST-START suite::case\\n:: Run1...\\n##TEST-END suite::case PASS\\n'"
        };
        let path = self.dir.join(if fail { "fail.json" } else { "pass.json" });
        let doc = serde_json::json!({
            "steps": [
                {"group": "g", "job": "first", "desc": "first", "cmd": first},
                {"group": "g", "job": "must_not_run", "desc": "second", "cmd": "printf 'late-step-ran\\n'", "deps": ["g.first"]}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        path
    }

    fn split_stream_dag(&self) -> PathBuf {
        let path = self.dir.join("split-stream.json");
        let doc = serde_json::json!({
            "steps": [{
                "group": "g",
                "job": "split",
                "desc": "split",
                "cmd": "printf '##TEST-START stdout::case\\n'; sleep 0.1; printf 'stderr raced after stdout marker\\n' >&2"
            }]
        });
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run(dag: &Path, extra: &[&str]) -> (i32, String, Duration) {
    let start = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_dagrun"))
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-j",
            "1",
            "--unsafe-no-cgroups",
            "--no-profile-feedback",
        ])
        .args(extra)
        .output()
        .unwrap();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text, start.elapsed())
}

#[test]
fn levels_bound_success_but_never_hide_failure() {
    let fixture = Fixture::new();
    let pass = fixture.dag(false);
    let fail = fixture.dag(true);

    let (code, level1, _) = run(&pass, &[]);
    assert_eq!(code, 0, "{level1}");
    assert!(
        !level1.contains(":: Run1..."),
        "level 1 leaked step body: {level1}"
    );
    assert!(
        level1.contains("[g.first] ✓ PASS"),
        "level 1 lost the step verdict: {level1}"
    );

    let (code, level2, _) = run(&pass, &["-v"]);
    assert_eq!(code, 0, "{level2}");
    assert!(
        level2.contains("[g.first] :: Run1..."),
        "level 2 did not stream: {level2}"
    );

    let (code, level5, _) = run(&pass, &["-v", "-v", "-v", "-v"]);
    assert_eq!(code, 0, "{level5}");
    assert!(
        level5.contains("[g.first][test=suite::case] :: Run1..."),
        "level 5 lost the test identity: {level5}"
    );
    for line in level5
        .lines()
        .filter(|line| line.contains("] :: Run1") || line.contains("] ##TEST-"))
    {
        assert!(line.contains("[test="), "unidentified level-5 line: {line}");
    }

    let (code, failed, wall) = run(&fail, &[]);
    assert_ne!(code, 0, "{failed}");
    assert!(
        failed.contains("full-error-line"),
        "default hid complete failure detail: {failed}"
    );
    assert!(
        !failed.contains("late-step-ran"),
        "fail-fast launched a dependent after failure: {failed}"
    );
    assert!(
        wall < Duration::from_secs(5),
        "fail-fast was not prompt: {wall:?}"
    );

    let (code, failed_level5, _) = run(&fail, &["-v", "-v", "-v", "-v"]);
    assert_ne!(code, 0, "{failed_level5}");
    let detail = failed_level5
        .split("[g.first] ----- detail -----")
        .nth(1)
        .and_then(|tail| tail.split("[g.first] ----- end detail -----").next())
        .expect("level-5 failure output omitted the complete detail block");
    assert!(
        detail.contains("[g.first][test=suite::failure] full-error-line"),
        "level-5 failure replay lost test identity: {detail}"
    );

    let (code, split_stream, _) = run(&fixture.split_stream_dag(), &["-v", "-v", "-v", "-v"]);
    assert_eq!(code, 0, "{split_stream}");
    assert!(
        split_stream.contains("[g.split][test=g.split] stderr raced after stdout marker"),
        "stderr borrowed a racy stdout identity: {split_stream}"
    );
    assert!(
        !split_stream.contains("[test=stdout::case] stderr raced after stdout marker"),
        "split streams falsely attributed stderr to the stdout test: {split_stream}"
    );
}
