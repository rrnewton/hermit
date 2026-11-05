//! End-to-end tests of the built `gh-paced` binary against a fake `gh` shell script.
//!
//! These run real processes on the real clock, so the budgets come from a per-test config file
//! with short intervals (1 to 2 s) and the assertions use generous margins. The decision logic is
//! covered with a fake clock in `replay.rs` and the unit tests; this file checks what only real
//! processes can show: the shared lock across processes, pass-through of exit codes, output,
//! stdin and signals, and the command-line surface.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Instant;

const BIN: &str = env!("CARGO_BIN_EXE_gh-paced");

const FAKE_GH: &str = r#"#!/bin/bash
# Fake gh for gh-paced tests. Logs each call with a nanosecond timestamp.
log="${FAKE_GH_LOG:?}"
if [ "$1 $2" = "api rate_limit" ] && [ $# -eq 2 ]; then
  echo "$(date +%s.%N) rate_limit" >> "$log"
  if [ -n "${FAKE_GH_RATE_STDERR_FILE:-}" ]; then cat "$FAKE_GH_RATE_STDERR_FILE" >&2; fi
  if [ -n "${FAKE_GH_RATE_JSON:-}" ]; then printf '%s\n' "$FAKE_GH_RATE_JSON"; else
    r=$(( $(date +%s) + 3000 ))
    printf '{"resources":{"core":{"limit":5000,"used":10,"remaining":4990,"reset":%s},"graphql":{"limit":5000,"used":0,"remaining":5000,"reset":%s},"search":{"limit":30,"used":0,"remaining":30,"reset":%s}}}\n' "$r" "$r" "$r"
  fi
  exit 0
fi
echo "$(date +%s.%N) start $*" >> "$log"
if [ -n "${FAKE_GH_IGNORE_TERM:-}" ]; then trap '' TERM; fi
if [ -n "${FAKE_GH_SIGSTATUS:-}" ]; then grep -E '^Sig(Blk|Ign)' /proc/$$/status > "$FAKE_GH_SIGSTATUS"; fi
if [ -n "${FAKE_GH_STDIN_FILE:-}" ]; then cat > "$FAKE_GH_STDIN_FILE"; fi
if [ -n "${FAKE_GH_NESTED:-}" ]; then
  # Run a nested gh-paced write from inside this call, as a gh alias or extension would.
  n="$FAKE_GH_NESTED"
  FAKE_GH_NESTED= FAKE_GH_SLEEP= "$n" --account test -- issue comment 99 --body nested
  echo "$(date +%s.%N) nested-rc $?" >> "$log"
fi
if [ -n "${FAKE_GH_EDIT:-}" ]; then
  # Open the editor on a file as gh does: split the editor command and append the file name.
  run_editor() { local f="$2"; eval "set -- $1"; "$@" "$f"; }
  ed="${GH_EDITOR:-${GIT_EDITOR:-${VISUAL:-${EDITOR:-nano}}}}"
  printf 'initial\n' > "$FAKE_GH_EDIT"
  echo "$(date +%s.%N) editor $ed" >> "$log"
  if run_editor "$ed" "$FAKE_GH_EDIT"; then
    echo "$(date +%s.%N) edited $(wc -c < "$FAKE_GH_EDIT")" >> "$log"
  else
    rc=$?; echo "$(date +%s.%N) editor-failed $rc" >> "$log"
    echo "could not prompt: editor exit status $rc" >&2; exit 1
  fi
fi
# gh is one process, so nothing it leaves behind holds its streams: the sleep, orphaned when a
# deadline stops this script, must not either.
if [ -n "${FAKE_GH_SLEEP:-}" ]; then sleep "$FAKE_GH_SLEEP" >/dev/null 2>&1; fi
# Write nothing until the file FAKE_GH_GO exists (at most 30 s), so a test can set something up
# first, such as holding the account lock.
if [ -n "${FAKE_GH_GO:-}" ]; then
  i=0; while [ ! -e "$FAKE_GH_GO" ] && [ "$i" -lt 600 ]; do sleep 0.05; i=$((i + 1)); done
fi
# Write to descriptor N without end, dying by SIGPIPE once the reader goes away (as gh would).
if [ -n "${FAKE_GH_FOREVER:-}" ]; then exec yes forever >&"$FAKE_GH_FOREVER"; fi
if [ -n "${FAKE_GH_BG:-}" ]; then
  # Leave a background helper running past gh's exit, holding every inherited descriptor.
  ( sleep "$FAKE_GH_BG"; echo "$(date +%s.%N) bg-end $*" >> "$log" ) >/dev/null 2>&1 &
fi
if [ -n "${FAKE_GH_LATE_ERR:-}" ]; then
  # Leave a helper that keeps gh's stderr after gh exits: quiet for FAKE_GH_LATE_ERR seconds,
  # then FAKE_GH_LATE_LINES lines half a second apart (as a process gh started may write).
  ( sleep "$FAKE_GH_LATE_ERR"; echo "$(date +%s.%N) helper-write" >> "$log"
    i=1; while [ "$i" -le "${FAKE_GH_LATE_LINES:-1}" ]; do
      echo "helper line $i ${FAKE_GH_LATE_TEXT:-}" >&2; i=$((i + 1)); sleep 0.5
    done
    echo "$(date +%s.%N) helper-end" >> "$log" ) </dev/null >/dev/null &
fi
if [ -n "${FAKE_GH_STDOUT_HEAD:-}" ]; then printf '%s' "$FAKE_GH_STDOUT_HEAD"; fi
if [ -n "${FAKE_GH_STDOUT_STEPS:-}" ]; then
  # Write each part (separated by the unit separator, 0x1f) with no line end after it, logging
  # "step N" after part N and pausing a second before the next, as gh writing in pieces would.
  rest="$FAKE_GH_STDOUT_STEPS"; i=0
  while :; do
    part="${rest%%$'\037'*}"; printf '%s' "$part"; i=$((i + 1))
    echo "$(date +%s.%N) step $i" >> "$log"
    [ "$part" = "$rest" ] && break
    rest="${rest#*$'\037'}"; sleep 1
  done
fi
if [ -n "${FAKE_GH_STDOUT_BYTES:-}" ]; then head -c "$FAKE_GH_STDOUT_BYTES" /dev/zero | tr '\0' x; echo; fi
if [ -n "${FAKE_GH_STDOUT:-}" ]; then printf '%s\n' "$FAKE_GH_STDOUT"; fi
if [ -n "${FAKE_GH_HOLD_OUT:-}" ]; then
  # Leave a helper that keeps gh's stdout after gh exits and writes a header-shaped line to it
  # (X-Held: N, CR LF) every 0.4 s, FAKE_GH_HOLD_OUT lines in all.
  ( i=1; while [ "$i" -le "$FAKE_GH_HOLD_OUT" ]; do
      sleep 0.4; printf 'X-Held: %s\r\n' "$i"; i=$((i + 1))
    done ) </dev/null 2>/dev/null &
fi
if [ -n "${FAKE_GH_STDERR:-}" ]; then printf '%s\n' "$FAKE_GH_STDERR" >&2; fi
if [ -n "${FAKE_GH_STDERR_BYTES:-}" ]; then head -c "$FAKE_GH_STDERR_BYTES" /dev/zero | tr '\0' y >&2; echo >&2; fi
echo "$(date +%s.%N) end $*" >> "$log"
if [ -n "${FAKE_GH_SIGNAL:-}" ]; then kill -s "$FAKE_GH_SIGNAL" $$; sleep 5; fi
exit "${FAKE_GH_EXIT:-0}"
"#;

/// A scratch directory with a fake gh, a state directory and a config file.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str, config: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gh-paced-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let gh = dir.join("gh");
        std::fs::write(&gh, FAKE_GH).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("config.json"), config).unwrap();
        std::fs::write(dir.join("gh.log"), "").unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// `gh-paced --account test -- <args>` with a clean environment.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path("home"))
            .env("GH_PACED_REAL_GH", self.path("gh"))
            .env("GH_PACED_STATE_DIR", self.path("state"))
            .env("GH_PACED_CONFIG", self.path("config.json"))
            .env("FAKE_GH_LOG", self.path("gh.log"))
            .args(["--account", "test", "--"])
            .args(args)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.cmd(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    /// `(time, event, args)` lines from the fake gh log.
    fn log(&self) -> Vec<(f64, String, String)> {
        std::fs::read_to_string(self.path("gh.log"))
            .unwrap()
            .lines()
            .map(|l| {
                let mut it = l.splitn(3, ' ');
                let t = it.next().unwrap().parse().unwrap();
                let ev = it.next().unwrap_or("").to_string();
                (t, ev, it.next().unwrap_or("").to_string())
            })
            .collect()
    }

    fn starts(&self) -> Vec<(f64, String)> {
        self.log()
            .into_iter()
            .filter(|(_, ev, _)| ev == "start")
            .map(|(t, _, a)| (t, a))
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Writes 1 per second (burst 1); reads 60/min with burst 10.
const FAST: &str = r#"{"write": {"per_minute": 60, "burst": 1, "per_hour": 30, "max_in_flight": 1},
                      "read": {"per_minute": 60, "burst": 10, "per_hour": 500}}"#;

#[test]
fn exit_code_stdout_and_stderr_pass_through() {
    let sb = Sandbox::new("exit", FAST);
    let o = sb.run(
        &["pr", "view", "5"],
        &[
            ("FAKE_GH_EXIT", "3"),
            ("FAKE_GH_STDOUT", "hello out"),
            ("FAKE_GH_STDERR", "hello err"),
        ],
    );
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    assert_eq!(stdout(&o), "hello out\n");
    assert!(stderr(&o).contains("hello err"), "{}", stderr(&o));
    assert!(
        !stderr(&o).contains("GH-PACED"),
        "a quiet call prints nothing extra: {}",
        stderr(&o)
    );
}

#[test]
fn signal_death_is_reproduced() {
    let sb = Sandbox::new("signal", FAST);
    let o = sb.run(&["pr", "view", "5"], &[("FAKE_GH_SIGNAL", "TERM")]);
    assert_eq!(
        o.status.signal(),
        Some(libc::SIGTERM),
        "{:?} {}",
        o.status,
        stderr(&o)
    );
}

/// `(blocked, ignored)` signal masks the fake gh saw.
fn child_masks(path: &std::path::Path) -> (u64, u64) {
    let text = std::fs::read_to_string(path).unwrap();
    let field = |name: &str| {
        let line = text.lines().find(|l| l.starts_with(name)).unwrap();
        u64::from_str_radix(line.split_whitespace().nth(1).unwrap(), 16).unwrap()
    };
    (field("SigBlk:"), field("SigIgn:"))
}

fn bit(sig: i32) -> u64 {
    1u64 << (sig - 1)
}

#[test]
fn child_gets_default_signal_handling_and_inherits_ignored_signals() {
    let sb = Sandbox::new("sigmask", FAST);
    let status = sb.path("sig.txt");
    let status_s = status.display().to_string();
    let o = sb.run(&["pr", "view", "5"], &[("FAKE_GH_SIGSTATUS", &status_s)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let (blocked, ignored) = child_masks(&status);
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        assert_eq!(
            blocked & bit(sig),
            0,
            "signal {sig} blocked in gh: {blocked:x}"
        );
        assert_eq!(
            ignored & bit(sig),
            0,
            "signal {sig} ignored in gh: {ignored:x}"
        );
    }
    // Started like `nohup gh-paced ...`: SIGHUP ignored, so gh must inherit that too.
    let mut cmd = sb.cmd(&["pr", "view", "6"]);
    cmd.env("FAKE_GH_SIGSTATUS", &status_s);
    // SAFETY: signal() is async-signal-safe; this runs in the forked child before exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let o = cmd.output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let (blocked, ignored) = child_masks(&status);
    assert_ne!(
        ignored & bit(libc::SIGHUP),
        0,
        "SIGHUP no longer ignored in gh: {ignored:x}"
    );
    assert_eq!(blocked & bit(libc::SIGTERM), 0);
}

#[test]
fn stdin_reaches_gh_untouched() {
    let sb = Sandbox::new("stdin", FAST);
    let got = sb.path("stdin.out");
    let got_s = got.display().to_string();
    // A read has no body to inspect, so gh inherits stdin directly. The input is far larger than
    // the guard's read limit (max_body_bytes + 1), so a buffered copy would arrive truncated.
    let big: Vec<u8> = (0..100_000u32)
        .map(|i| b"abcdefgh\n"[(i % 9) as usize])
        .collect();
    let mut child = sb
        .cmd(&["api", "-X", "GET", "repos/o/r/issues", "--input", "-"])
        .env("FAKE_GH_STDIN_FILE", &got_s)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let writer = {
        let big = big.clone();
        std::thread::spawn(move || input.write_all(&big))
    };
    let o = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        std::fs::read(&got).unwrap(),
        big,
        "read input arrived whole"
    );
    // A GraphQL request on stdin may be a mutation, so it is classified WRITE (fail safe): the
    // body is buffered for the content guard and then replayed byte for byte.
    let query = b"{\"query\":\"query { viewer { login } }\"}";
    let mut child = sb
        .cmd(&["api", "graphql", "--input", "-"])
        .env("FAKE_GH_STDIN_FILE", &got_s)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(query).unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(std::fs::read(&got).unwrap(), query);
    // A write body on stdin is buffered the same way.
    let body = b"{\"body\":\"a short prose note\"}\n";
    let mut child = sb
        .cmd(&[
            "api",
            "-X",
            "POST",
            "repos/o/r/issues/1/comments",
            "--input",
            "-",
        ])
        .env("FAKE_GH_STDIN_FILE", &got_s)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body).unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(std::fs::read(&got).unwrap(), body);
    // Both buffered bodies were charged as writes.
    let st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert_eq!(
        st["buckets"]["write"]["window"].as_array().unwrap().len(),
        2,
        "{st}"
    );
}

#[test]
fn content_guard_refuses_before_gh_runs() {
    let sb = Sandbox::new("guard", FAST);
    let encoded: String = (0..3000).map(|i| ["Ab3", "x9Z", "Qq7"][i % 3]).collect();
    let request = sb.path("part.request.json");
    std::fs::write(&request, serde_json::json!({ "body": encoded }).to_string()).unwrap();
    let request = request.display().to_string();
    let o = sb.run(
        &[
            "api",
            "--method",
            "POST",
            "repos/o/r/issues/1/comments",
            "--input",
            &request,
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(65), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("GH-PACED REFUSED [test] content guard"),
        "{err}"
    );
    assert!(err.contains("over the 8192-byte limit"), "{err}");
    assert!(err.contains("base64-looking run"), "{err}");
    let o = sb.run(
        &["pr", "comment", "1", "--body", &"word ".repeat(2000)],
        &[],
    );
    assert_eq!(o.status.code(), Some(65), "{}", stderr(&o));
    assert!(sb.starts().is_empty(), "gh never ran: {:?}", sb.log());
    // The audit log records the refusal without the body.
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    assert!(audit.contains("\"refuse\""), "{audit}");
    assert!(!audit.contains("Ab3x9Z"), "audit must not hold bodies");
    assert!(!audit.contains("word word"), "audit must not hold bodies");
    // An explicit override lets it through, loudly.
    let o = sb.run(
        &["pr", "comment", "1", "--body", &"word ".repeat(2000)],
        &[("GH_PACED_ALLOW_LARGE_BODY", "1")],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("content guard skipped"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn concurrent_processes_share_one_write_budget() {
    let sb = Sandbox::new("shared", FAST);
    let begun = Instant::now();
    let children: Vec<_> = (0..3)
        .map(|i| {
            sb.cmd(&["issue", "comment", &format!("{i}"), "--body", "short note"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let o = child.wait_with_output().unwrap();
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    }
    let mut starts: Vec<f64> = sb.starts().iter().map(|(t, _)| *t).collect();
    starts.sort_by(f64::total_cmp);
    assert_eq!(starts.len(), 3);
    // The fake gh's own timestamps include bash start-up jitter, so the spacing is checked on
    // the admission times gh-paced recorded under the shared lock.
    let st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    let mut admitted: Vec<f64> = st["buckets"]["write"]["window"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e[0].as_f64().unwrap())
        .collect();
    admitted.sort_by(f64::total_cmp);
    assert_eq!(admitted.len(), 3, "{st}");
    for pair in admitted.windows(2) {
        assert!(
            pair[1] - pair[0] >= 1.0 - 1e-6,
            "admitted {} s apart: {admitted:?}",
            pair[1] - pair[0]
        );
    }
    assert!(
        starts[2] - starts[0] >= 1.8,
        "gh runs spread over {} s",
        starts[2] - starts[0]
    );
    assert!(begun.elapsed().as_secs_f64() >= 1.8);
    assert_eq!(
        st["in_flight"].as_array().unwrap().len(),
        0,
        "holders released: {st}"
    );
}

#[test]
fn only_one_write_in_flight_per_account() {
    // Bucket allows 5 at once; the in-flight limit must still serialize them.
    let cfg = r#"{"write": {"per_minute": 60, "burst": 5, "per_hour": 30, "max_in_flight": 1}}"#;
    let sb = Sandbox::new("inflight", cfg);
    let children: Vec<_> = (0..2)
        .map(|i| {
            sb.cmd(&["issue", "comment", &format!("{i}"), "--body", "short note"])
                .env("FAKE_GH_SLEEP", "1.5")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<Output> = children
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .collect();
    for o in &outputs {
        assert_eq!(o.status.code(), Some(0), "{}", stderr(o));
    }
    let log: Vec<(f64, String)> = sb
        .log()
        .into_iter()
        .filter(|(_, ev, _)| ev == "start" || ev == "end")
        .map(|(t, ev, _)| (t, ev))
        .collect();
    let mut sorted = log.clone();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let events: Vec<&str> = sorted.iter().map(|(_, e)| e.as_str()).collect();
    assert_eq!(
        events,
        ["start", "end", "start", "end"],
        "overlapping writes: {sorted:?}"
    );
    let waited = outputs
        .iter()
        .any(|o| stderr(o).contains("1 write(s) already in flight on this host"));
    assert!(
        waited,
        "the second writer announced the wait: {:?}",
        outputs.iter().map(stderr).collect::<Vec<_>>()
    );
}

#[test]
fn sleeps_within_the_bound_and_refuses_beyond_it() {
    let cfg = r#"{"write": {"per_minute": 30, "burst": 1, "per_hour": 30, "max_in_flight": 1}}"#;
    let sb = Sandbox::new("bound", cfg);
    let args = ["pr", "comment", "1", "--body", "short note"];
    let o = sb.run(&args, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    // Next slot is ~2 s away: a 1 s bound refuses without running gh.
    let o = sb.run(&args, &[("GH_PACED_MAX_WAIT", "1")]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("GH-PACED REFUSED [test] write budget 30/min (burst 1) per host reached"),
        "{err}"
    );
    assert!(err.contains("beyond GH_PACED_MAX_WAIT=1 s"), "{err}");
    assert_eq!(sb.starts().len(), 1);
    // The default bound sleeps, warns, then runs.
    let begun = Instant::now();
    let o = sb.run(&args, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("GH-PACED WARNING [test] write budget 30/min (burst 1) per host reached"),
        "{err}"
    );
    assert!(err.contains("sleeping") && err.contains(" ET)"), "{err}");
    assert!(begun.elapsed().as_secs_f64() >= 0.5, "it slept");
    assert_eq!(sb.starts().len(), 2);
}

#[test]
fn github_pushback_starts_a_cooldown() {
    let sb = Sandbox::new("pushback", FAST);
    let o = sb.run(
        &["pr", "list"],
        &[
            ("FAKE_GH_STDERR", "HTTP 403: You have exceeded a secondary rate limit and have been temporarily blocked"),
            ("FAKE_GH_EXIT", "1"),
        ],
    );
    assert_eq!(
        o.status.code(),
        Some(1),
        "gh's status passes through: {}",
        stderr(&o)
    );
    let err = stderr(&o);
    assert!(
        err.contains("secondary rate limit and have been"),
        "gh's own stderr is shown: {err}"
    );
    assert!(err.contains("GH-PACED PUSHBACK [test]"), "{err}");
    assert!(err.contains("Never switch accounts"), "{err}");
    for args in [
        &["pr", "view", "1"][..],
        &["pr", "comment", "1", "--body", "x"],
        &["auth", "git-credential", "get"],
    ] {
        let o = sb.run(args, &[("GH_PACED_MAX_WAIT", "5")]);
        assert_eq!(o.status.code(), Some(75), "{args:?}: {}", stderr(&o));
        assert!(stderr(&o).contains("cooldown"), "{}", stderr(&o));
    }
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.log());
    // Status shows the cooldown.
    let o = Command::new(BIN)
        .env_clear()
        .env("HOME", sb.path("home"))
        .env("GH_PACED_STATE_DIR", sb.path("state"))
        .env("GH_PACED_CONFIG", sb.path("config.json"))
        .args(["status", "--account", "test"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("COOLDOWN:"), "{}", stdout(&o));
}

/// During a cooldown the git credential helper is refused at once, with one line, and prints
/// `quit=1` so git stops instead of prompting: with the default 900 s GH_PACED_MAX_WAIT it used
/// to sleep out the whole cooldown inside git.
#[test]
fn credential_helper_is_refused_at_once_during_a_cooldown() {
    let sb = Sandbox::new("cred-cooldown", FAST);
    let o = sb.run(
        &["pr", "list"],
        &[
            ("FAKE_GH_STDERR", "HTTP 403: You have exceeded a secondary rate limit and have been temporarily blocked"),
            ("FAKE_GH_EXIT", "1"),
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let started = Instant::now();
    let o = sb.run(&["auth", "git-credential", "get"], &[]);
    let took = started.elapsed().as_secs_f64();
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(75), "{err}");
    assert!(took < 10.0, "took {took} s: {err}");
    assert_eq!(stdout(&o), "quit=1\n", "{err}");
    let refused: Vec<&str> = err.lines().filter(|l| l.contains("REFUSED")).collect();
    assert_eq!(refused.len(), 1, "{err}");
    assert!(
        refused[0].contains("cooldown after GitHub pushback"),
        "{err}"
    );
    assert!(refused[0].contains(" s left (ends "), "{err}");
    assert_eq!(err.lines().count(), 1, "one line: {err}");
    assert_eq!(sb.starts().len(), 1, "gh did not run: {:?}", sb.log());
    // Other classes are not told to quit, and still refuse against GH_PACED_MAX_WAIT.
    let o = sb.run(&["pr", "view", "1"], &[("GH_PACED_MAX_WAIT", "5")]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
    assert!(
        stderr(&o).contains("beyond GH_PACED_MAX_WAIT=5 s"),
        "{}",
        stderr(&o)
    );
}

/// GH_PACED_GIT_MAX_WAIT bounds a credential call's budget wait: beyond it the call is refused
/// at once with `quit=1`, and within it the call waits and runs.
#[test]
fn credential_helper_wait_is_bounded_by_its_own_knob() {
    let sb = Sandbox::new(
        "cred-bound",
        r#"{"git_credential": {"per_minute": 60, "burst": 1, "per_hour": 120}}"#,
    );
    let o = sb.run(&["auth", "git-credential", "get"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
    let o = sb.run(
        &["auth", "git-credential", "get"],
        &[("GH_PACED_GIT_MAX_WAIT", "0")],
    );
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(75), "{err}");
    assert_eq!(stdout(&o), "quit=1\n", "{err}");
    assert!(err.contains("beyond GH_PACED_GIT_MAX_WAIT=0 s"), "{err}");
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.log());
    let o = sb.run(&["auth", "git-credential", "get"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
    assert_eq!(sb.starts().len(), 2, "{:?}", sb.log());
}

/// Every failure before gh starts tells git to quit, so git never falls back to prompting for a
/// password; a failure after gh started, and any other command, prints nothing on stdout.
#[test]
fn credential_helper_quits_on_every_failure_before_gh_starts() {
    let sb = Sandbox::new("cred-quit", FAST);
    let get = ["auth", "git-credential", "get"];
    // Nesting depth.
    let o = sb.run(&get, &[("GH_PACED_DEPTH", "8")]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    assert_eq!(stdout(&o), "quit=1\n", "{}", stderr(&o));
    let o = sb.run(&["pr", "list"], &[("GH_PACED_DEPTH", "8")]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
    // A configuration error.
    let missing = sb.path("no-such-config.json");
    let missing = missing.to_str().unwrap();
    let o = sb.run(&get, &[("GH_PACED_CONFIG", missing)]);
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert_eq!(stdout(&o), "quit=1\n", "{}", stderr(&o));
    let o = sb.run(&["pr", "list"], &[("GH_PACED_CONFIG", missing)]);
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
    assert!(sb.starts().is_empty(), "{:?}", sb.log());
    // gh ran and failed: its own output is all git sees.
    let o = sb.run(&get, &[("FAKE_GH_EXIT", "1")]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.log());
}

/// A credential call waits for the account lock only within GH_PACED_GIT_MAX_WAIT, not the
/// 30 s GH_PACED_LOCK_WAIT, then exits 70 with `quit=1`; other commands still wait the whole
/// GH_PACED_LOCK_WAIT.
#[test]
fn credential_helper_waits_for_the_account_lock_only_within_its_bound() {
    let sb = Sandbox::new("cred-lock", FAST);
    let held = hold_account_lock(&sb);
    let started = Instant::now();
    let o = sb.run(
        &["auth", "git-credential", "get"],
        &[("GH_PACED_GIT_MAX_WAIT", "1")],
    );
    let took = started.elapsed().as_secs_f64();
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(70), "{err}");
    assert_eq!(stdout(&o), "quit=1\n", "{err}");
    assert!((1.0..1.5).contains(&took), "took {took} s: {err}");
    assert!(err.contains("GH_PACED_GIT_MAX_WAIT=1 s bound"), "{err}");
    let started = Instant::now();
    let o = sb.run(&["pr", "list"], &[("GH_PACED_LOCK_WAIT", "2")]);
    let took = started.elapsed().as_secs_f64();
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(70), "{err}");
    assert_eq!(stdout(&o), "");
    assert!(took >= 2.0, "took {took} s: {err}");
    assert!(!err.contains("GH_PACED_GIT_MAX_WAIT"), "{err}");
    drop(held);
    assert!(sb.starts().is_empty(), "{:?}", sb.log());
}

/// Refusing a credential call because gh's configuration cannot be read is recorded under the
/// account lock, and that wait is held to GH_PACED_GIT_MAX_WAIT too: the bound counts from the
/// start of the call, not from after the aliases are read.
#[test]
fn credential_helper_refusing_an_alias_waits_for_the_lock_only_within_its_bound() {
    let sb = Sandbox::new("cred-alias-lock", FAST);
    write_gh_config(&sb, "aliases:\n  up: !!str api -X POST x --input -\n");
    let get = ["auth", "git-credential", "get"];
    let held = hold_account_lock(&sb);
    let started = Instant::now();
    let o = sb.run(&get, &[("GH_PACED_GIT_MAX_WAIT", "1")]);
    let took = started.elapsed().as_secs_f64();
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(70), "{err}");
    assert_eq!(stdout(&o), "quit=1\n", "{err}");
    assert!((1.0..1.5).contains(&took), "took {took} s: {err}");
    assert!(err.contains("GH_PACED_GIT_MAX_WAIT=1 s bound"), "{err}");
    drop(held);
    let o = sb.run(&get, &[("GH_PACED_GIT_MAX_WAIT", "1")]);
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(78), "{err}");
    assert_eq!(stdout(&o), "quit=1\n", "{err}");
    assert!(err.contains("gh's configuration cannot be read"), "{err}");
    assert!(sb.starts().is_empty(), "{:?}", sb.log());
}

/// A credential call whose gh cannot be executed (no execute permission, or an interpreter that
/// does not exist) exits 127 and tells git to quit, as gh never answered.
#[test]
fn credential_helper_quits_when_gh_cannot_be_executed() {
    let sb = Sandbox::new("cred-exec", FAST);
    let get = ["auth", "git-credential", "get"];
    let plain = sb.path("gh-not-executable");
    std::fs::write(&plain, FAKE_GH).unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    let orphan = sb.path("gh-no-interpreter");
    std::fs::write(&orphan, "#!/nonexistent/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o755)).unwrap();
    for gh in [&plain, &orphan] {
        let o = sb.run(&get, &[("GH_PACED_REAL_GH", gh.to_str().unwrap())]);
        let err = stderr(&o);
        assert_eq!(o.status.code(), Some(127), "{gh:?}: {err}");
        assert_eq!(stdout(&o), "quit=1\n", "{gh:?}: {err}");
        assert!(err.contains("cannot run"), "{gh:?}: {err}");
    }
    assert!(sb.starts().is_empty(), "{:?}", sb.log());
}

#[test]
fn local_commands_run_without_touching_state() {
    let sb = Sandbox::new("local", FAST);
    let o = sb.run(&["--version"], &[("FAKE_GH_STDOUT", "gh version 9.9.9")]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "gh version 9.9.9\n");
    let entries: Vec<_> = std::fs::read_dir(sb.path("state")).unwrap().collect();
    assert!(entries.is_empty(), "LOCAL left state behind: {entries:?}");
}

#[test]
fn surface_errors_have_distinct_exit_codes() {
    let sb = Sandbox::new("errors", FAST);
    // No account.
    let o = Command::new(BIN)
        .env_clear()
        .env("GH_PACED_REAL_GH", sb.path("gh"))
        .env("GH_PACED_STATE_DIR", sb.path("state"))
        .args(["--", "pr", "list"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(64), "{}", stderr(&o));
    // The "real gh" is gh-paced itself: refused instead of recursing.
    let o = sb.run(&["pr", "list"], &[("GH_PACED_REAL_GH", BIN)]);
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    // Nesting depth.
    let o = sb.run(&["pr", "list"], &[("GH_PACED_DEPTH", "8")]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    // A loosening env override is ignored with a warning, a malformed one is a config error.
    let o = sb.run(&["pr", "list"], &[("GH_PACED_WRITE_PER_HOUR", "lots")]);
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(sb.starts().is_empty());
}

#[test]
fn help_classify_and_status_subcommands() {
    let sb = Sandbox::new("help", FAST);
    for args in [
        &["--help"][..],
        &["status", "--help"],
        &["classify", "--help"],
    ] {
        let o = Command::new(BIN).env_clear().args(args).output().unwrap();
        assert_eq!(o.status.code(), Some(0), "{args:?}");
        let text = stdout(&o);
        assert!(
            text.contains("EXAMPLES") || text.contains("Examples"),
            "{args:?}: {text}"
        );
    }
    let o = Command::new(BIN).env_clear().output().unwrap();
    assert_eq!(
        o.status.code(),
        Some(64),
        "no arguments prints help and fails"
    );
    let o = Command::new(BIN)
        .env_clear()
        .env("HOME", sb.path("home"))
        .env("GH_PACED_CONFIG", sb.path("config.json"))
        .args([
            "classify",
            "--json",
            "--",
            "api",
            "-X",
            "PATCH",
            "repos/o/r/issues/1",
            "-f",
            "state=closed",
        ])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["class"], "write");
    assert!(sb.starts().is_empty(), "classify never runs gh");
    // One read, then status reports it.
    let o = sb.run(&["pr", "view", "1"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let o = Command::new(BIN)
        .env_clear()
        .env("HOME", sb.path("home"))
        .env("GH_PACED_STATE_DIR", sb.path("state"))
        .env("GH_PACED_CONFIG", sb.path("config.json"))
        .args(["status", "--account", "test", "--json"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(
        v["classes"]["read"]["hour_used"], 2,
        "1 call + 1 rate_limit refresh: {v}"
    );
    assert_eq!(
        v["rate_limit"]["resources"]["core"]["remaining"], 4990,
        "{v}"
    );
}

/// `status` reads a state directory it cannot write to, creates no lock file there, and creates
/// no state directory for an account that has none.
#[test]
fn status_takes_no_lock_and_creates_nothing() {
    let sb = Sandbox::new("status-ro", FAST);
    let o = sb.run(&["pr", "view", "1"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let state = sb.path("state");
    let lock = state.join("test.lock");
    std::fs::remove_file(&lock).expect("the paced call created the lock file");
    let status = |dir: &std::path::Path| {
        Command::new(BIN)
            .env_clear()
            .env("HOME", sb.path("home"))
            .env("GH_PACED_STATE_DIR", dir)
            .env("GH_PACED_CONFIG", sb.path("config.json"))
            .args(["status", "--account", "test", "--json"])
            .output()
            .unwrap()
    };
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).unwrap();
    let o = status(&state);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["classes"]["read"]["hour_used"], 2, "{v}");
    assert!(!lock.exists(), "status created {}", lock.display());
    let missing = sb.path("no-such-state");
    let o = status(&missing);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!missing.exists(), "status created {}", missing.display());
}

/// Secrets in an endpoint's query string reach gh unchanged but never the audit log, the state
/// file or gh-paced's own messages.
#[test]
fn query_string_secrets_stay_out_of_the_records() {
    let sb = Sandbox::new("canary", FAST);
    let o = sb.run(
        &[
            "api",
            "repos/o/r/issues?access_token=github_pat_CANARY_READ",
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let o2 = sb.run(
        &[
            "api",
            "-X",
            "POST",
            "https://api.github.com/repos/o/r/issues?access_token=github_pat_CANARY_WRITE",
            "-f",
            "title=short",
        ],
        &[],
    );
    assert_eq!(o2.status.code(), Some(0), "{}", stderr(&o2));
    // An alias or extension: its positional arguments may be a body or a secret.
    let o3 = sb.run(
        &["my-alias", "CANARY_ALIAS secret body", "--flag=CANARY_FLAG"],
        &[],
    );
    assert_eq!(o3.status.code(), Some(0), "{}", stderr(&o3));
    let log = std::fs::read_to_string(sb.path("gh.log")).unwrap();
    assert!(
        log.contains("CANARY_ALIAS") && log.contains("CANARY_FLAG"),
        "{log}"
    );
    assert!(
        log.contains("CANARY_READ") && log.contains("CANARY_WRITE"),
        "gh got the real args: {log}"
    );
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    assert!(audit.contains("\"admit\""), "{audit}");
    let state = std::fs::read_to_string(sb.path("state/test.json")).unwrap();
    for (what, text) in [
        ("audit", audit.as_str()),
        ("state", state.as_str()),
        ("stderr", &stderr(&o)),
        ("stderr", &stderr(&o2)),
        ("stderr", &stderr(&o3)),
    ] {
        assert!(!text.contains("CANARY"), "{what} holds a secret: {text}");
        assert!(
            !text.contains("access_token"),
            "{what} holds the query: {text}"
        );
    }
}

fn wait_for_log(sb: &Sandbox, needle: &str) {
    let begun = Instant::now();
    while !std::fs::read_to_string(sb.path("gh.log"))
        .unwrap()
        .contains(needle)
    {
        assert!(
            begun.elapsed().as_secs_f64() < 15.0,
            "never saw {needle:?}: {:?}",
            sb.log()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Time of the first `event` line whose arguments start with `args`.
fn event_time(sb: &Sandbox, event: &str, args: &str) -> f64 {
    sb.log()
        .into_iter()
        .find(|(_, ev, a)| ev == event && a.starts_with(args))
        .map(|(t, _, _)| t)
        .unwrap_or_else(|| panic!("no {event} {args}: {:?}", sb.log()))
}

fn lease_files(sb: &Sandbox) -> Vec<String> {
    std::fs::read_dir(sb.path("state"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".lease-"))
        .collect()
}

/// Five writes may start at once by the bucket; only the in-flight limit separates them.
const BURSTY: &str =
    r#"{"write": {"per_minute": 60, "burst": 5, "per_hour": 30, "max_in_flight": 1}}"#;

/// A write's slot lasts as long as gh does, not as long as the wrapper: when the wrapper is killed
/// with SIGKILL its orphaned gh keeps the lease file locked, so the next write waits for gh.
#[test]
fn killed_wrapper_leaves_its_orphaned_gh_holding_the_write_slot() {
    let sb = Sandbox::new("orphan", BURSTY);
    let mut first = sb
        .cmd(&["issue", "comment", "1", "--body", "orphaned"])
        .env("FAKE_GH_SLEEP", "3")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_log(&sb, "start issue comment 1");
    first.kill().unwrap();
    first.wait().unwrap();
    assert_eq!(lease_files(&sb).len(), 1, "the lease outlives the wrapper");
    let o = sb.run(&["issue", "comment", "2", "--body", "next"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let orphan_end = event_time(&sb, "end", "issue comment 1");
    let next_start = event_time(&sb, "start", "issue comment 2");
    assert!(
        next_start >= orphan_end,
        "second write started {} s before the orphan finished",
        orphan_end - next_start
    );
    assert!(
        stderr(&o).contains("1 write(s) already in flight on this host"),
        "{}",
        stderr(&o)
    );
    assert!(lease_files(&sb).is_empty(), "{:?}", lease_files(&sb));
    let st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert_eq!(st["in_flight"].as_array().unwrap().len(), 0, "{st}");
}

/// A gh-paced started by gh itself (an alias or extension) skips the in-flight wait for its
/// parent's write, because it holds the parent's lease file descriptor. Naming the parent's
/// nonce in `GH_PACED_INFLIGHT_CHAIN` without that descriptor is not enough: such a process
/// waits like any other.
#[test]
fn nesting_needs_the_inherited_lease_not_just_the_chain() {
    let sb = Sandbox::new("nested", BURSTY);
    let outer = sb
        .cmd(&["issue", "comment", "1", "--body", "outer"])
        .env("FAKE_GH_NESTED", BIN)
        .env("FAKE_GH_SLEEP", "3")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_log(&sb, "nested-rc");
    let st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    let holders = st["in_flight"].as_array().unwrap();
    assert_eq!(holders.len(), 1, "only the outer write is in flight: {st}");
    let nonce = holders[0]["nonce"].as_str().unwrap().to_string();
    let forged = sb.run(
        &["issue", "comment", "2", "--body", "forged"],
        &[("GH_PACED_INFLIGHT_CHAIN", &nonce)],
    );
    let outer = outer.wait_with_output().unwrap();
    assert_eq!(outer.status.code(), Some(0), "{}", stderr(&outer));
    assert_eq!(forged.status.code(), Some(0), "{}", stderr(&forged));
    let log = sb.log();
    let nested_rc = log
        .iter()
        .find(|(_, ev, _)| ev == "nested-rc")
        .map(|(_, _, a)| a.clone());
    assert_eq!(nested_rc.as_deref(), Some("0"), "{log:?}");
    let outer_start = event_time(&sb, "start", "issue comment 1");
    let outer_end = event_time(&sb, "end", "issue comment 1");
    let nested_start = event_time(&sb, "start", "issue comment 99");
    assert!(
        nested_start > outer_start && nested_start < outer_end,
        "the nested write ran inside the outer one: {log:?}"
    );
    assert!(
        !stderr(&outer).contains("already in flight"),
        "the nested write did not wait: {}",
        stderr(&outer)
    );
    let forged_start = event_time(&sb, "start", "issue comment 2");
    assert!(
        forged_start >= outer_end,
        "the forged chain skipped the wait: {log:?}"
    );
    assert!(
        stderr(&forged).contains("1 write(s) already in flight on this host"),
        "{}",
        stderr(&forged)
    );
}

/// Opening a write's lease file again is not the inherited lease: the new descriptor holds no
/// lock. A process that names the write's nonce in `GH_PACED_INFLIGHT_CHAIN` and passes such a
/// descriptor waits for the write like any other.
#[test]
fn reopening_the_lease_file_does_not_prove_descent() {
    let sb = Sandbox::new("reopen", BURSTY);
    let outer = sb
        .cmd(&["issue", "comment", "1", "--body", "outer"])
        .env("FAKE_GH_SLEEP", "3")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_log(&sb, "start issue comment 1");
    let leases = lease_files(&sb);
    assert_eq!(leases.len(), 1, "{leases:?}");
    let st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    let nonce = st["in_flight"][0]["nonce"].as_str().unwrap().to_string();
    let reopened = std::fs::File::open(sb.path("state").join(&leases[0])).unwrap();
    let forged = sb
        .cmd(&["issue", "comment", "2", "--body", "forged"])
        .env("GH_PACED_INFLIGHT_CHAIN", &nonce)
        .stdin(Stdio::from(reopened))
        .output()
        .unwrap();
    let outer = outer.wait_with_output().unwrap();
    assert_eq!(outer.status.code(), Some(0));
    assert_eq!(forged.status.code(), Some(0), "{}", stderr(&forged));
    let outer_end = event_time(&sb, "end", "issue comment 1");
    let forged_start = event_time(&sb, "start", "issue comment 2");
    assert!(
        forged_start >= outer_end,
        "the reopened lease skipped the wait: {:?}",
        sb.log()
    );
    assert!(
        stderr(&forged).contains("1 write(s) already in flight on this host"),
        "{}",
        stderr(&forged)
    );
}

/// A write's slot lasts as long as anything gh started still holds the lease: when gh leaves a
/// background helper running, the wrapper keeps the holder and the lease file, says so, and the
/// next write waits for the helper. The lease is reaped once the helper exits.
#[test]
fn background_helper_keeps_the_write_slot() {
    let sb = Sandbox::new("bg-holder", BURSTY);
    let first = sb.run(
        &["issue", "comment", "1", "--body", "first"],
        &[("FAKE_GH_BG", "3")],
    );
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert!(
        stderr(&first).contains("a process it started still holds this write's slot"),
        "{}",
        stderr(&first)
    );
    assert_eq!(lease_files(&sb).len(), 1, "the lease outlives gh");
    let st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert_eq!(st["in_flight"].as_array().unwrap().len(), 1, "{st}");
    let second = sb.run(&["issue", "comment", "2", "--body", "second"], &[]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let bg_end = event_time(&sb, "bg-end", "issue comment 1");
    let second_start = event_time(&sb, "start", "issue comment 2");
    assert!(
        second_start >= bg_end,
        "the second write started {} s before the helper exited",
        bg_end - second_start
    );
    assert!(
        stderr(&second).contains("1 write(s) already in flight on this host"),
        "{}",
        stderr(&second)
    );
    assert!(lease_files(&sb).is_empty(), "{:?}", lease_files(&sb));
    let st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert_eq!(st["in_flight"].as_array().unwrap().len(), 0, "{st}");
}

/// A watch loop is stopped once it has polled for as long as its up-front charge covers, and the
/// wrapper exits 75.
#[test]
fn watch_past_its_deadline_is_stopped() {
    // 6 tokens: 2 for startup (resolving the pull request), then 2 polls of 2 requests, 1 s
    // apart: a 2 s budget.
    let cfg = r#"{"watch_cost": 6, "min_watch_interval_secs": 0,
                  "read": {"per_minute": 60, "burst": 10, "per_hour": 500}}"#;
    let sb = Sandbox::new("deadline", cfg);
    let begun = Instant::now();
    // The fake gh's `sleep`, orphaned when the deadline stops gh, holds neither stream, as the
    // real gh (one process) leaves nothing behind. A descendant that did hold stderr would be
    // relayed by the drainer until it exits (see the late-output tests), and the caller would
    // see end of file only then.
    let o = sb
        .cmd(&["pr", "checks", "1", "--watch", "--interval", "1"])
        .env("GH_PACED_ALLOW_WATCH", "1")
        .env("FAKE_GH_SLEEP", "8")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let took = begun.elapsed().as_secs_f64();
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("GH-PACED DEADLINE [test]"), "{err}");
    assert!(err.contains("ran past its 2 s budget"), "{err}");
    // The per-poll request count is an estimate, and the banner says so.
    assert!(
        err.contains("is estimated to cover that many polls"),
        "{err}"
    );
    assert!((1.9..7.5).contains(&took), "stopped after {took} s");
    assert!(
        !sb.log()
            .iter()
            .any(|(_, ev, a)| ev == "end" && a.starts_with("pr checks")),
        "gh was stopped before it finished: {:?}",
        sb.log()
    );
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    assert!(audit.contains("\"deadline\""), "{audit}");
}

/// A watch that ignores SIGTERM at its deadline is sent SIGKILL `KILL_GRACE_SECS` (5 s) later,
/// so a stuck gh cannot keep polling past what it paid for.
#[test]
fn watch_that_ignores_term_is_killed() {
    let cfg = r#"{"watch_cost": 6, "min_watch_interval_secs": 0,
                  "read": {"per_minute": 60, "burst": 10, "per_hour": 500}}"#;
    let sb = Sandbox::new("deadline-kill", cfg);
    let begun = Instant::now();
    let o = sb
        .cmd(&["pr", "checks", "1", "--watch", "--interval", "1"])
        .env("GH_PACED_ALLOW_WATCH", "1")
        .env("FAKE_GH_SLEEP", "30")
        .env("FAKE_GH_IGNORE_TERM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let took = begun.elapsed().as_secs_f64();
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("ran past its 2 s budget"), "{err}");
    // TERM at 2 s is ignored; KILL follows 5 s later. Without the escalation the wrapper would
    // wait for the full 30 s sleep.
    assert!((6.9..20.0).contains(&took), "stopped after {took} s");
    assert!(
        !sb.log()
            .iter()
            .any(|(_, ev, a)| ev == "end" && a.starts_with("pr checks")),
        "gh was killed before it finished: {:?}",
        sb.log()
    );
}

/// `gh api --include` prints the response headers on stdout. A Retry-After there sets the
/// cooldown, and stdout still passes through byte for byte. Without `--include` the same text is
/// a response body and is not read as headers.
#[test]
fn include_headers_on_stdout_set_the_cooldown() {
    let sb = Sandbox::new("include", FAST);
    let response = "HTTP/2.0 403 Forbidden\r\nRetry-After: 3600\r\nX-Ratelimit-Remaining: 0\r\n\r\n{\"message\":\"You have exceeded a secondary rate limit\"}";
    let env = [("FAKE_GH_STDOUT", response), ("FAKE_GH_EXIT", "1")];
    let state = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap()).unwrap()
    };
    let o = sb.run(&["api", "repos/o/r/issues"], &env);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{response}\n"));
    assert!(
        state()["cooldown"].is_null(),
        "body text is not headers: {}",
        state()
    );
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let o = sb.run(&["api", "-i", "repos/o/r/issues"], &env);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{response}\n"), "stdout passes through");
    assert!(
        stderr(&o).contains("GH-PACED PUSHBACK [test]"),
        "{}",
        stderr(&o)
    );
    let until = state()["cooldown"]["until"].as_f64().unwrap();
    assert!(
        until - before >= 3599.0 && until - before < 3700.0,
        "cooldown {} s",
        until - before
    );
}

/// A long `Retry-After` is never shortened: two days are recorded as two days (one day before),
/// and a wait longer than gh-paced counts down (365 days) starts a cooldown with no end time,
/// which refuses every later paced call with a clear message instead of ending early.
#[test]
fn a_long_retry_after_is_never_shortened() {
    let state = |sb: &Sandbox, name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(sb.path(name)).unwrap()).unwrap()
    };
    let sb = Sandbox::new("retry-after-long", FAST);
    let before = epoch_now();
    let two_days = "HTTP/2.0 429 Too Many Requests\r\nRetry-After: 172800\r\n\r\n{}";
    let o = sb.run(
        &["api", "-i", "repos/o/r"],
        &[("FAKE_GH_STDOUT", two_days), ("FAKE_GH_EXIT", "1")],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let until = state(&sb, "state/test.json")["cooldown"]["until"]
        .as_f64()
        .unwrap();
    assert!(
        until - before >= 172_799.0 && until - before < 172_900.0,
        "cooldown {} s",
        until - before
    );
    let o = sb.run(&["api", "repos/o/r"], &[]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.starts());

    let sb = Sandbox::new("retry-after-no-end", FAST);
    let never = "HTTP/2.0 429 Too Many Requests\r\nRetry-After: 99999999999\r\n\r\n{}";
    let o = sb.run(
        &["api", "-i", "repos/o/r"],
        &[("FAKE_GH_STDOUT", never), ("FAKE_GH_EXIT", "1")],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("refused from now on: the cooldown has no end time"),
        "{}",
        stderr(&o)
    );
    let until = state(&sb, "state/test.json")["cooldown"]["until"].as_f64();
    assert_eq!(until, Some(f64::MAX), "the cooldown never ends by itself");
    let until = state(&sb, "state/test.cooldown")["until"].as_f64();
    assert_eq!(until, Some(f64::MAX));
    let o = sb.run(&["api", "repos/o/r"], &[]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("the cooldown has no end time"),
        "{}",
        stderr(&o)
    );
    assert_eq!(sb.starts().len(), 1, "gh ran: {:?}", sb.starts());
    let status = |json: bool| {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("HOME", sb.path("home"))
            .env("GH_PACED_STATE_DIR", sb.path("state"))
            .env("GH_PACED_CONFIG", sb.path("config.json"))
            .args(["status", "--account", "test"]);
        if json {
            c.arg("--json");
        }
        c.output().unwrap()
    };
    let o = status(true);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["cooldown"]["remaining_secs"].is_null(), "{v}");
    assert_eq!(v["cooldown"]["until"], "no end time", "{v}");
    let o = status(false);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("COOLDOWN: no end time"),
        "{}",
        stdout(&o)
    );
}

/// Run one `gh api --include` call whose fake output (written through the fake gh's variable
/// `var`) holds an over-long header line in a 429 or 403 response, and check that it starts a
/// cooldown with no end time, never a shorter one: the banner names the two things a person
/// removes to end it, both files record no end, and the next paced call is refused without
/// running gh.
fn assert_overlong_header_starts_a_no_end_cooldown(name: &str, var: &str, out: &str) {
    let state = |sb: &Sandbox, name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(sb.path(name)).unwrap()).unwrap()
    };
    for (mode, args) in [
        ("single", &["api", "-i", "repos/o/r"][..]),
        ("paginate", &["api", "-i", "--paginate", "repos/o/r"][..]),
    ] {
        let sb = Sandbox::new(&format!("{name}-{mode}"), FAST);
        let o = sb.run(args, &[(var, out), ("FAKE_GH_EXIT", "1")]);
        let err = stderr(&o);
        assert_eq!(o.status.code(), Some(1), "{mode}: {err}");
        assert!(
            err.contains("refused from now on: the cooldown has no end time"),
            "{mode}: {err}"
        );
        assert!(err.contains("longer than the 8192 bytes"), "{mode}: {err}");
        for file in ["state/test.cooldown", "state/test.json"] {
            let path = sb.path(file).display().to_string();
            assert!(err.contains(&path), "{mode}: {path} not named: {err}");
        }
        let until = state(&sb, "state/test.json")["cooldown"]["until"].as_f64();
        assert_eq!(
            until,
            Some(f64::MAX),
            "{mode}: the cooldown never ends by itself"
        );
        let until = state(&sb, "state/test.cooldown")["until"].as_f64();
        assert_eq!(until, Some(f64::MAX), "{mode}");
        let o = sb.run(&["api", "repos/o/r"], &[]);
        assert_eq!(o.status.code(), Some(75), "{mode}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("the cooldown has no end time"),
            "{mode}: {}",
            stderr(&o)
        );
        assert_eq!(sb.starts().len(), 1, "{mode}: gh ran: {:?}", sb.starts());
    }
}

/// A `Retry-After` of two days written behind 9,000 leading zeros is longer than the 8,192-byte
/// header line gh-paced reads. With and without `--paginate` it starts a cooldown with no end
/// time, never the 900 s floor and never no cooldown at all.
#[test]
fn a_retry_after_with_leading_zeros_never_shortens_a_wait() {
    let out = format!(
        "HTTP/2.0 429 Too Many Requests\r\nRetry-After: {}172800\r\n\r\n{{}}",
        "0".repeat(9000)
    );
    assert_overlong_header_starts_a_no_end_cooldown("retry-after-zeros", "FAKE_GH_STDOUT", &out);
}

/// An over-long header line before a valid two-day `Retry-After` in a 403 response: the wait
/// cannot be trusted to have been read, so the cooldown has no end time.
#[test]
fn an_overlong_header_before_retry_after_never_shortens_a_wait() {
    let out = format!(
        "HTTP/2.0 403 Forbidden\r\nX-Long: {}\r\nRetry-After: 172800\r\n\r\n{{}}",
        "x".repeat(9000)
    );
    assert_overlong_header_starts_a_no_end_cooldown("overlong-header", "FAKE_GH_STDOUT", &out);
}

/// Output that ends inside an over-long `Retry-After` line, before its line end arrives (gh
/// stopped mid-write): the wait is not known, so with and without `--paginate` the cooldown has
/// no end time. (Before, the paginated call dropped the block and recorded no cooldown at all.)
#[test]
fn output_cut_inside_an_overlong_header_never_shortens_a_wait() {
    let out = format!(
        "HTTP/2.0 429 Too Many Requests\r\nRetry-After: {}172800",
        "0".repeat(9000)
    );
    // FAKE_GH_STDOUT_HEAD is written with no line end after it.
    assert_overlong_header_starts_a_no_end_cooldown("cut-overlong", "FAKE_GH_STDOUT_HEAD", &out);
}

/// `gh api --include --paginate --jq` prints each page's header block, then the page's body
/// through jq. A body holding a status-shaped line, then an over-long line that ends in CR LF but
/// has no colon (a header line needs one), is body text: no cooldown starts and the next call
/// runs. (Round 9 read the long line as a header and started a cooldown with no end time.)
#[test]
fn a_colon_free_long_body_line_starts_no_cooldown() {
    let sb = Sandbox::new("jq-long-body", FAST);
    let response = format!(
        "HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 4000\r\n\r\nHTTP/2.0 429 Too Many Requests\n{}\r\n\r\n",
        "x".repeat(9000)
    );
    let o = sb.run(
        &["api", "-i", "--paginate", "--jq", ".[]", "repos/o/r"],
        &[("FAKE_GH_STDOUT", &response)],
    );
    let err = stderr(&o);
    assert!(
        !sb.path("state/test.cooldown").exists(),
        "a cooldown record was written: {err}"
    );
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert!(
        stdout(&o) == format!("{response}\n"),
        "stdout passes through"
    );
    assert!(!err.contains("PUSHBACK"), "{err}");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert!(state["cooldown"].is_null(), "{state}");
    let o = sb.run(&["api", "repos/o/r"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(sb.starts().len(), 2, "{:?}", sb.starts());
}

/// The rate-limit refresh keeps the first 1 MiB of gh's stderr. A two-day `Retry-After` cut off
/// at that limit after its first digit must not be read as 1 s, which gave the 900 s floor: the
/// cut is reported, and the cooldown has no end time.
#[test]
fn a_retry_after_cut_by_the_capture_limit_never_shortens_a_wait() {
    let state = |sb: &Sandbox, name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(sb.path(name)).unwrap()).unwrap()
    };
    let sb = Sandbox::new("refresh-cut", FAST);
    let tail = "\nHTTP 429\nRetry-After: 1";
    let mut text = "y".repeat((1 << 20) - tail.len());
    text.push_str(tail);
    assert_eq!(text.len(), 1 << 20, "the cut falls after the first digit");
    text.push_str("72800\n");
    let file = sb.path("rate-stderr");
    std::fs::write(&file, &text).unwrap();
    let before = epoch_now();
    // A paced read is due a refresh first (there is no snapshot yet). GH_PACED_MAX_WAIT keeps a
    // finite cooldown from holding the call.
    let o = sb.run(
        &["pr", "view", "1"],
        &[
            ("FAKE_GH_RATE_STDERR_FILE", file.to_str().unwrap()),
            ("GH_PACED_MAX_WAIT", "5"),
        ],
    );
    let err = stderr(&o);
    let until = state(&sb, "state/test.json")["cooldown"]["until"]
        .as_f64()
        .unwrap_or_else(|| panic!("no cooldown: {err}"));
    assert_eq!(
        until,
        f64::MAX,
        "cooldown of {} s, not one with no end time: {err}",
        until - before
    );
    let until = state(&sb, "state/test.cooldown")["until"].as_f64();
    assert_eq!(until, Some(f64::MAX));
    assert!(
        err.contains(
            "rate-limit refresh: gh's stderr ran past the 1048576 bytes gh-paced reads and was \
             cut off there"
        ),
        "{err}"
    );
    assert!(err.contains("stopped reading stderr"), "{err}");
    assert!(err.contains("the cooldown has no end time"), "{err}");
    assert_eq!(o.status.code(), Some(75), "{err}");
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    // The audit record of the refresh names the pushback and the cut.
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    let refresh = audit
        .lines()
        .find(|l| l.contains("\"refresh\""))
        .unwrap_or_else(|| panic!("no refresh record: {audit}"));
    assert!(refresh.contains("Retry-After cut off"), "{refresh}");
    assert!(
        refresh.contains("stderr ran past the 1048576 bytes"),
        "{refresh}"
    );
}

/// After a CR LF header line, an over-long line in a 429 or 403 block never drops the block.
/// Round 10 dropped it when the long line's colon came after the 8,192 bytes kept, and lost the
/// two-day `Retry-After` already read: with `--paginate` no cooldown started. With the colon late
/// or missing, with and without `--paginate`, the cooldown has no end time.
#[test]
fn a_long_line_after_a_retry_after_never_drops_the_wait() {
    let filler = "X".repeat(9000);
    let late = format!(
        "HTTP/2.0 429 Too Many Requests\r\nRetry-After: 172800\r\n{filler}: value\r\n\r\n{{}}"
    );
    assert_overlong_header_starts_a_no_end_cooldown("late-colon", "FAKE_GH_STDOUT", &late);
    let missing = format!("HTTP/2.0 403 Forbidden\r\nRetry-After: 172800\r\n{filler}\r\n\r\n{{}}");
    assert_overlong_header_starts_a_no_end_cooldown("no-colon", "FAKE_GH_STDOUT", &missing);
}

/// Run `gh api --include`, with and without `--paginate`, whose fake gh writes a 429 response
/// in three writes a second apart, `...Retry-After: 17`, then `28`, then `00` and the rest, while
/// delivery to the consumer fails: through a pipe whose reader is already gone (`| head -c0`), or
/// to a terminal whose master side closes after the first write has reached it. gh-paced stops
/// reading after the chunk it holds, with the `Retry-After: 172800` line unfinished. The wait is
/// not known, so the cooldown must have no end time, never 900 s or the value read up to the cut,
/// and the next paced call is refused without running gh.
fn assert_cut_delivery_starts_a_no_end_cooldown(name: &str, terminal: bool) {
    let steps = [
        "HTTP/2.0 429 Too Many Requests\nA-Pad: x\r\nRetry-After: 17",
        "28",
        "00\r\n\r\n{}\n",
    ]
    .join("\u{1f}");
    for (mode, args) in [
        ("single", &["api", "-i", "repos/o/r"][..]),
        ("paginate", &["api", "-i", "--paginate", "repos/o/r"][..]),
    ] {
        let sb = Sandbox::new(&format!("{name}-{mode}"), FAST);
        let mut cmd = sb.cmd(args);
        cmd.env("FAKE_GH_STDOUT_STEPS", &steps)
            .env("FAKE_GH_EXIT", "1")
            .stderr(Stdio::piped());
        let master = if terminal {
            // SAFETY: standard pty allocation; both sides are owned by Files and closed on drop.
            let (master, slave) = unsafe {
                let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
                assert!(fd >= 0, "posix_openpt failed");
                assert_eq!(libc::grantpt(fd), 0);
                assert_eq!(libc::unlockpt(fd), 0);
                let mut name = [0 as libc::c_char; 128];
                assert_eq!(libc::ptsname_r(fd, name.as_mut_ptr(), name.len()), 0);
                let slave = libc::open(
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
                );
                assert!(slave >= 0, "opening the pty slave failed");
                (
                    <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd),
                    <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(slave),
                )
            };
            cmd.stdout(Stdio::from(slave));
            Some(master)
        } else {
            let mut fds = [0 as libc::c_int; 2];
            // SAFETY: pipe2 fills two new descriptors, each then owned by a File.
            let (reader, writer) = unsafe {
                assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0);
                (
                    <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fds[0]),
                    <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fds[1]),
                )
            };
            drop(reader);
            cmd.stdout(Stdio::from(writer));
            None
        };
        let child = cmd.spawn().unwrap();
        drop(cmd);
        if let Some(mut master) = master {
            // The first write reaches the terminal; then the terminal goes away.
            let mut seen = Vec::new();
            let begun = Instant::now();
            while !String::from_utf8_lossy(&seen).contains("Retry-After: 17") {
                assert!(
                    begun.elapsed().as_secs_f64() < 15.0,
                    "{mode}: the first write never reached the terminal: {seen:?}"
                );
                let mut pfd = libc::pollfd {
                    fd: std::os::fd::AsRawFd::as_raw_fd(&master),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll on one descriptor owned by `master`.
                if unsafe { libc::poll(&mut pfd, 1, 100) } > 0 {
                    let mut buf = [0u8; 4096];
                    let n = std::io::Read::read(&mut master, &mut buf).unwrap();
                    seen.extend_from_slice(&buf[..n]);
                }
            }
            drop(master);
        }
        let o = child.wait_with_output().unwrap();
        let err = stderr(&o);
        assert!(
            err.contains("refused from now on: the cooldown has no end time"),
            "{mode}: {err}"
        );
        assert!(
            err.contains("before its Retry-After was read whole"),
            "{mode}: {err}"
        );
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
                .unwrap();
        assert_eq!(
            state["cooldown"]["until"].as_f64(),
            Some(f64::MAX),
            "{mode}: {state}"
        );
        let o = sb.run(&["api", "repos/o/r"], &[]);
        assert_eq!(o.status.code(), Some(75), "{mode}: {}", stderr(&o));
        assert_eq!(sb.starts().len(), 1, "{mode}: gh ran: {:?}", sb.starts());
    }
}

/// Delivery to a consumer that has gone away (a pipe with no reader) fails on the first write,
/// with gh's `Retry-After: 172800` line cut at `Retry-After: 17`: the cooldown has no end time.
/// (At `65829cb0` the cut value or the bare 429 gave the 900 s floor.)
#[test]
fn a_consumer_gone_mid_retry_after_never_shortens_the_wait() {
    assert_cut_delivery_starts_a_no_end_cooldown("cut-pipe", false);
}

/// Delivery to a terminal that goes away after gh's first write reached it fails on the second,
/// with gh's `Retry-After: 172800` line cut at `Retry-After: 1728`: the cooldown has no end time.
/// (At `65829cb0` the cut value or the bare 429 gave a cooldown of 1,728 s or 900 s.)
#[test]
fn a_terminal_gone_mid_retry_after_never_shortens_the_wait() {
    assert_cut_delivery_starts_a_no_end_cooldown("cut-tty", true);
}

/// The user guide's command in "Ending a cooldown by hand", run as printed with only the example
/// state path replaced, ends a cooldown with no end time: `status` then shows no cooldown and the
/// next paced call runs, while the rest of the state file and the audit log are left as they
/// were. A command that cleared only one of the two copies would leave every call refused. It is
/// run under `umask 0777` with a read-only `test.json.clearing` (the temporary file of an earlier
/// version of the command) in the way: the state file still ends up mode 0600, and the command
/// leaves no file of its own behind.
#[test]
fn the_guides_command_ends_a_cooldown_with_no_end_time() {
    let section = gh_paced::USER_GUIDE
        .split_once("\n### Ending a cooldown by hand\n")
        .expect("the guide has the section")
        .1;
    let block = section
        .split_once("\n```sh\n")
        .expect("the section has a shell block")
        .1
        .split_once("\n```\n")
        .expect("the block ends")
        .0;
    let (first, rest) = block.split_once('\n').expect("more than one line");
    assert!(first.starts_with("state="), "{first}");

    let sb = Sandbox::new("clear-cooldown", FAST);
    let read = |name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(sb.path(name)).unwrap()).unwrap()
    };
    let out = "HTTP/2.0 403 Forbidden\r\nRetry-After: 40000000\r\n\r\n{}";
    let o = sb.run(
        &["api", "-i", "repos/o/r"],
        &[("FAKE_GH_STDOUT", out), ("FAKE_GH_EXIT", "1")],
    );
    assert!(
        stderr(&o).contains("the cooldown has no end time"),
        "{}",
        stderr(&o)
    );
    let o = sb.run(&["api", "repos/o/r"], &[]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    assert_eq!(
        read("state/test.cooldown")["until"].as_f64(),
        Some(f64::MAX)
    );
    let before = read("state/test.json");
    assert_eq!(before["cooldown"]["until"].as_f64(), Some(f64::MAX));
    let audit = std::fs::read(sb.path("state/test.audit.jsonl")).unwrap();
    let stale = sb.path("state/test.json.clearing");
    std::fs::write(&stale, "stale").unwrap();
    std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o400)).unwrap();
    let names = || {
        let mut v: Vec<String> = std::fs::read_dir(sb.path("state"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let mut expected = names();
    expected.retain(|n| n != "test.cooldown");

    let script = format!(
        "umask 0777\nstate='{}'\n{rest}\n",
        sb.path("state/test.json").display()
    );
    let o = Command::new("/bin/sh")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(["-c", &script])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));

    assert!(!sb.path("state/test.cooldown").exists());
    assert_eq!(names(), expected);
    assert_eq!(std::fs::read_to_string(&stale).unwrap(), "stale");
    let after = read("state/test.json");
    assert!(after["cooldown"].is_null(), "{after}");
    let without_cooldown = |mut v: serde_json::Value| {
        v.as_object_mut().unwrap().remove("cooldown");
        v
    };
    assert_eq!(without_cooldown(after), without_cooldown(before));
    let mode = std::fs::metadata(sb.path("state/test.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        std::fs::read(sb.path("state/test.audit.jsonl")).unwrap(),
        audit
    );

    let o = Command::new(BIN)
        .env_clear()
        .env("HOME", sb.path("home"))
        .env("GH_PACED_STATE_DIR", sb.path("state"))
        .env("GH_PACED_CONFIG", sb.path("config.json"))
        .args(["status", "--account", "test"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("  cooldown: none\n"), "{}", stdout(&o));
    let o = sb.run(&["api", "repos/o/r"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(sb.starts().len(), 2, "{:?}", sb.log());
}

/// `gh api --include --paginate --jq` output that ends inside a later page's body: a
/// status-shaped line, then a long `X-Long: ...` line whose end never arrives, with no CR LF header
/// line before it. That may be body text cut short, so no cooldown starts and the next call runs.
/// (Round 10 committed such a block at the end of the output and started a cooldown with no end
/// time.)
#[test]
fn a_page_body_cut_inside_a_long_line_starts_no_cooldown() {
    let sb = Sandbox::new("jq-cut-body", FAST);
    let response = format!(
        "HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 4000\r\n\r\nHTTP/2.0 429 Too Many Requests\nX-Long: {}",
        "x".repeat(9000)
    );
    // FAKE_GH_STDOUT_HEAD is written with no line end after it.
    let o = sb.run(
        &["api", "-i", "--paginate", "--jq", ".body", "repos/o/r"],
        &[("FAKE_GH_STDOUT_HEAD", &response)],
    );
    let err = stderr(&o);
    assert!(
        !sb.path("state/test.cooldown").exists(),
        "a cooldown record was written: {err}"
    );
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert!(stdout(&o) == response, "stdout passes through");
    assert!(!err.contains("PUSHBACK"), "{err}");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert!(state["cooldown"].is_null(), "{state}");
    let o = sb.run(&["api", "repos/o/r"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(sb.starts().len(), 2, "{:?}", sb.starts());
}

/// The rate-limit refresh's stderr cut off by the 1 MiB capture limit just after the words
/// `retry-after:` in the middle of a line of prose. They are not a header, so no cooldown starts
/// and the requested call runs; the cut is still reported. (Round 10 matched the words anywhere
/// and started a cooldown with no end time.)
#[test]
fn a_capture_cut_after_prose_starts_no_cooldown() {
    let sb = Sandbox::new("refresh-cut-prose", FAST);
    let tail = "\nDocumentation example retry-after: ";
    let mut text = "y".repeat((1 << 20) - tail.len());
    text.push_str(tail);
    assert_eq!(text.len(), 1 << 20, "the cut falls just after the words");
    text.push_str("not a response header\n");
    let file = sb.path("rate-stderr");
    std::fs::write(&file, &text).unwrap();
    // A paced read is due a refresh first (there is no snapshot yet).
    let o = sb.run(
        &["pr", "view", "1"],
        &[("FAKE_GH_RATE_STDERR_FILE", file.to_str().unwrap())],
    );
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert_eq!(sb.starts().len(), 1, "gh did not run: {err}");
    assert!(
        !sb.path("state/test.cooldown").exists(),
        "a cooldown record was written: {err}"
    );
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert!(state["cooldown"].is_null(), "{state}");
    assert!(
        err.contains(
            "rate-limit refresh: gh's stderr ran past the 1048576 bytes gh-paced reads and was \
             cut off there"
        ),
        "{err}"
    );
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    let refresh = audit
        .lines()
        .find(|l| l.contains("\"refresh\""))
        .unwrap_or_else(|| panic!("no refresh record: {audit}"));
    assert!(!refresh.contains("Retry-After cut off"), "{refresh}");
    assert!(
        refresh.contains("stderr ran past the 1048576 bytes"),
        "{refresh}"
    );
}

/// A consumer that reads gh's output late loses none of it. The fake gh writes its output and exits
/// at once while the consumer has not read a byte; it starts reading 4 s later, long after gh has
/// gone. 120 KiB is more than one 64 KiB pipe holds, so part of it is still inside gh-paced when gh
/// exits; 2 MiB is absorbed by the forwarding queue; 10 MiB is past the queue bound, so gh itself
/// is held back until the consumer reads. Each case ends with a header block, which must still be
/// seen behind the unread output and must set the cooldown. A header block after other output is
/// a later page's, so these calls use `--paginate`; without it only a block on gh's first stdout
/// line is read (see `include_body_with_crlf_headers_starts_no_cooldown`).
#[test]
fn slow_consumer_receives_every_byte() {
    let header = "HTTP/2.0 429 Too Many Requests\r\nRetry-After: 1200\r\n\r\n";
    let cases: Vec<(Sandbox, usize, std::process::Child)> = [120 << 10, 2 << 20, 10 << 20]
        .into_iter()
        .map(|n: usize| {
            let sb = Sandbox::new(&format!("slow-{n}"), FAST);
            let child = sb
                .cmd(&["api", "--include", "--paginate", "repos/o/r"])
                .env("FAKE_GH_STDOUT_BYTES", n.to_string())
                .env("FAKE_GH_STDOUT", header)
                .env("FAKE_GH_EXIT", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            (sb, n, child)
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_secs(4));
    for (sb, n, child) in cases {
        let o = child.wait_with_output().unwrap();
        assert_eq!(o.status.code(), Some(1), "{n} bytes: {}", stderr(&o));
        let expected = format!("{}\n{header}\n", "x".repeat(n));
        assert_eq!(o.stdout.len(), expected.len(), "{n} bytes: {}", stderr(&o));
        assert!(
            o.stdout == expected.as_bytes(),
            "{n} bytes: content differs"
        );
        assert!(
            stderr(&o).contains("GH-PACED PUSHBACK [test]"),
            "{n} bytes: {}",
            stderr(&o)
        );
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
                .unwrap();
        assert!(
            state["cooldown"]["until"].as_f64().is_some(),
            "{n} bytes: no cooldown: {state}"
        );
    }
}

/// Seconds since the epoch.
fn epoch_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// Wait for the cooldown record while `child`'s consumer reads nothing, then drain it. Returns
/// the record's `until` (if it appeared within 15 s), whether gh-paced was still running when it
/// appeared, and the drained output.
fn cooldown_before_delivery(
    sb: &Sandbox,
    mut child: std::process::Child,
) -> (Option<f64>, bool, Output) {
    let begun = Instant::now();
    let mut until = None;
    while begun.elapsed().as_secs_f64() < 15.0 {
        if let Ok(text) = std::fs::read_to_string(sb.path("state/test.cooldown")) {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            until = v["until"].as_f64();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let running = child.try_wait().unwrap().is_none();
    (until, running, child.wait_with_output().unwrap())
}

/// GitHub pushback is recorded as soon as gh-paced reads it, while gh's output is still waiting
/// for a consumer that has read nothing (1 MiB, far more than a pipe holds, so gh-paced cannot
/// finish delivering it). Other gh-paced processes therefore pause at once, not only after the
/// consumer catches up. Covers a stderr message and a single response's `--include` headers.
#[test]
fn pushback_is_recorded_before_output_is_delivered() {
    let n: usize = 1 << 20;
    let sb = Sandbox::new("early-stderr", FAST);
    let before = epoch_now();
    let child = sb
        .cmd(&["pr", "view", "5"])
        .env(
            "FAKE_GH_STDERR",
            "HTTP 403: You have exceeded a secondary rate limit",
        )
        .env("FAKE_GH_STDERR_BYTES", n.to_string())
        .env("FAKE_GH_EXIT", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (until, running, o) = cooldown_before_delivery(&sb, child);
    let until = until.expect("no cooldown record while stderr was undelivered");
    assert!(running, "gh-paced had already finished delivering");
    assert!(
        until - before >= 899.0 && until - before < 1000.0,
        "cooldown {} s",
        until - before
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains(&format!("\n{}\n", "y".repeat(n))),
        "every byte delivered"
    );
    assert!(
        stderr(&o).contains("GH-PACED PUSHBACK [test]"),
        "the banner still prints"
    );

    let sb = Sandbox::new("early-include", FAST);
    let header = "HTTP/2.0 429 Too Many Requests\nRetry-After: 1200\r\n\r\n";
    let before = epoch_now();
    let child = sb
        .cmd(&["api", "--include", "repos/o/r"])
        .env("FAKE_GH_STDOUT_HEAD", header)
        .env("FAKE_GH_STDOUT_BYTES", n.to_string())
        .env("FAKE_GH_EXIT", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (until, running, o) = cooldown_before_delivery(&sb, child);
    let until = until.expect("no cooldown record while stdout was undelivered");
    assert!(running, "gh-paced had already finished delivering");
    assert!(
        until - before >= 1199.0 && until - before < 1300.0,
        "cooldown {} s",
        until - before
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        o.stdout == format!("{header}{}\n", "x".repeat(n)).as_bytes(),
        "stdout differs"
    );
    assert!(
        stderr(&o).contains("GH-PACED PUSHBACK [test]"),
        "{}",
        stderr(&o)
    );
}

/// Without `--paginate`, gh prints one response, so only the header block on its first stdout
/// line is read. A body that holds a CR LF status line and headers after a healthy block (a
/// string with embedded CR LF, printed by --jq or --template) starts no cooldown.
#[test]
fn include_body_with_crlf_headers_starts_no_cooldown() {
    let sb = Sandbox::new("include-crlf-body", FAST);
    let response = "HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 4000\r\n\r\nHTTP/2.0 403 Forbidden\nRetry-After: 99999\r\nX-Ratelimit-Remaining: 0\r\n\r\n";
    let o = sb.run(&["api", "-i", "repos/o/r"], &[("FAKE_GH_STDOUT", response)]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{response}\n"), "stdout passes through");
    assert!(!stderr(&o).contains("PUSHBACK"), "{}", stderr(&o));
    assert!(
        !sb.path("state/test.cooldown").exists(),
        "a cooldown record was written"
    );
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert!(state["cooldown"].is_null(), "{state}");
}

/// A signal that arrives after gh has exited, while gh's output is stuck behind a consumer that
/// has stopped reading, ends gh-paced by that signal within a few seconds. gh-paced's own
/// warning is given up to 2 s to reach stderr and is then dropped; it does not wait for the
/// consumer to read again.
#[test]
fn late_signal_ends_gh_paced_while_the_consumer_is_stalled() {
    let sb = Sandbox::new("late-stall", FAST);
    let mut child = sb
        .cmd(&["pr", "view", "5"])
        .env("FAKE_GH_STDERR_BYTES", (1usize << 20).to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_log(&sb, "end pr view 5");
    // Let gh exit and be reaped, so the signal is not forwarded to it.
    std::thread::sleep(std::time::Duration::from_secs(1));
    // SAFETY: signalling our own child, which has not been reaped.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let sent = Instant::now();
    let mut ended = None;
    while sent.elapsed().as_secs_f64() < 8.0 {
        if let Some(status) = child.try_wait().unwrap() {
            ended = Some((status, sent.elapsed().as_secs_f64()));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Drain stderr so a stuck gh-paced cannot outlive the test.
    let o = child.wait_with_output().unwrap();
    let (status, secs) = ended.expect("gh-paced was still waiting for its consumer 8 s after TERM");
    assert!(secs < 5.0, "gh-paced took {secs} s to die");
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{}", stderr(&o));
}

/// Both records of the cooldown, `test.json` and `test.cooldown`, end two days after `before`,
/// and gh-paced's own pushback banner gives the two days as the cooldown it recorded. The banner
/// comes from what gh-paced scanned before it died, while a reader's hook can still write the
/// state file just before the process ends, so only the banner shows that gh-paced itself saw
/// the wait. A copy of the scanner without it would print a shorter pause, or the two days with
/// "an existing longer cooldown stays" (the reader having recorded them first).
fn assert_two_day_cooldown(sb: &Sandbox, before: f64, err: &str) {
    let line = err
        .lines()
        .find(|l| l.contains("is paused for "))
        .unwrap_or_else(|| panic!("no pause in the banner:\n{err}"));
    let secs: u64 = line
        .split("is paused for ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unreadable pause: {line}\n{err}"));
    assert!(
        (172_790..172_900).contains(&secs),
        "the banner gives {secs} s:\n{err}"
    );
    assert!(
        !err.contains("an existing longer cooldown stays"),
        "gh-paced recorded a shorter cooldown than the reader's:\n{err}"
    );
    for (file, key) in [
        ("state/test.json", Some("cooldown")),
        ("state/test.cooldown", None),
    ] {
        let text =
            std::fs::read_to_string(sb.path(file)).unwrap_or_else(|e| panic!("{file}: {e}\n{err}"));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let cd = key.map_or(&v, |k| &v[k]);
        let until = cd["until"]
            .as_f64()
            .unwrap_or_else(|| panic!("{file}: {v}\n{err}"));
        assert!(
            until - before >= 172_790.0 && until - before < 172_900.0,
            "{file}: cooldown {} s\n{err}",
            until - before
        );
    }
}

/// A late signal keeps the wait from a header block gh-paced has already read. gh printed a 429
/// block with a whole two-day `Retry-After` line and no blank line, then exited, leaving a
/// helper that holds gh's stdout and keeps writing header lines to it, so gh-paced is still
/// reading stdout, with the block still open, when TERM arrives 1 s later. gh-paced dies by the
/// signal and the cooldown is the two days. Before round 12 it took the scanner without feeding
/// it the end of stdout, so the open block was never committed and no cooldown started.
#[test]
fn a_late_signal_keeps_the_wait_from_a_header_block_already_read() {
    let sb = Sandbox::new("late-open-block", FAST);
    let child = sb
        .cmd(&["api", "-i", "repos/o/r"])
        .env(
            "FAKE_GH_STDOUT",
            "HTTP/2.0 429 Too Many Requests\r\nRetry-After: 172800\r",
        )
        .env("FAKE_GH_EXIT", "1")
        .env("FAKE_GH_HOLD_OUT", "10")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_log(&sb, "end api -i repos/o/r");
    // Let gh exit and be reaped, so the signal is not forwarded to it. gh-paced goes on reading
    // stdout for up to 5 s after the exit while the helper writes.
    std::thread::sleep(std::time::Duration::from_secs(1));
    let before = epoch_now();
    // SAFETY: signalling our own child, which has not been reaped.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let o = child.wait_with_output().unwrap();
    let err = stderr(&o);
    assert_eq!(o.status.signal(), Some(libc::SIGTERM), "{err}");
    assert!(
        err.contains("signal 15 arrived while its output was still being delivered"),
        "the signal did not arrive while gh-paced was still reading: {err}"
    );
    assert_two_day_cooldown(&sb, before, &err);
}

/// After a late signal gh-paced still scans output gh wrote that it had not read. The test holds
/// the account lock, so the stdout reader, recording the cooldown of gh's first page (a 429 with
/// `Retry-After: 60`, so 900 s), waits for the lock while gh writes a second page (a 429 with a
/// two-day `Retry-After`) and exits. TERM arrives, the test lets the lock go 0.1 s later, and the
/// cooldown is the two days. Before round 12 the reader stopped without reading the second page,
/// and the cooldown was 900 s.
#[test]
fn a_late_signal_scans_output_gh_wrote_but_gh_paced_had_not_read() {
    assert_late_signal_scans_the_unread_page("late-unread", 100);
}

/// As above, but the test keeps the lock for 2.5 s after TERM, longer than the 1 s gh-paced
/// waits for the readers beyond the lock's own wait. gh-paced waits for the reader to get the
/// lock, record its cooldown and scan the second page, and the cooldown is the two days. With a
/// fixed 1 s wait gh-paced took the scanner without the second page.
#[test]
fn a_late_signal_waits_for_a_reader_held_up_by_the_state_lock() {
    assert_late_signal_scans_the_unread_page("late-unread-held", 2500);
}

/// The two tests above: gh writes two 429 pages while the stdout reader waits for the account
/// lock, which the test holds until `hold_ms` after it sends TERM. gh writes nothing until the
/// test holds the lock, so the reader's hook for the first page always waits.
fn assert_late_signal_scans_the_unread_page(name: &str, hold_ms: u64) {
    let sb = Sandbox::new(name, FAST);
    let steps = [
        "HTTP/2.0 429 Too Many Requests\r\nRetry-After: 60\r\n\r\n{}\n",
        "HTTP/2.0 429 Too Many Requests\r\nRetry-After: 172800\r\n\r\n{}\n",
    ]
    .join("\u{1f}");
    let child = sb
        .cmd(&["api", "-i", "--paginate", "repos/o/r"])
        .env("FAKE_GH_GO", sb.path("go"))
        .env("FAKE_GH_STDOUT_STEPS", &steps)
        .env("FAKE_GH_EXIT", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_log(&sb, "start api");
    let held = hold_account_lock(&sb);
    std::fs::write(sb.path("go"), "").unwrap();
    wait_for_log(&sb, "end api");
    // Let gh exit and be reaped, so the signal is not forwarded to it.
    std::thread::sleep(std::time::Duration::from_secs(1));
    let before = epoch_now();
    // SAFETY: signalling our own child, which has not been reaped.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(hold_ms));
    drop(held);
    let o = child.wait_with_output().unwrap();
    let err = stderr(&o);
    assert_eq!(o.status.signal(), Some(libc::SIGTERM), "{err}");
    assert!(
        err.contains("signal 15 arrived while its output was still being delivered"),
        "the signal did not arrive while gh-paced was still reading: {err}"
    );
    assert_two_day_cooldown(&sb, before, &err);
}

/// Take the account lock, as another gh-paced process recording a cooldown would hold it.
fn hold_account_lock(sb: &Sandbox) -> std::fs::File {
    use std::os::fd::AsRawFd;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(sb.path("state/test.lock"))
        .unwrap();
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
    held
}

/// gh's stdout, a terminal, for the two tests below: a healthy first page with a 100 KiB body,
/// then three 429 pages (`Retry-After` 60, 1200 and 172800 s) with 5 KiB of body between them,
/// more than one read from a terminal returns (4,095 bytes), so each is read separately.
fn three_429_pages() -> String {
    let line = format!("{}\n", "x".repeat(1023));
    let page = |wait: u32| format!("HTTP/2.0 429 Too Many Requests\r\nRetry-After: {wait}\r\n\r\n");
    format!(
        "HTTP/2.0 200 OK\r\nX-Ratelimit-Remaining: 4000\r\n\r\n{}{}{}{}{}{}{{}}\n",
        line.repeat(100),
        page(60),
        line.repeat(5),
        page(1200),
        line.repeat(5),
        page(172_800),
    )
}

/// The account lock is held and `lock_wait_secs` is 3, so each time the stdout reader records a
/// cooldown its hook waits 3 s and gives up. gh writes its pages and exits during the hook for the
/// first 429; the reader then sees the exit, reads the second 429 and waits 3 s again, by which
/// time the stream has been silent for more than 2 s as far as the reader knows and it stops
/// reading, with the third page, gh's own output, still unread. The test lets the lock go 6.5 s
/// after gh's exit. With `late`, the terminal is never read, so gh-paced is still delivering when
/// TERM arrives 7.5 s after gh's exit. Either way the cooldown must be the two days of the third
/// page; before round 13 it was the 1,200 s of the second, because a reader stopped at a cutoff
/// handed the rest of stdout to a drainer that does not scan it, or, after a late signal, dropped
/// it.
fn assert_stopped_reader_scans_what_gh_wrote(name: &str, late: bool) {
    let config = FAST.replacen('{', r#"{"lock_wait_secs": 3, "#, 1);
    let sb = Sandbox::new(name, &config);
    let (master, slave) = pty_pair();
    let child = sb
        .cmd(&["api", "-i", "--paginate", "repos/o/r"])
        .env("FAKE_GH_GO", sb.path("go"))
        .env("FAKE_GH_STDOUT_HEAD", three_429_pages())
        .env("FAKE_GH_EXIT", "1")
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_log(&sb, "start api");
    let held = hold_account_lock(&sb);
    std::fs::write(sb.path("go"), "").unwrap();
    // A consumer that keeps reading, or (late) one that has stopped but is still there.
    let (stalled, reader) = if late {
        (Some(master), None)
    } else {
        let mut master = master;
        let reader = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = std::io::Read::read(&mut master, &mut buf) {
                seen.extend_from_slice(&buf[..n]);
            }
            String::from_utf8_lossy(&seen).into_owned()
        });
        (None, Some(reader))
    };
    wait_for_log(&sb, "end api");
    let exited = Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(6500));
    drop(held);
    let before = epoch_now();
    if late {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        // SAFETY: signalling our own child, which has not been reaped.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
    }
    let o = child.wait_with_output().unwrap();
    let err = stderr(&o);
    if late {
        assert_eq!(o.status.signal(), Some(libc::SIGTERM), "{err}");
        assert!(
            err.contains("signal 15 arrived while its output was still being delivered"),
            "the signal did not arrive while gh-paced was still delivering: {err}"
        );
    } else {
        assert_eq!(o.status.code(), Some(1), "{err}");
    }
    assert!(
        exited.elapsed().as_secs_f64() > 6.0,
        "gh-paced finished before the lock was let go: {err}"
    );
    assert_two_day_cooldown(&sb, before, &err);
    if let Some(reader) = reader {
        // What the reader took before it stopped was delivered too, in order.
        let seen = reader.join().unwrap();
        let waits: Vec<&str> = seen
            .split("Retry-After: ")
            .skip(1)
            .map(|rest| rest.split(['\r', '\n']).next().unwrap_or(""))
            .collect();
        assert_eq!(waits, ["60", "1200", "172800"], "{err}");
        assert_eq!(seen.matches(&"x".repeat(1023)).count(), 110, "{err}");
    }
    drop(stalled);
}

/// A reader that stopped at a cutoff after gh exited, with gh's last page unread, still scans
/// that page before handing the stream on (see `assert_stopped_reader_scans_what_gh_wrote`).
#[test]
fn a_reader_stopped_after_gh_exits_still_scans_what_gh_wrote() {
    assert_stopped_reader_scans_what_gh_wrote("stopped-reader", false);
}

/// As above, with a late signal after the reader stopped: gh-paced takes the scanner with the
/// last page in it.
#[test]
fn a_late_signal_after_a_reader_stopped_keeps_what_gh_wrote() {
    assert_stopped_reader_scans_what_gh_wrote("stopped-reader-late", true);
}

/// A terminal's INT (Ctrl-C) goes to the whole foreground process group, gh included, so
/// gh-paced does not forward it. Once gh has exited, it is for gh-paced alone: it ends gh-paced
/// by SIGINT within a few seconds, even while gh's output is stuck behind a consumer that has
/// stopped reading, just as it would have ended gh writing to that consumer.
#[test]
fn terminal_interrupt_after_gh_exits_ends_gh_paced() {
    let sb = Sandbox::new("late-tty", FAST);
    // A pseudo-terminal that becomes gh-paced's controlling terminal, in a session of its own.
    // SAFETY: standard pty allocation; the master is owned by a File and closed on drop.
    let (mut master, slave_name) = unsafe {
        let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        assert!(fd >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(fd), 0);
        assert_eq!(libc::unlockpt(fd), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(fd, name.as_mut_ptr(), name.len()), 0);
        let name = std::ffi::CStr::from_ptr(name.as_ptr()).to_owned();
        (
            <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd),
            name,
        )
    };
    let mut cmd = sb.cmd(&["pr", "view", "5"]);
    cmd.env("FAKE_GH_STDERR_BYTES", (1usize << 20).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: setsid and open are async-signal-safe; the name was copied before the fork.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // A session leader without a terminal acquires the first terminal it opens
            // without O_NOCTTY; its process group becomes the terminal's foreground group.
            if libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    wait_for_log(&sb, "end pr view 5");
    // Let gh exit and be reaped, so gh-paced is the only process left in the group.
    std::thread::sleep(std::time::Duration::from_secs(1));
    master.write_all(b"\x03").unwrap();
    let sent = Instant::now();
    let mut ended = None;
    while sent.elapsed().as_secs_f64() < 8.0 {
        if let Some(status) = child.try_wait().unwrap() {
            ended = Some((status, sent.elapsed().as_secs_f64()));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Drain stderr so a stuck gh-paced cannot outlive the test.
    let o = child.wait_with_output().unwrap();
    let (status, secs) =
        ended.expect("gh-paced was still waiting for its consumer 8 s after the terminal's INT");
    assert!(secs < 5.0, "gh-paced took {secs} s to die");
    assert_eq!(status.signal(), Some(libc::SIGINT), "{}", stderr(&o));
}

/// Snapshot directories in the state directory, by name.
fn snapshot_dirs(sb: &Sandbox) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(sb.path("state"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("snap-"))
        .collect();
    v.sort();
    v
}

/// Make a directory look `secs` seconds old.
fn age_dir(path: &std::path::Path, secs: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
    std::fs::File::open(path).unwrap().set_modified(t).unwrap();
}

/// A process ID that has exited and been reaped.
fn dead_pid() -> u32 {
    let mut c = Command::new("/bin/true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

/// Every paced call, not only one that copies a file, removes abandoned snapshot directories: a
/// dead creator, a free lock and past the grace period. A directory whose lock is still held is
/// kept however old, and a local command touches nothing.
#[test]
fn every_paced_call_sweeps_abandoned_snapshots() {
    let sb = Sandbox::new("sweep", FAST);
    let pid = dead_pid();
    let abandoned = sb.path(&format!("state/snap-{pid}-1-00000000000000d1"));
    let held = sb.path(&format!("state/snap-{pid}-1-00000000000000d2"));
    std::fs::create_dir_all(abandoned.join("0")).unwrap();
    std::fs::write(abandoned.join("0/body.md"), "old copy").unwrap();
    std::fs::create_dir_all(&held).unwrap();
    let lock = std::fs::File::create(held.join(".lock")).unwrap();
    // SAFETY: flock on a descriptor owned by `lock`, released when it is dropped.
    assert_eq!(
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) },
        0
    );
    age_dir(&abandoned, 3600);
    age_dir(&held, 3600);
    let o = sb.run(&["--version"], &[("FAKE_GH_STDOUT", "gh version 9.9.9")]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        snapshot_dirs(&sb).len(),
        2,
        "a local command sweeps nothing"
    );
    let o = sb.run(&["pr", "view", "5"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        snapshot_dirs(&sb),
        vec![format!("snap-{pid}-1-00000000000000d2")],
        "the abandoned directory is removed and the held one kept"
    );
    drop(lock);
    let o = sb.run(&["pr", "view", "6"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(snapshot_dirs(&sb).is_empty(), "{:?}", snapshot_dirs(&sb));
}

/// gh inherits the snapshot lock, so a background helper gh leaves running keeps the copies of
/// the files it may still read after the wrapper has exited, even from a sweep that finds the
/// directory old and its creator dead. Once the helper exits the next paced call removes it.
#[test]
fn background_helper_keeps_the_snapshot() {
    let sb = Sandbox::new("bg-snap", BURSTY);
    let body = sb.path("body.md");
    std::fs::write(&body, "a short note").unwrap();
    let body_arg = format!("--body-file={}", body.display());
    let first = sb.run(
        &["issue", "comment", "1", &body_arg],
        &[("FAKE_GH_BG", "3")],
    );
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let dirs = snapshot_dirs(&sb);
    assert_eq!(
        dirs.len(),
        1,
        "the copies outlive the wrapper while the helper runs"
    );
    let snap = sb.path(&format!("state/{}", dirs[0]));
    let started = event_time(&sb, "start", "issue comment 1 --body-file=");
    let gh_args = sb.starts()[0].1.clone();
    assert!(
        gh_args.contains(&snap.display().to_string()),
        "gh was given the copy: {gh_args}"
    );
    age_dir(&snap, 3600);
    let o = sb.run(&["pr", "view", "5"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        event_time(&sb, "start", "pr view 5") - started < 3.0,
        "the sweep ran while the helper was alive"
    );
    assert_eq!(snapshot_dirs(&sb), dirs, "the helper's lock keeps it");
    wait_for_log(&sb, "bg-end issue comment 1");
    age_dir(&snap, 3600);
    let o = sb.run(&["pr", "view", "6"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(snapshot_dirs(&sb).is_empty(), "{:?}", snapshot_dirs(&sb));
}

/// The same late signal, when gh itself died by a signal after printing a pushback phrase: the
/// PUSHBACK banner that follows is written with the same 2 s bound, so gh-paced still dies by
/// the caller's signal within a few seconds instead of waiting for the stalled consumer while
/// holding the account lock.
#[test]
fn late_signal_after_gh_died_by_a_signal_still_bounds_the_pushback_banner() {
    let sb = Sandbox::new("late-stall-signalled", FAST);
    let mut child = sb
        .cmd(&["pr", "view", "5"])
        .env(
            "FAKE_GH_STDERR",
            "HTTP 403: You have exceeded a secondary rate limit and have been temporarily blocked",
        )
        .env("FAKE_GH_STDERR_BYTES", (1usize << 20).to_string())
        .env("FAKE_GH_SIGNAL", "TERM")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_log(&sb, "end pr view 5");
    // Let gh kill itself and be reaped, so the caller's signal is not forwarded to it.
    std::thread::sleep(std::time::Duration::from_secs(1));
    // SAFETY: signalling our own child, which has not been reaped.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let sent = Instant::now();
    let mut ended = None;
    while sent.elapsed().as_secs_f64() < 8.0 {
        if let Some(status) = child.try_wait().unwrap() {
            ended = Some((status, sent.elapsed().as_secs_f64()));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Drain stderr so a stuck gh-paced cannot outlive the test.
    let o = child.wait_with_output().unwrap();
    let (status, secs) = ended.expect("gh-paced was still waiting for its consumer 8 s after TERM");
    assert!(secs < 5.0, "gh-paced took {secs} s to die");
    assert_eq!(status.signal(), Some(libc::SIGTERM), "{}", stderr(&o));
    // The pushback was still recorded, under the lock, before gh-paced died.
    assert!(sb.path("state/test.json").exists());
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.path("state/test.json")).unwrap())
            .unwrap();
    assert!(!state["cooldown"].is_null(), "{state}");
}

/// A pipe whose buffer is already full, so the next write to its write end blocks until the
/// test reads. Returns (read end, write end).
fn full_pipe() -> (std::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe2 fills two descriptors that are then owned below; the writes use a local
    // buffer, and the write end is made blocking again before anything else sees it.
    unsafe {
        assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0);
        let flags = libc::fcntl(fds[1], libc::F_GETFL);
        assert_eq!(
            libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK),
            0
        );
        let buf = [b'z'; 4096];
        while libc::write(fds[1], buf.as_ptr().cast(), buf.len()) > 0 {}
        while libc::write(fds[1], buf.as_ptr().cast(), 1) > 0 {}
        assert_eq!(libc::fcntl(fds[1], libc::F_SETFL, flags), 0);
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    }
}

/// Waits up to `secs` for `child` to exit; None if it is still running.
fn exit_within(child: &mut std::process::Child, secs: f64) -> Option<std::process::ExitStatus> {
    let begun = Instant::now();
    while begun.elapsed().as_secs_f64() < secs {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

/// An invocation whose stderr consumer has stopped reading never holds the account lock while
/// it waits to write: its throttle warning is written after the lock is released, so the
/// account's other gh calls go ahead, and the stalled call itself continues once its consumer
/// reads again.
#[test]
fn a_stalled_stderr_consumer_never_holds_the_account_lock() {
    let sb = Sandbox::new("stalled-lock", FAST);
    // Spend the one write token, so the next write throttles for about 1 s.
    let o = sb.run(&["pr", "comment", "1", "--body", "short note"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let (mut consumer, stderr_end) = full_pipe();
    let mut stalled = sb
        .cmd(&["pr", "comment", "2", "--body", "short note"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_end))
        .spawn()
        .unwrap();
    // Time to take the lock, decide to throttle and try to say so.
    std::thread::sleep(std::time::Duration::from_millis(700));
    assert!(
        stalled.try_wait().unwrap().is_none(),
        "the stalled call ended before the check"
    );
    let mut other = sb
        .cmd(&["pr", "view", "5"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let other_status = exit_within(&mut other, 8.0);
    // Read the stalled call's stderr to the end, so it can finish.
    let drain = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut consumer, &mut v);
        v
    });
    if other_status.is_none() {
        let _ = other.kill();
    }
    let other_out = other.wait_with_output().unwrap();
    let stalled_status = exit_within(&mut stalled, 10.0);
    if stalled_status.is_none() {
        let _ = stalled.kill();
    }
    let _ = stalled.wait();
    let stalled_err = String::from_utf8_lossy(&drain.join().unwrap()).into_owned();
    let other_status = other_status
        .expect("a READ waited 8 s for the lock while a WRITE was stuck writing to stderr");
    assert_eq!(other_status.code(), Some(0), "{}", stderr(&other_out));
    assert_eq!(
        stalled_status.and_then(|s| s.code()),
        Some(0),
        "{stalled_err}"
    );
    assert!(
        stalled_err.contains("GH-PACED WARNING [test] write budget"),
        "the throttle warning was delivered late, not lost: {stalled_err}"
    );
}

/// A lock that another process keeps (here the test itself) fails the invocation with exit 70
/// after `GH_PACED_LOCK_WAIT` seconds, naming the lock file, instead of hanging.
#[test]
fn a_lock_held_too_long_fails_instead_of_hanging() {
    use std::os::fd::AsRawFd;
    let sb = Sandbox::new("lock-wait", FAST);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(sb.path("state/test.lock"))
        .unwrap();
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
    let begun = Instant::now();
    let mut child = sb
        .cmd(&["pr", "view", "5"])
        .env("GH_PACED_LOCK_WAIT", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = exit_within(&mut child, 10.0);
    if status.is_none() {
        let _ = child.kill();
    }
    let secs = begun.elapsed().as_secs_f64();
    let o = child.wait_with_output().unwrap();
    drop(held);
    let status = status.expect("gh-paced was still waiting for the lock after 10 s");
    assert_eq!(status.code(), Some(70), "{}", stderr(&o));
    assert!(secs < 6.0, "gh-paced took {secs} s to give up");
    let err = stderr(&o);
    assert!(
        err.contains("test.lock is still held by another gh-paced process after 1 s"),
        "{err}"
    );
    assert!(
        sb.log().is_empty(),
        "gh ran without the lock: {:?}",
        sb.log()
    );
}

/// When gh-paced's consumer goes away (a closed pipe, as after `| head -n1` exits), gh sees the
/// broken pipe on its next write, as it would without gh-paced, instead of writing on into
/// gh-paced forever. Here a gh that writes without end, on the teed stdout of `api --include`
/// and on the always-teed stderr, stops, and gh-paced dies by SIGPIPE as gh did.
#[test]
fn a_consumer_that_goes_away_is_passed_on_to_gh() {
    for (args, fd) in [
        (&["api", "--include", "repos/o/r"][..], 1),
        (&["pr", "view", "5"][..], 2),
    ] {
        let sb = Sandbox::new(&format!("epipe-{fd}"), FAST);
        let mut cmd = sb.cmd(args);
        cmd.env("FAKE_GH_FOREVER", fd.to_string());
        if fd == 1 {
            cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        }
        let mut child = cmd.spawn().unwrap();
        let mut consumer: Box<dyn std::io::Read> = if fd == 1 {
            Box::new(child.stdout.take().unwrap())
        } else {
            Box::new(child.stderr.take().unwrap())
        };
        // Read a little, then go away.
        let mut buf = [0u8; 4096];
        consumer.read_exact(&mut buf).unwrap();
        drop(consumer);
        let status = exit_within(&mut child, 10.0);
        if status.is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        let status = status.unwrap_or_else(|| {
            panic!("fd {fd}: gh was still writing 10 s after the consumer went away")
        });
        assert_eq!(status.signal(), Some(libc::SIGPIPE), "fd {fd}: {status:?}");
    }
}

/// One gh call makes every request its cost stands for back to back, so a cost above the class
/// burst is refused before gh runs instead of being admitted and charged as debt. A cost equal to
/// the burst still runs.
#[test]
fn a_call_costing_more_than_the_burst_is_refused_before_gh_runs() {
    let sb = Sandbox::new("over-burst", FAST);
    // READ burst 10: `--limit 2000` is 20 pages.
    let o = sb.run(&["pr", "list", "--limit", "2000"], &[]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("more than the read burst of 10"), "{err}");
    assert!(err.contains("lower --limit"), "{err}");
    // SEARCH burst 2 (the default): `--limit 300` is 3 pages.
    let o = sb.run(&["search", "issues", "x", "--limit", "300"], &[]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("more than the search burst of 2"),
        "{}",
        stderr(&o)
    );
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    // Exactly the burst is admitted: `--limit 1000` is 10 pages.
    let o = sb.run(&["pr", "list", "--limit", "1000"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let starts = sb.starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].1, "pr list --limit 1000");
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    assert_eq!(audit.matches("\"refuse\"").count(), 2, "{audit}");
}

/// `gh status` is one of gh's own everyday commands, so the burst rule must not refuse it at
/// the default budgets. Its requests are REST reads and GraphQL queries (its searches are
/// GraphQL searches, which `gh api graphql` and `pr list --search` already charge to READ), so it
/// is charged to READ, at the whole READ burst.
#[test]
fn gh_status_runs_at_the_default_budgets() {
    let sb = Sandbox::new("gh-status", "{}");
    let o = sb.run(&["status"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let starts = sb.starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].1, "status");
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    let admit: serde_json::Value = audit
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["event"] == "admit" && v["command"] == "status")
        .unwrap_or_else(|| panic!("no admit record for status: {audit}"));
    assert_eq!(admit["class"], "read", "{admit}");
    assert_eq!(admit["cost"], 10, "{admit}");
}

/// A watch is refused (exit 64) before gh runs unless `GH_PACED_ALLOW_WATCH=1` is set, and the
/// refusal is audited.
#[test]
fn a_watch_without_the_opt_in_is_refused_before_gh_runs() {
    let sb = Sandbox::new("watch-optin", FAST);
    let o = sb.run(&["pr", "checks", "1", "--watch", "--interval", "30"], &[]);
    assert_eq!(o.status.code(), Some(64), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("GH-PACED REFUSED [test]"), "{err}");
    assert!(err.contains("GH_PACED_ALLOW_WATCH=1"), "{err}");
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    assert!(audit.contains("\"refuse\""), "{audit}");
    // Opted in, the same watch runs (the fake gh returns at once).
    let o = sb.run(
        &["pr", "checks", "1", "--watch", "--interval", "30"],
        &[("GH_PACED_ALLOW_WATCH", "1")],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.starts());
}

/// gh expands an ordinary alias inside its own process, so the stdin that an alias's
/// `--input -` sends, or a body file named inside the alias, would never reach a second
/// gh-paced. gh-paced expands the alias from gh's configuration file and runs gh with the
/// expansion, so those bodies are inspected (and body files snapshotted) like typed ones.
#[test]
fn an_alias_expansion_has_its_body_inspected() {
    let sb = Sandbox::new("alias-body", FAST);
    let big = sb.path("big.md");
    std::fs::write(&big, "word ".repeat(3000)).unwrap();
    let small = sb.path("small.md");
    std::fs::write(&small, "a short note\n").unwrap();
    write_gh_config(
        &sb,
        &format!(
            "version: 1\naliases:\n    upload: api -X POST repos/o/r/issues/$1/comments --input -\n    post: issue comment $1 --body-file {}\n    note: issue comment $1 --body-file {}\n",
            big.display(),
            small.display()
        ),
    );
    let got = sb.path("stdin.out");
    // 12 KB of JSON on stdin, over the 8 KiB limit.
    let oversized = format!("{{\"body\":\"{}\"}}", "word ".repeat(2400));
    let o = run_with_stdin(&sb, &got, &["upload", "7"], oversized.as_bytes());
    assert_eq!(o.status.code(), Some(65), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("GH-PACED REFUSED [test]"),
        "{}",
        stderr(&o)
    );
    // A 15 KB body file named inside the alias.
    let o = run_with_stdin(&sb, &got, &["post", "7"], b"");
    assert_eq!(o.status.code(), Some(65), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("content guard: write body is 15000 bytes"),
        "{}",
        stderr(&o)
    );
    // The refusal names the alias it expanded.
    assert!(stderr(&o).contains("gh alias `post`"), "{}", stderr(&o));
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    // A small body passes and reaches gh byte for byte; gh is given the expansion, not the
    // alias name, so it cannot expand the alias differently.
    let body = b"{\"body\":\"a short prose note\"}\n";
    let o = run_with_stdin(&sb, &got, &["upload", "7"], body);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(std::fs::read(&got).unwrap(), body);
    let starts = sb.starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(
        starts[0].1,
        "api -X POST repos/o/r/issues/7/comments --input -"
    );
    // A body file named inside an alias is snapshotted like a typed one: gh reads the copy.
    let o = run_with_stdin(&sb, &got, &["note", "7"], b"");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let starts = sb.starts();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(
        starts[1].1.starts_with("issue comment 7 --body-file "),
        "{starts:?}"
    );
    assert!(
        !starts[1].1.contains(&small.display().to_string()),
        "gh read the original file, not the snapshot: {starts:?}"
    );
}

fn write_gh_config(sb: &Sandbox, text: &str) {
    std::fs::create_dir_all(sb.path("home/.config/gh")).unwrap();
    std::fs::write(sb.path("home/.config/gh/config.yml"), text).unwrap();
}

/// Run `gh-paced -- args` with `input` on stdin; the fake gh copies its stdin to `got`.
fn run_with_stdin(sb: &Sandbox, got: &std::path::Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = sb
        .cmd(args)
        .env("FAKE_GH_STDIN_FILE", got)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    // gh-paced stops reading at the limit, so this write may fail with EPIPE.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let o = child.wait_with_output().unwrap();
    writer.join().unwrap();
    o
}

/// An alias that expands to a watch loop is held to the watch gate: before, gh received the
/// alias name and polled with nothing counting the requests.
#[test]
fn an_alias_to_a_watch_loop_is_refused_like_the_watch() {
    let sb = Sandbox::new("alias-watch", FAST);
    write_gh_config(
        &sb,
        "aliases:\n  w: run watch 99 --interval 30\n  cw: pr checks 5 --watch\n",
    );
    for alias in ["w", "cw"] {
        let o = sb.run(&[alias], &[]);
        assert_eq!(o.status.code(), Some(64), "{}", stderr(&o));
        assert!(
            stderr(&o).contains("GH_PACED_ALLOW_WATCH=1"),
            "{}",
            stderr(&o)
        );
        assert!(
            stderr(&o).contains(&format!("gh alias `{alias}`")),
            "{}",
            stderr(&o)
        );
    }
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    // With the gate open, gh runs the expanded watch, never the alias name.
    let o = sb.run(&["w"], &[("GH_PACED_ALLOW_WATCH", "1")]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let starts = sb.starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert!(starts[0].1.starts_with("run watch 99"), "{starts:?}");
}

/// Aliases written as a YAML flow mapping, and aliases reached after a flag that cobra skips
/// (`issue -R o/r upload`), are found the way gh finds them.
#[test]
fn flow_mapping_aliases_and_aliases_after_flags_are_found() {
    let sb = Sandbox::new("alias-flow", FAST);
    write_gh_config(
        &sb,
        "aliases: {up: 'api -X POST repos/o/r/issues/1/comments --input -',\n  issue upload: issue comment 7 --body-file -}\n",
    );
    let got = sb.path("stdin.out");
    let oversized = format!("{{\"body\":\"{}\"}}", "word ".repeat(2400));
    for args in [&["up"][..], &["issue", "-R", "o/r", "upload"][..]] {
        let o = run_with_stdin(&sb, &got, args, oversized.as_bytes());
        assert_eq!(o.status.code(), Some(65), "{args:?}: {}", stderr(&o));
    }
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    let o = run_with_stdin(&sb, &got, &["issue", "-R", "o/r", "upload"], b"short\n");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(std::fs::read(&got).unwrap(), b"short\n");
    let starts = sb.starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].1, "issue comment 7 --body-file - -R o/r");
}

/// When gh's configuration cannot be read, every command is refused with the configuration exit
/// status, gh's own commands included: any word may be an alias in that file. Before, built-in
/// commands ran. A line that names no command still runs.
#[test]
fn an_unreadable_gh_configuration_refuses_every_command() {
    let sb = Sandbox::new("alias-unreadable", FAST);
    write_gh_config(&sb, "aliases:\n  up: !!str api -X POST x --input -\n");
    for args in [&["up"][..], &["pr", "view", "1"][..]] {
        let o = sb.run(args, &[]);
        assert_eq!(o.status.code(), Some(78), "{args:?}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("gh's configuration cannot be read"),
            "{args:?}: {}",
            stderr(&o)
        );
    }
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    let o = sb.run(&["--version"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.starts());
    // An ambiguous position (a flag of unknown arity before the alias name) is refused too.
    write_gh_config(&sb, "aliases:\n  issue up: issue comment 7 --body-file -\n");
    let o = sb.run(&["issue", "--flag", "up", "x"], &[]);
    assert_eq!(o.status.code(), Some(64), "{}", stderr(&o));
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.starts());
}

/// gh adds its `help` command after the aliases, so an alias named `help` can run in its place.
/// Before, when the configuration could not be read (a `!!str` tag, a NEL inside a quoted
/// expansion, a `GH_CONFIG_DIR` that is not UTF-8), gh-paced took `gh help` for gh's own
/// command and ran it without admission, and gh ran the watch loop.
#[test]
fn a_help_alias_in_an_unreadable_configuration_is_refused() {
    use std::os::unix::ffi::OsStrExt;
    let sb = Sandbox::new("alias-help", FAST);
    for text in [
        "aliases:\n  help: !!str 'run watch 99 --interval 1'\n",
        "aliases:\n  help: \"run\u{85}    watch 99 --interval 1\"\n",
    ] {
        write_gh_config(&sb, text);
        for args in [&["help"][..], &["help", "x"][..]] {
            let o = sb.run(args, &[]);
            assert_eq!(
                o.status.code(),
                Some(78),
                "{text:?} {args:?}: {}",
                stderr(&o)
            );
            assert!(
                stderr(&o).contains("gh's configuration cannot be read"),
                "{text:?} {args:?}: {}",
                stderr(&o)
            );
        }
    }
    let dir = sb.path("").join(std::ffi::OsStr::from_bytes(b"cfg-\xff"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yml"),
        "aliases:\n  help: run watch 99 --interval 1\n",
    )
    .unwrap();
    let o = sb
        .cmd(&["help"])
        .env("GH_CONFIG_DIR", &dir)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
}

/// A regular file in gh's extensions directory makes gh register no extensions, so an alias named
/// after an installed extension runs. Before, when the configuration could not be read, gh-paced
/// dropped what it knew about that and ran `gh myext` as the extension, and gh ran the watch.
#[test]
fn an_extension_name_in_an_unreadable_configuration_is_refused() {
    let sb = Sandbox::new("alias-ext-unreadable", FAST);
    let cfg = sb.path("cfg");
    let exts = sb.path("data/gh/extensions");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(exts.join("gh-myext")).unwrap();
    std::fs::write(exts.join("gh-bad"), "").unwrap();
    std::fs::write(
        cfg.join("config.yml"),
        "aliases:\n  myext: !!str 'run watch 99 --interval 1'\n",
    )
    .unwrap();
    let data = sb.path("data");
    let env = [
        ("GH_CONFIG_DIR", cfg.to_str().unwrap()),
        ("XDG_DATA_HOME", data.to_str().unwrap()),
    ];
    let o = sb.run(&["myext"], &env);
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("gh's configuration cannot be read"),
        "{}",
        stderr(&o)
    );
    // Without the stray file gh registers the extension, but the file still cannot be read.
    std::fs::remove_file(exts.join("gh-bad")).unwrap();
    let o = sb.run(&["myext"], &env);
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
}

/// gh names an alias command after the first space-separated word of its last name word, so
/// `"issue 'publish hidden'"` runs as `gh issue publish`. Before, gh-paced looked for the whole
/// word, passed `issue publish` (and an alias expanding to it) to gh unexpanded, and gh ran an
/// unchecked watch loop.
#[test]
fn an_alias_name_with_a_space_runs_as_its_first_word() {
    let sb = Sandbox::new("alias-space", FAST);
    write_gh_config(
        &sb,
        "aliases:\n  a: issue publish\n  \"issue 'publish hidden'\": run watch 99 --interval 1\n",
    );
    for args in [&["a"][..], &["issue", "publish"][..]] {
        let o = sb.run(args, &[]);
        assert_eq!(o.status.code(), Some(64), "{args:?}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("GH_PACED_ALLOW_WATCH=1"),
            "{args:?}: {}",
            stderr(&o)
        );
    }
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    // A first word that is also a built-in command's: which of the two gh runs is not known.
    write_gh_config(&sb, "aliases:\n  \"issue 'view all'\": issue list\n");
    let o = sb.run(&["issue", "view", "1"], &[]);
    assert_eq!(o.status.code(), Some(64), "{}", stderr(&o));
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
}

/// YAML keys and values may start with `-`, `?` or `:` when a non-blank character follows, as
/// gh's YAML library reads them; before, gh-paced refused the whole file (exit 78).
#[test]
fn aliases_starting_with_dash_question_or_colon_are_read() {
    let sb = Sandbox::new("alias-indicator", FAST);
    write_gh_config(
        &sb,
        "aliases:\n  pv: -R o/r pr view\n  ?w: run watch 99\n  :w: run watch 98\n",
    );
    let o = sb.run(&["pv", "1"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let starts = sb.starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].1, "-R o/r pr view 1");
    for alias in ["?w", ":w"] {
        let o = sb.run(&[alias], &[]);
        assert_eq!(o.status.code(), Some(64), "{alias}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("GH_PACED_ALLOW_WATCH=1"),
            "{alias}: {}",
            stderr(&o)
        );
    }
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.starts());
}

/// libyaml reads NEL as a line break and skips a byte order mark at the start of any line, so
/// gh can see an alias that a line-by-line reader misses. Such a file is refused, not guessed.
#[test]
fn a_configuration_with_line_breaks_gh_reads_differently_is_refused() {
    let sb = Sandbox::new("alias-nel", FAST);
    for text in [
        "aliases:\n  w: \"run\u{85}    watch 99 --interval 1\"\n",
        "aliases:\n  co: pr checkout\u{85}  w: run watch 99\n",
        "\u{feff}\u{feff}aliases:\n  w: run watch 99\n",
    ] {
        write_gh_config(&sb, text);
        let o = sb.run(&["w"], &[]);
        assert_eq!(o.status.code(), Some(78), "{text:?}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("gh's configuration cannot be read"),
            "{text:?}: {}",
            stderr(&o)
        );
    }
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
}

/// `GH_CONFIG_DIR` holding bytes that are not UTF-8 is still where gh reads its aliases; before,
/// gh-paced took it as unset, read `$HOME/.config/gh` instead and passed the alias through. Every
/// command is refused, as for any configuration that cannot be read.
#[test]
fn a_gh_config_dir_that_is_not_utf8_refuses_every_command() {
    use std::os::unix::ffi::OsStrExt;
    let sb = Sandbox::new("alias-nonutf8", FAST);
    let dir = sb.path("").join(std::ffi::OsStr::from_bytes(b"cfg-\xff"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yml"), "aliases:\n  up: run watch 99\n").unwrap();
    let o = sb.cmd(&["up"]).env("GH_CONFIG_DIR", &dir).output().unwrap();
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("`GH_CONFIG_DIR` is not UTF-8"),
        "{}",
        stderr(&o)
    );
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
    let o = sb
        .cmd(&["pr", "view", "1"])
        .env("GH_CONFIG_DIR", &dir)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
}

/// An installed extension runs instead of an alias of the same name, as in gh; before, gh-paced
/// refused the call (exit 64). An entry that can make gh register no extension keeps it refused.
#[test]
fn an_extension_runs_instead_of_an_alias_of_its_name() {
    let sb = Sandbox::new("alias-ext", FAST);
    let exts = sb.path("home/.local/share/gh/extensions");
    std::fs::create_dir_all(exts.join("gh-myext")).unwrap();
    write_gh_config(&sb, "aliases:\n  myext: run watch 99\n");
    let o = sb.run(&["myext", "x"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let starts = sb.starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].1, "myext x");
    std::fs::write(exts.join("gh-other"), "").unwrap();
    let o = sb.run(&["myext", "x"], &[]);
    assert_eq!(o.status.code(), Some(64), "{}", stderr(&o));
    assert_eq!(sb.starts().len(), 1, "{:?}", sb.starts());
}

/// A refused alias's expansion can hold a body; the refusal names the alias and keeps the
/// expansion out of the audit log and the banner.
#[test]
fn a_refused_alias_keeps_its_expansion_out_of_the_records() {
    let sb = Sandbox::new("alias-leak", FAST);
    write_gh_config(
        &sb,
        "aliases:\n  post: issue comment $1 --body CANARY_PRIVATE_BODY\n",
    );
    let o = sb.run(&["post"], &[]);
    assert_eq!(o.status.code(), Some(64), "{}", stderr(&o));
    assert!(stderr(&o).contains("the alias `post`"), "{}", stderr(&o));
    assert!(
        !stderr(&o).contains("CANARY_PRIVATE_BODY"),
        "{}",
        stderr(&o)
    );
    let audit = std::fs::read_to_string(sb.path("state/test.audit.jsonl")).unwrap();
    assert!(audit.contains("gh alias:"), "{audit}");
    assert!(!audit.contains("CANARY_PRIVATE_BODY"), "{audit}");
    assert!(sb.starts().is_empty(), "gh ran: {:?}", sb.starts());
}

/// An editor script at a path with a space in it, so the quoting gh-paced adds is exercised. It
/// checks its own argument and replaces the file gh gave it with `$FAKE_EDITOR_TEXT`.
fn fake_editor(sb: &Sandbox) -> String {
    let path = sb.path("my editor");
    std::fs::write(
        &path,
        "#!/bin/bash\n[ \"$1\" = -x ] || exit 9\ncat \"$FAKE_EDITOR_TEXT\" > \"$2\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!("'{}' -x", path.display())
}

fn editor_line(sb: &Sandbox) -> String {
    sb.log()
        .into_iter()
        .rev()
        .find(|(_, ev, _)| ev == "editor")
        .map(|(_, _, a)| a)
        .unwrap_or_default()
}

/// gh composes the body of an interactive `issue create` (and the commit message of an
/// interactive `pr merge`) in an editor, where no argument shows it. gh-paced makes gh open the
/// editor through `gh-paced --edit-guard`, which runs the user's editor and then checks the text.
#[test]
fn text_written_in_the_editor_is_checked() {
    let sb = Sandbox::new("editor-guard", FAST);
    let editor = fake_editor(&sb);
    let edit_file = sb.path("gh-edit.md").display().to_string();
    let text_file = sb.path("text.md").display().to_string();
    let run = |args: &[&str], text: &str, extra: &[(&str, &str)]| -> Output {
        std::fs::write(&text_file, text).unwrap();
        let mut env = vec![
            ("EDITOR", editor.as_str()),
            ("FAKE_GH_EDIT", edit_file.as_str()),
            ("FAKE_EDITOR_TEXT", text_file.as_str()),
        ];
        env.extend_from_slice(extra);
        sb.run(args, &env)
    };
    // 20 KB of prose: the guard refuses it, gh stops, and the text is kept.
    let big = "word ".repeat(4000);
    let o = run(&["issue", "create", "-t", "title"], &big, &[]);
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains(
            "GH-PACED REFUSED [test] content guard: write body is 20005 bytes across 1 \
             source(s) (including 5 bytes given on the command line)"
        ),
        "{err}"
    );
    assert!(
        sb.log()
            .iter()
            .any(|(_, ev, a)| ev == "editor-failed" && a == "65"),
        "{err}"
    );
    let kept = err
        .lines()
        .find_map(|l| l.split("your text is kept in ").nth(1))
        .expect("kept path");
    assert_eq!(std::fs::read_to_string(kept.trim()).unwrap(), big);
    assert!(
        editor_line(&sb).contains("--edit-guard 'test'"),
        "{}",
        editor_line(&sb)
    );
    // A base64 run is refused the same way.
    let o = run(
        &["pr", "merge", "1"],
        &format!("Merge it\n\n{}\n", "QUJD".repeat(500)),
        &[],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("base64-looking run of 2000"),
        "{}",
        stderr(&o)
    );
    // A short note passes, exactly as the editor wrote it.
    let note = "Fix the flaky retry loop.\n\nSee the log on the host.\n";
    let o = run(&["issue", "create", "-t", "title"], note, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(std::fs::read_to_string(&edit_file).unwrap(), note);
    let edited: Vec<_> = sb
        .log()
        .into_iter()
        .filter(|(_, ev, _)| ev == "edited")
        .collect();
    assert_eq!(edited.len(), 1, "{edited:?}");
    assert_eq!(edited[0].2, note.len().to_string());
    // The editor text and the arguments share one allowance. An 8,000-byte body passes after a
    // 5-byte title, but a 200-byte title on the same call pushes the total over 8,192.
    let body = "word ".repeat(1600);
    let o = run(&["issue", "create", "-t", "title"], &body, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let title = "word ".repeat(40);
    let o = run(&["issue", "create", "-t", &title], &body, &[]);
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains(
            "write body is 8200 bytes across 1 source(s) (including 200 bytes given on the \
             command line), over the 8192-byte limit"
        ),
        "{err}"
    );
    // The wrapper sets the allowance variable for its child itself, replacing a caller's value.
    let o = run(
        &["issue", "create", "-t", "title"],
        note,
        &[("GH_PACED_BODY_BYTES_USED", "lots")],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    // The guard itself refuses a value that is not a byte count rather than counting it as 0.
    std::fs::write(&edit_file, note).unwrap();
    let o = Command::new(BIN)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", sb.path("home"))
        .env("GH_PACED_STATE_DIR", sb.path("state"))
        .env("GH_PACED_CONFIG", sb.path("config.json"))
        .env("GH_PACED_EDITOR", "true")
        .env("GH_PACED_BODY_BYTES_USED", "lots")
        .args(["--edit-guard", "test", edit_file.as_str()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(78), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("GH_PACED_BODY_BYTES_USED=\"lots\" is not a byte count"),
        "{}",
        stderr(&o)
    );
    // A GH_EDITOR the caller set is the editor the guard runs.
    let o = run(
        &["issue", "create", "-t", "title"],
        note,
        &[("EDITOR", "false"), ("GH_EDITOR", editor.as_str())],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    // Reads get no guard, and neither does a write under GH_PACED_ALLOW_LARGE_BODY=1.
    let o = run(&["pr", "view", "1"], note, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(editor_line(&sb), editor);
    let o = run(
        &["issue", "create", "-t", "title"],
        &big,
        &[("GH_PACED_ALLOW_LARGE_BODY", "1")],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(editor_line(&sb), editor);
}

/// A pseudo-terminal `(master, slave)`; the slave as stdin makes gh-paced see a terminal.
fn pty_pair() -> (std::fs::File, std::fs::File) {
    use std::os::unix::io::FromRawFd;
    // SAFETY: standard pty allocation; each descriptor is checked and then owned by a File.
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        assert!(m >= 0);
        let master = std::fs::File::from_raw_fd(m);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(m, name.as_mut_ptr(), name.len()), 0);
        let s = libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        assert!(s >= 0);
        (master, std::fs::File::from_raw_fd(s))
    }
}

/// On a terminal, a bare `gh issue create` or `gh pr merge` prompts and opens the editor. These
/// run, with the editor guard installed; `gh pr create` still does not, since gh can submit a
/// body composed from commit messages without opening the editor.
#[test]
fn interactive_issue_create_and_pr_merge_run_on_a_terminal() {
    let sb = Sandbox::new("interactive", FAST);
    let editor = fake_editor(&sb);
    let text_file = sb.path("text.md");
    std::fs::write(&text_file, "A short body.\n").unwrap();
    let run = |args: &[&str]| -> Output {
        let (_master, slave) = pty_pair();
        sb.cmd(args)
            .env("EDITOR", &editor)
            .env("FAKE_GH_EDIT", sb.path("gh-edit.md"))
            .env("FAKE_EDITOR_TEXT", &text_file)
            .stdin(Stdio::from(slave))
            .output()
            .unwrap()
    };
    let o = run(&["issue", "create"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        editor_line(&sb).contains("--edit-guard"),
        "{}",
        editor_line(&sb)
    );
    let o = run(&["pr", "merge", "1"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let o = run(&["pr", "create"]);
    assert_eq!(o.status.code(), Some(65), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("composed from commit messages"),
        "{}",
        stderr(&o)
    );
    let starts: Vec<String> = sb.starts().into_iter().map(|(_, a)| a).collect();
    assert_eq!(starts, vec!["issue create", "pr merge 1"]);
}

/// Run `pr list` while a helper gh started keeps gh's stderr after gh exits. Stderr goes to a
/// file, so gh-paced's own exit is timed apart from the helper. Returns gh-paced's exit status,
/// the time it exited, the helper's first write time (from the log), and the complete stderr
/// once the helper has finished.
fn run_with_late_helper(
    sb: &Sandbox,
    quiet: &str,
    lines: &str,
    text: &str,
) -> (std::process::ExitStatus, f64, f64, String) {
    let epoch = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    };
    let err_path = sb.path("stderr.txt");
    let mut c = sb.cmd(&["pr", "list"]);
    c.env("FAKE_GH_LATE_ERR", quiet)
        .env("FAKE_GH_LATE_LINES", lines)
        .env("FAKE_GH_LATE_TEXT", text)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err_path).unwrap());
    let status = c.status().unwrap();
    let exited = epoch();
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    while !sb.log().iter().any(|(_, ev, _)| ev == "helper-end") && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let first_write = sb
        .log()
        .into_iter()
        .find(|(_, ev, _)| ev == "helper-write")
        .map_or(f64::NAN, |(t, _, _)| t);
    let err = std::fs::read_to_string(&err_path).unwrap();
    (status, exited, first_write, err)
}

/// Every helper line arrives once, in order.
fn assert_lines_in_order(err: &str, lines: usize) {
    let mut at = 0;
    for i in 1..=lines {
        let line = format!("helper line {i} ");
        assert_eq!(err.matches(&line).count(), 1, "line {i}: {err}");
        let pos = err.find(&line).unwrap();
        assert!(pos >= at, "line {i} out of order: {err}");
        at = pos;
    }
}

/// A process gh started that stays quiet for longer than gh-paced's idle cutoff and then writes
/// to stderr still reaches the caller, and the pushback in what it writes is recorded; gh-paced
/// itself exits without waiting for that process.
#[test]
fn late_output_after_a_quiet_spell_is_delivered_and_scanned() {
    let sb = Sandbox::new("late-quiet", FAST);
    let (status, exited, first_write, err) = run_with_late_helper(
        &sb,
        "4",
        "4",
        "HTTP 403: You have exceeded a secondary rate limit",
    );
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(
        exited < first_write,
        "gh-paced exited {} s after the helper's first write",
        exited - first_write
    );
    assert_lines_in_order(&err, 4);
    assert!(!err.contains("GH-PACED WARNING"), "{err}");
    let cd: serde_json::Value = std::fs::read_to_string(sb.path("state/test.cooldown"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let reason = cd["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("secondary"),
        "no cooldown from the late output: {cd}\n{err}"
    );
    assert_eq!(cd["command"], "pr list", "{cd}");
    let next = sb.run(&["pr", "view", "1"], &[("GH_PACED_MAX_WAIT", "5")]);
    assert_eq!(next.status.code(), Some(75), "{}", stderr(&next));
    assert!(stderr(&next).contains("cooldown"), "{}", stderr(&next));
}

/// A process gh started that keeps writing past gh-paced's elapsed cutoff loses nothing.
#[test]
fn late_output_past_the_elapsed_cutoff_is_delivered() {
    let sb = Sandbox::new("late-steady", FAST);
    let (status, _, _, err) = run_with_late_helper(&sb, "0", "16", "steady");
    assert_eq!(status.code(), Some(0), "{err}");
    assert_lines_in_order(&err, 16);
    assert!(!err.contains("GH-PACED WARNING"), "{err}");
}
