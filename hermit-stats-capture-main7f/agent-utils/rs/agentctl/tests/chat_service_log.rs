//! `chat run` writes the chat bridge's service log, and every line of it starts with the UTC time
//! it was written, including the final error line printed as the process exits. Other commands,
//! such as `chat tick`, print their final error line without a time.
//!
//! This drives the real binary as a subprocess, so it checks the line the process writes rather
//! than a helper's return value. Each run points at a bridge state that does not exist, a scratch
//! registry, scratch home directories, and a Herdr executable that does not exist, so it fails
//! before touching anything.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long one run may take before it is killed. Each run fails at once, so this bounds a hang.
const RUN_LIMIT: Duration = Duration::from_secs(60);

struct Failure {
    code: Option<i32>,
    stderr: String,
    state: PathBuf,
}

/// Run `agentctl chat <subcommand>` in a scratch directory named for `label`. Standard error goes
/// to `device` when one is given, such as `/dev/full`; otherwise it goes to a scratch file and is
/// returned in `Failure::stderr`.
fn run_chat_against_absent_state(label: &str, subcommand: &str, device: Option<&Path>) -> Failure {
    let root = std::env::temp_dir().join(format!(
        "agentctl-chat-service-log-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create scratch directory");
    let state = root.join("absent-state");
    let registry = root.join("registry");
    let stderr_path = root.join("stderr");
    let stderr = match device {
        Some(device) => fs::OpenOptions::new().write(true).open(device),
        None => fs::File::create(&stderr_path),
    }
    .expect("open standard error");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .current_dir(&root)
        .env("HOME", &home)
        .env("AGENTCTL_HOME", home.join(".agentctl"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_RUNTIME_DIR", home.join("runtime"))
        .arg("--registry")
        .arg(&registry)
        .arg("--herdr-bin")
        .arg(root.join("absent-herdr"))
        .args(["chat", subcommand, "--bridge-state"])
        .arg(&state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("run agentctl");
    let status = wait_at_most(&mut child, RUN_LIMIT);
    let created = state.exists() || registry.exists();
    let stderr = match device {
        Some(_) => String::new(),
        None => String::from_utf8(fs::read(&stderr_path).expect("read standard error"))
            .expect("UTF-8 standard error"),
    };
    fs::remove_dir_all(&root).expect("remove scratch directory");
    let status = status.unwrap_or_else(|| {
        panic!("chat {subcommand} ran longer than {RUN_LIMIT:?} and was killed: {stderr}")
    });
    assert!(!created, "chat {subcommand} created state or a registry");
    Failure {
        code: status.code(),
        stderr,
        state,
    }
}

/// Wait at most `limit` for `child` to exit. Past that, kill it and return `None`.
fn wait_at_most(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("poll agentctl") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn chat_run_final_error_line_starts_with_the_utc_second_and_chat_tick_has_none() {
    let run = run_chat_against_absent_state("run", "run", None);
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    let line = run.stderr.strip_suffix('\n').expect("one complete line");
    assert!(!line.contains('\n'), "{}", run.stderr);
    let (stamp, text) = line
        .split_once(' ')
        .expect("the time, a space, then the line");
    let shape = "0000-00-00T00:00:00Z";
    assert!(
        stamp.len() == shape.len()
            && stamp
                .chars()
                .zip(shape.chars())
                .all(|(actual, form)| if form == '0' {
                    actual.is_ascii_digit()
                } else {
                    actual == form
                }),
        "{line}"
    );
    let expected = format!(
        "agentctl: cannot inspect chat state directory {}: ",
        run.state.display()
    );
    assert!(text.starts_with(&expected), "{line}");

    let tick = run_chat_against_absent_state("tick", "tick", None);
    assert_eq!(tick.code, Some(1), "{}", tick.stderr);
    let line = tick.stderr.strip_suffix('\n').expect("one complete line");
    assert!(!line.contains('\n'), "{}", tick.stderr);
    let expected = format!(
        "agentctl: cannot inspect chat state directory {}: ",
        tick.state.display()
    );
    assert!(line.starts_with(&expected), "{line}");
}

/// A service manager's log can fill its disk. `chat run` then loses its final error line but still
/// exits 1, as for any other failure, rather than panicking on the failed write and exiting 101 as
/// if it had crashed.
#[cfg(target_os = "linux")]
#[test]
fn chat_run_exits_one_when_its_final_error_line_cannot_be_written() {
    let run = run_chat_against_absent_state("run-dev-full", "run", Some(Path::new("/dev/full")));
    assert_eq!(run.code, Some(1));
}
