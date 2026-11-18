/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The experimental fork server (`HERMIT_FORK_SERVER`): one `hermit run`
//! launch serves many runs of its command line, and each served run is the
//! run a plain invocation of that command line makes.
//!
//! Served and plain runs use the same command line, so the guest sees the same
//! arguments; `--base-env=minimal` keeps the server's own environment out of
//! the guest's.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Output;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// Wall backstop for a server's whole life, for a hung tracer.
const SERVER_SECONDS: u64 = 300;

/// Per-run timeout a served run gets, in milliseconds.
const RUN_MS: u64 = 60_000;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

/// The repository's chaos schedule-diversity guest, compiled once. Its
/// `contention` mode prints a trace that depends on the schedule.
fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let source = repository.join("tests/e2e/determinism-stress/thread_contention.c");
        let output = scratch("guest").join("thread_contention");
        let mut command = Command::new("cc");
        command
            .args([
                "-std=c11", "-O2", "-g", "-pthread", "-Wall", "-Wextra", "-Werror",
            ])
            .arg(&source)
            .arg("-o")
            .arg(&output);
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("failed to run cc: {error}"));
        assert!(status.success(), "guest compilation failed: {command:?}");
        output
    })
}

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("fork-server-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create the test directory");
    dir
}

/// `hermit run <options> -- <program>` in `dir`, bounded by `seconds`.
fn hermit_command(dir: &Path, seconds: u64, options: &[String], program: &[&str]) -> Command {
    let mut command = Command::new("timeout");
    command
        .current_dir(dir)
        .arg("--kill-after=2s")
        .arg(format!("{seconds}s"))
        .arg(hermit_test::hermit_binary())
        .args(["run", "--base-env=minimal", "--cpu-profile=x86-64-v2"])
        .args(options)
        .arg("--")
        .args(program);
    hermit_test::configure_guest_execution(&mut command);
    command
}

/// A fork server listening on `s.sock` in its directory.
struct Server {
    child: Child,
    /// Keeps the directory's descriptor, through which the socket is named:
    /// the directory's absolute path can exceed a socket address's 108 bytes.
    directory: fs::File,
    connection: Option<(BufReader<UnixStream>, UnixStream)>,
    stderr: PathBuf,
}

impl Server {
    /// Start `hermit run <options> -- <program>` as a fork server in `dir`,
    /// and connect to it.
    fn start(dir: &Path, options: &[String], program: &[&str]) -> Self {
        Self::start_with_env(dir, options, program, &[])
    }

    /// [`Server::start`] with `env` added to the server's environment.
    fn start_with_env(
        dir: &Path,
        options: &[String],
        program: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let stderr = dir.join("server.stderr");
        let mut command = hermit_command(dir, SERVER_SECONDS, options, program);
        command
            .envs(env.iter().copied())
            .env("HERMIT_FORK_SERVER", "s.sock")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&stderr).unwrap());
        let child = command.spawn().expect("failed to start the server");
        let directory = fs::File::open(dir).expect("open the test directory");
        let mut server = Server {
            child,
            directory,
            connection: None,
            stderr,
        };
        server.connect();
        server
    }

    fn socket(&self) -> String {
        format!("/proc/self/fd/{}/s.sock", self.directory.as_raw_fd())
    }

    fn connect(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Ok(stream) = UnixStream::connect(self.socket()) {
                let reader = BufReader::new(stream.try_clone().unwrap());
                self.connection = Some((reader, stream));
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "the server exited ({status}) before listening:\n{}",
                    fs::read_to_string(&self.stderr).unwrap_or_default()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the server did not listen within 120 s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Send one request line and return the reply line.
    fn request(&mut self, line: &str) -> String {
        self.send(line);
        self.reply()
    }

    fn send(&mut self, line: &str) {
        let (_, writer) = self.connection.as_mut().unwrap();
        writeln!(writer, "{line}").unwrap();
    }

    fn reply(&mut self) -> String {
        let (reader, _) = self.connection.as_mut().unwrap();
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        reply.trim_end().to_owned()
    }

    /// Serve one run with stdin `/dev/null`, stdout and stderr to
    /// `<name>.out` and `<name>.err` in `dir`, as an [`Output`], so that the
    /// harness's skid retry applies to it as to a plain run.
    fn run(&mut self, dir: &Path, name: &str) -> Output {
        self.run_reading(dir, name, Path::new("/dev/null"))
    }

    /// [`Server::run`] with stdin from `stdin`.
    fn run_reading(&mut self, dir: &Path, name: &str, stdin: &Path) -> Output {
        let (out, err) = (
            dir.join(format!("{name}.out")),
            dir.join(format!("{name}.err")),
        );
        let label = format!("served run {name}");
        let (output, ()) = hermit_test::hermit_output_with(&label, || {
            let reply = self.request(&format!(
                "RUN {} {} {} {RUN_MS}",
                stdin.display(),
                out.display(),
                err.display()
            ));
            let words: Vec<&str> = reply.split(' ').collect();
            let ["STATUS", code, _elapsed] = words.as_slice() else {
                panic!("{label}: reply {reply:?}");
            };
            let code: i32 = code.parse().unwrap();
            let status = if code < 128 {
                ExitStatus::from_raw(code << 8)
            } else {
                ExitStatus::from_raw(code - 128)
            };
            let output = Output {
                status,
                stdout: fs::read(&out).unwrap(),
                stderr: fs::read(&err).unwrap(),
            };
            (output, ())
        });
        output
    }

    /// Ask the server to stop, and return its exit status.
    fn quit(mut self) -> ExitStatus {
        let (_, writer) = self.connection.as_mut().unwrap();
        writeln!(writer, "QUIT").unwrap();
        self.child.wait().unwrap()
    }

    /// The server's direct children: none between runs.
    fn children(&self) -> Vec<u32> {
        children_of(self.hermit())
    }

    /// The server's Hermit process: `timeout` runs it as its only child.
    fn hermit(&self) -> u32 {
        *children_of(self.child.id())
            .first()
            .expect("Hermit is running")
    }
}

impl Drop for Server {
    /// Stop the server without orphaning its Hermit process. SIGTERM to
    /// `timeout` is forwarded to Hermit, and `--kill-after=2s` follows it with
    /// SIGKILL. SIGKILL to `timeout` itself would leave Hermit running with no
    /// deadline, listening on its socket forever.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let pid = libc::pid_t::try_from(self.child.id()).expect("a pid fits pid_t");
            // SAFETY: kill(2) on our own child, which is not reaped yet, so
            // its pid cannot have been reused.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let _ = poll_until(Duration::from_secs(30), || {
                self.child.try_wait().ok().flatten()
            });
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The processes whose parent is `parent`, from `/proc/<pid>/stat` (this
/// kernel need not have `/proc/<pid>/task/<tid>/children`).
fn children_of(parent: u32) -> Vec<u32> {
    let mut children = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The fields after the parenthesized command are state, then ppid.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|ppid| ppid.parse::<u32>().ok());
        if ppid == Some(parent) {
            children.push(pid);
        }
    }
    children
}

/// Whether `pid` is running (not gone, and not a zombie).
fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z")
    })
}

/// Poll `probe` every 50 ms until it gives a value or `budget` passes.
fn poll_until<T>(budget: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The chaos options shared by the served and plain runs of the guest.
fn chaos_options(dir: &Path) -> Vec<String> {
    vec![
        "--no-virtualize-cpuid".into(),
        "--chaos".into(),
        "--sched-seed=3".into(),
        format!("--choice-tape={}", dir.join("tape").display()),
        format!("--choice-record={}", dir.join("record.jsonl").display()),
    ]
}

/// Tapes that each lead the guest through another schedule.
fn tape(k: u8) -> Vec<u8> {
    (0..64u8)
        .map(|i| {
            if (i + k).is_multiple_of(3) {
                0
            } else {
                i.wrapping_mul(37).wrapping_add(k * 29)
            }
        })
        .collect()
}

#[test]
fn served_runs_are_the_plain_runs_of_the_command_line() {
    let dir = scratch("identity");
    let options = chaos_options(&dir);
    let program = [guest().to_str().unwrap(), "contention"];
    let mut served = Vec::new();
    // `hermit run` checks that the tape is readable before it serves.
    fs::write(dir.join("tape"), b"").unwrap();
    let mut server = Server::start(&dir, &options, &program);
    for k in 0..3 {
        fs::write(dir.join("tape"), tape(k)).unwrap();
        let output = server.run(&dir, &format!("served{k}"));
        assert!(output.status.success(), "served run {k}: {output:?}");
        served.push((output.stdout, fs::read(dir.join("record.jsonl")).unwrap()));
        assert_eq!(
            server.children(),
            Vec::<u32>::new(),
            "a served run outlived its reply"
        );
    }
    assert!(server.quit().success(), "the server did not exit 0 on QUIT");
    for (k, (stdout, record)) in served.iter().enumerate() {
        fs::write(dir.join("tape"), tape(k as u8)).unwrap();
        let mut command = hermit_command(&dir, 120, &options, &program);
        let plain = hermit_test::hermit_output(&format!("plain run {k}"), &mut command);
        assert!(plain.status.success(), "plain run {k}: {plain:?}");
        assert_eq!(&plain.stdout, stdout, "tape {k}: served stdout differs");
        assert_eq!(
            &fs::read(dir.join("record.jsonl")).unwrap(),
            record,
            "tape {k}: served choice record differs"
        );
    }
    assert_ne!(
        served[0].1, served[1].1,
        "the tapes should make other choices"
    );
}

#[test]
fn each_served_run_reads_its_own_stdin() {
    let dir = scratch("stdin");
    let mut server = Server::start(&dir, &[], &["/bin/cat"]);
    for k in 0..2 {
        let input = dir.join(format!("in{k}"));
        fs::write(&input, format!("input of run {k}\n")).unwrap();
        let output = server.run_reading(&dir, &format!("cat{k}"), &input);
        assert!(output.status.success(), "run {k}: {output:?}");
        assert_eq!(output.stdout, format!("input of run {k}\n").as_bytes());
    }
    assert!(server.quit().success());
}

/// A test that fails before QUIT drops its server: the drop must stop the
/// server's Hermit process too, not only the `timeout` wrapper around it.
#[test]
fn a_dropped_server_leaves_no_hermit_running() {
    let dir = scratch("drop");
    let server = Server::start(&dir, &[], &["/bin/true"]);
    let hermit = server.hermit();
    assert!(alive(hermit), "the server's Hermit is not running");
    drop(server);
    assert!(
        !alive(hermit),
        "Hermit (pid {hermit}) outlived its dropped server"
    );
}

/// A served run's container init watches the run's own process, not the
/// server's (the child renews the pidfd it inherited). A run killed while its
/// init is still setting up, before the init arms its parent-death signal,
/// ends the init at once. Watching the server, which lives on, the init would
/// wait out its whole setup stall and then run the guest unbounded.
#[test]
fn a_served_runs_init_watches_the_run_not_the_server() {
    // The container init's test hook (container.rs): it stalls up to 60 s
    // before it arms, renamed to `unarmed-init`, and ends the stall early when
    // the process that forked it exits.
    const STALL_ENV: &str = "HERMIT_INTERNAL_STALL_CONTAINER_INIT_BEFORE_ARMING";
    let dir = scratch("init");
    let mut server = Server::start_with_env(&dir, &[], &["/bin/true"], &[(STALL_ENV, "1")]);
    server.send("RUN /dev/null run.out run.err 0");
    let hermit = server.hermit();
    let run = poll_until(Duration::from_secs(60), || {
        children_of(hermit).first().copied()
    })
    .expect("the server forked no run");
    let init = poll_until(Duration::from_secs(60), || {
        children_of(run).into_iter().find(|pid| {
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|comm| comm.trim_end() == "unarmed-init")
        })
    })
    .expect("the run's container init never stalled before arming");
    // SAFETY: `kill` takes a pid and a signal and touches no caller memory;
    // `run` is the server's child, observed above.
    assert_eq!(unsafe { libc::kill(run as i32, libc::SIGKILL) }, 0);
    let gone = poll_until(Duration::from_secs(15), || (!alive(init)).then_some(()));
    assert!(
        gone.is_some(),
        "the init outlived its killed run by 15 s: it is watching the server"
    );
    let reply = server.reply();
    assert!(reply.starts_with("STATUS 137 "), "{reply:?}");
    assert!(server.quit().success());
}

#[test]
fn a_run_past_its_timeout_is_killed_and_the_server_goes_on() {
    let dir = scratch("timeout");
    let mut server = Server::start(&dir, &[], &["/bin/sh", "-c", "while :; do :; done"]);
    for name in ["first", "second"] {
        let started = Instant::now();
        let reply = server.request(&format!("RUN /dev/null {name}.out {name}.err 1500"));
        assert!(reply.starts_with("TIMEOUT "), "{name}: {reply:?}");
        assert!(
            started.elapsed() >= Duration::from_millis(1500),
            "{name}: early"
        );
        assert_eq!(
            server.children(),
            Vec::<u32>::new(),
            "{name}: the killed run left a process"
        );
    }
    assert!(server.quit().success());
}

#[test]
fn a_request_that_starts_no_run_is_answered_err() {
    let dir = scratch("errors");
    let mut server = Server::start(&dir, &[], &["/bin/true"]);
    for (request, expected) in [
        ("RUN a b c", "ERR RUN takes"),
        (
            "RUN /dev/null a.out a.err soon",
            "ERR the timeout \"soon\" is not a whole number",
        ),
        (
            "RUN missing/in a.out a.err 0",
            "ERR cannot open the stdin file missing/in",
        ),
        (
            "RUN /dev/null missing/out a.err 0",
            "ERR cannot open the stdout file missing/out",
        ),
        ("HELLO", "ERR unknown request: HELLO"),
    ] {
        let reply = server.request(request);
        assert!(reply.starts_with(expected), "{request:?}: {reply:?}");
    }
    let output = server.run(&dir, "true");
    assert!(output.status.success(), "{output:?}");
    assert!(server.quit().success());
}

#[test]
fn modes_a_server_cannot_serve_are_policy_refusals() {
    let dir = scratch("refusals");
    for (options, named) in [
        (vec!["--verify".to_owned()], "--verify"),
        (
            vec![format!("--fatal-core-dir={}", dir.display())],
            "--fatal-core-dir",
        ),
    ] {
        let mut command = hermit_command(&dir, 120, &options, &["/bin/true"]);
        command.env("HERMIT_FORK_SERVER", "s.sock");
        let output = command.output().expect("failed to start Hermit");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{options:?}\n{stderr}"
        );
        assert!(stderr.contains("class=policy-refusal"), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "HERMIT_FORK_SERVER serves only an ordinary ptrace run; this invocation has \
                 {named}"
            )),
            "{stderr}"
        );
        assert!(!dir.join("s.sock").exists(), "a refused server listened");
    }
}

/// A server started on `s.sock` in `dir` that is expected to refuse it:
/// returns the exit code and stderr.
fn refused_server(dir: &Path) -> (Option<i32>, String) {
    let mut command = hermit_command(dir, 120, &[], &["/bin/true"]);
    command.env("HERMIT_FORK_SERVER", "s.sock");
    let output = command.output().expect("failed to start Hermit");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_socket_path_naming_another_file_is_refused_and_the_file_kept() {
    let dir = scratch("not-a-socket");
    let file = dir.join("s.sock");
    fs::write(&file, "not a socket\n").unwrap();
    let (code, stderr) = refused_server(&dir);
    assert_eq!(
        code,
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(stderr.contains("class=policy-refusal"), "{stderr}");
    assert!(
        stderr
            .contains("HERMIT_FORK_SERVER=s.sock: it names an existing file that is not a socket"),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "not a socket\n");
}

#[test]
fn a_socket_a_server_listens_on_is_refused_and_a_stale_one_replaced() {
    let dir = scratch("socket-claims");
    let mut first = Server::start(&dir, &[], &["/bin/true"]);
    let (code, stderr) = refused_server(&dir);
    assert_eq!(
        code,
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_FORK_SERVER=s.sock: another server listens on it"),
        "{stderr}"
    );
    let output = first.run(&dir, "first");
    assert!(
        output.status.success(),
        "the first server stopped serving: {output:?}"
    );
    assert!(first.quit().success());
    // The socket file the first server left behind is stale now.
    assert!(dir.join("s.sock").exists());
    let mut second = Server::start(&dir, &[], &["/bin/true"]);
    let output = second.run(&dir, "second");
    assert!(output.status.success(), "{output:?}");
    assert!(second.quit().success());
}
