/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Maintained controls over actual owner/API source; no keeper or policy load.
//! The native monitor and final controller exit are explicit test substitutes.
use std::fs::File;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::thread::{self};
use std::time::Duration;
use std::time::Instant;

const WALL: Duration = Duration::from_secs(30);
const CLEANUP: Duration = Duration::from_secs(5);
const LOG_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
struct Member {
    pid: i32,
    state: char,
    start_ticks: u64,
}
fn members(group: i32) -> io::Result<Vec<Member>> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let raw = match fs::read_to_string(entry.path().join("stat")) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let Some((_, tail)) = raw.rsplit_once(") ") else {
            return Err(io::Error::other("invalid owned-group stat"));
        };
        let words: Vec<_> = tail.split_whitespace().collect();
        if words.get(2).and_then(|x| x.parse::<i32>().ok()) == Some(group) {
            found.push(Member {
                pid,
                state: words[0].chars().next().unwrap(),
                start_ticks: words
                    .get(19)
                    .and_then(|x| x.parse().ok())
                    .ok_or_else(|| io::Error::other("invalid start_ticks"))?,
            });
        }
    }
    Ok(found)
}
fn exited_without_reap(child: &Child) -> io::Result<bool> {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } == child.id() as i32)
}
fn read_log<R: Read + Send + 'static>(
    mut input: R,
    path: PathBuf,
    errors: mpsc::Sender<String>,
) -> io::Result<JoinHandle<io::Result<()>>> {
    let mut file = File::create(path)?;
    thread::Builder::new()
        .name("guard-test-log".into())
        .spawn(move || {
            let result = (|| {
                let mut stored = 0;
                let mut bytes = [0u8; 8192];
                loop {
                    let size = input.read(&mut bytes)?;
                    if size == 0 {
                        return Ok(());
                    }
                    let keep = size.min(LOG_LIMIT - stored);
                    file.write_all(&bytes[..keep])?;
                    stored += keep;
                    if keep != size {
                        return Err(io::Error::other("1MiB log limit"));
                    }
                }
            })();
            if let Err(error) = &result {
                let _ = errors.send(format!("log reader: {error}"));
            }
            result
        })
}
/// If cleanup cannot finish, the receipt retains the actual child and reader
/// handles. A failure string is never substituted for those remaining owners.
#[derive(Debug)]
#[must_use]
struct Receipt {
    started: Instant,
    deadline: Instant,
    elapsed: Duration,
    terminal_observed_elapsed: Option<Duration>,
    cleanup_deadline: Instant,
    cleanup_observed_elapsed: Duration,
    status: Option<ExitStatus>,
    primary: Option<String>,
    cleanup: Vec<String>,
    group: i32,
    remaining: Vec<Member>,
    saw_closed_streams_while_alive: bool,
    pending_child: Option<Child>,
    pending_readers: Vec<JoinHandle<io::Result<()>>>,
}
#[derive(Clone, Copy)]
enum Inject {
    None,
    ErrorAfterSpawn,
    DelayFirstObservation(Duration),
    DelayCleanupObservation { budget: Duration, delay: Duration },
}
fn run(
    root: &Path,
    label: &str,
    command: &mut Command,
    wall: Duration,
    injection: Inject,
) -> io::Result<Receipt> {
    let started = Instant::now();
    let deadline = started + wall;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            for (kind, value) in [
                (libc::RLIMIT_AS, 2147483648),
                (libc::RLIMIT_NOFILE, 128),
                (libc::RLIMIT_CORE, 0),
            ] {
                let value = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(kind, &value) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    let group = child.id() as i32;
    let mut owned = Some(child);
    let mut readers = Vec::new();
    let mut primary = None;
    let (errors, error_rx) = mpsc::channel();
    let mut terminal_observed_elapsed = None;
    let mut closed_alive = false;
    // All fallible post-spawn work is inside this closure. Cleanup below runs
    // for every ordinary error, including EOF followed by a hung child.
    let body = (|| -> io::Result<()> {
        let child = owned.as_mut().unwrap();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout pipe"))?;
        readers.push(read_log(
            stdout,
            root.join(format!("{label}.stdout")),
            errors.clone(),
        )?);
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr pipe"))?;
        readers.push(read_log(
            stderr,
            root.join(format!("{label}.stderr")),
            errors,
        )?);
        match injection {
            Inject::ErrorAfterSpawn => {
                return Err(io::Error::other("injected error after owned spawn"));
            }
            Inject::DelayFirstObservation(delay) => thread::sleep(delay),
            Inject::None | Inject::DelayCleanupObservation { .. } => {}
        }
        loop {
            if let Ok(error) = error_rx.try_recv() {
                return Err(io::Error::other(error));
            }
            let exited = exited_without_reap(child)?;
            let observed_at = Instant::now();
            if exited {
                terminal_observed_elapsed = Some(observed_at.duration_since(started));
            }
            // Even a terminal child cannot pass if its completion was first
            // observed after the original deadline. Pipe EOF is not authority.
            if observed_at >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "child wall deadline",
                ));
            }
            if exited {
                return Ok(());
            }
            if readers.iter().all(JoinHandle::is_finished) {
                closed_alive = true;
            }
            thread::sleep(Duration::from_millis(2));
        }
    })();
    if let Err(error) = body {
        primary = Some(error.to_string());
    }
    // WNOWAIT above leaves this exact child unreaped, reserving its group ID
    // through the kill. Never signal a numeric group after relinquishing that
    // identity. Even ordinary leader exit gets this owned-descendant cleanup.
    let mut cleanup = Vec::new();
    let cleanup_started = Instant::now();
    let end = cleanup_started
        + match injection {
            Inject::DelayCleanupObservation { budget, .. } => budget.min(CLEANUP),
            _ => CLEANUP,
        };
    if unsafe { libc::kill(-group, libc::SIGKILL) } < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            cleanup.push(format!("owned kill: {error}"));
        }
    }
    let mut status = None;
    loop {
        match exited_without_reap(owned.as_ref().unwrap()) {
            Ok(true) => {
                match owned.as_mut().unwrap().wait() {
                    Ok(value) => {
                        status = Some(value);
                        owned.take();
                    }
                    Err(error) => cleanup.push(format!("owned reap: {error}")),
                };
                break;
            }
            Ok(false) => {}
            Err(error) => {
                cleanup.push(format!("owned exit observation: {error}"));
                break;
            }
        }
        if Instant::now() >= end {
            cleanup.push("owned reap deadline".into());
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let mut remaining = Vec::new();
    loop {
        match members(group) {
            Ok(found) => remaining = found,
            Err(error) => {
                cleanup.push(format!("group readback: {error}"));
                break;
            }
        }
        if remaining.is_empty() {
            break;
        }
        if Instant::now() >= end {
            cleanup.push("group absence deadline".into());
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let mut pending_readers = Vec::new();
    for reader in readers {
        while !reader.is_finished() && Instant::now() < end {
            thread::sleep(Duration::from_millis(2));
        }
        if reader.is_finished() {
            match reader.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => cleanup.push(format!("reader: {error}")),
                Err(_) => cleanup.push("reader panicked".into()),
            }
        } else {
            cleanup.push("reader join deadline".into());
            pending_readers.push(reader);
        }
    }
    if let Inject::DelayCleanupObservation { delay, .. } = injection {
        thread::sleep(delay);
    }
    let cleanup_observed_at = Instant::now();
    if cleanup_observed_at >= end {
        cleanup.push("final cleanup observation deadline".into());
    }
    let mut receipt = Receipt {
        started,
        deadline,
        elapsed: started.elapsed(),
        terminal_observed_elapsed,
        cleanup_deadline: end,
        cleanup_observed_elapsed: cleanup_observed_at.duration_since(cleanup_started),
        status,
        primary,
        cleanup,
        group,
        remaining,
        saw_closed_streams_while_alive: closed_alive,
        pending_child: owned,
        pending_readers,
    };
    if let Err(error) = fs::write(
        root.join(format!("{label}.receipt")),
        format!(
            "{receipt:#?}\nowned_group={} identity_rows={:?} started={:?} deadline={:?} elapsed={:?} terminal_observed_elapsed={:?} cleanup_deadline={:?} cleanup_observed_elapsed={:?}\n",
            receipt.group,
            receipt
                .remaining
                .iter()
                .map(|member| (member.pid, member.state, member.start_ticks))
                .collect::<Vec<_>>(),
            receipt.started,
            receipt.deadline,
            receipt.elapsed,
            receipt.terminal_observed_elapsed,
            receipt.cleanup_deadline,
            receipt.cleanup_observed_elapsed,
        ),
    ) {
        receipt
            .cleanup
            .push(format!("receipt write failed; ownership retained: {error}"));
    }
    Ok(receipt)
}
fn require_clean(receipt: &Receipt) {
    assert!(receipt.cleanup.is_empty(), "cleanup failure: {receipt:#?}");
    assert!(
        receipt.remaining.is_empty()
            && receipt.pending_child.is_none()
            && receipt.pending_readers.is_empty(),
        "unresolved ownership: {receipt:#?}"
    );
}
fn require_success(receipt: &Receipt) {
    require_clean(receipt);
    assert!(
        receipt.primary.is_none() && receipt.status.is_some_and(|status| status.success()),
        "command failure: {receipt:#?}"
    );
}
fn rustc() -> Command {
    let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    // This workspace's pinned x86_64 toolchain uses bundled lld. Bound its
    // worker population as well as the unchanged child memory/FD limits.
    command.args(["-C", "link-arg=-Wl,--threads=2"]);
    command
}
fn create_owned_directory() -> io::Result<PathBuf> {
    let parent = std::env::temp_dir();
    for n in 0..1000 {
        let path = parent.join(format!(
            "hermit-unix-owner-controls-{}-{n}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other("no exclusive owner-control directory"))
}
#[test]
fn unix_guard_owner_controls() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = create_owned_directory().expect("exclusive evidence directory");
    eprintln!(
        "guard owner evidence retained until success: {}",
        root.display()
    );
    let adapter = fs::read(source.join("src/unix_guard_control.rs")).unwrap();
    let controls = fs::read(source.join("tests/unix_guard_owner/owner-controls.rs")).unwrap();
    let mut joined = adapter;
    joined.extend_from_slice(&controls);
    fs::write(root.join("copied-unix-guard-control.rs"), joined).unwrap();
    let api = source
        .parent()
        .unwrap()
        .join("detcore/src/network_runtime/guard.rs")
        .canonicalize()
        .unwrap();
    let harness = fs::read_to_string(source.join("tests/unix_guard_owner/harness.rs.in"))
        .unwrap()
        .replace("@GUARD_API@", api.to_str().unwrap());
    fs::write(root.join("harness.rs"), harness).unwrap();
    let binary = root.join("controls");
    require_success(
        &run(
            &root,
            "build",
            rustc()
                .args(["--edition=2024", "--test"])
                .arg(root.join("harness.rs"))
                .args(["-C", "debuginfo=0", "-o"])
                .arg(&binary),
            WALL,
            Inject::None,
        )
        .unwrap(),
    );
    require_success(
        &run(
            &root,
            "list",
            Command::new(&binary).arg("--list"),
            WALL,
            Inject::None,
        )
        .unwrap(),
    );
    let inventory = fs::read_to_string(root.join("list.stdout")).unwrap();
    assert_eq!(
        inventory
            .lines()
            .filter(|line| line.starts_with("actual::owner_controls::") && line.ends_with(": test"))
            .count(),
        10
    );
    require_success(
        &run(
            &root,
            "controls",
            Command::new(&binary).arg("--test-threads=1"),
            WALL,
            Inject::None,
        )
        .unwrap(),
    );
    let result = fs::read_to_string(root.join("controls.stdout")).unwrap();
    assert!(
        result.contains("test result: ok.")
            && result.contains("0 ignored")
            && result.contains("0 filtered out"),
        "{result}"
    );
    eprintln!("{result}");
    // A concrete close-stdio hang and a separate ordinary error after spawn
    // exercise the cleanup edges which the old selector-based runner missed.
    fs::write(root.join("hang.rs"),"unsafe extern \"C\" {fn close(fd:i32)->i32;} fn main(){if std::env::args().any(|arg|arg==\"--late-success\") {std::thread::sleep(std::time::Duration::from_millis(20));return;} unsafe {close(1);close(2);} loop {std::thread::park();}}\n").unwrap();
    let hang = root.join("hang");
    require_success(
        &run(
            &root,
            "build-hang",
            rustc()
                .arg(root.join("hang.rs"))
                .args(["--edition=2024", "-o"])
                .arg(&hang),
            WALL,
            Inject::None,
        )
        .unwrap(),
    );
    let hung = run(
        &root,
        "closed-stdio-hang",
        &mut Command::new(&hang),
        Duration::from_millis(500),
        Inject::None,
    )
    .unwrap();
    require_clean(&hung);
    assert_eq!(hung.primary.as_deref(), Some("child wall deadline"));
    assert!(hung.saw_closed_streams_while_alive);
    assert!(!hung.status.unwrap().success());
    let interrupted = run(
        &root,
        "error-after-spawn",
        &mut Command::new(&hang),
        WALL,
        Inject::ErrorAfterSpawn,
    )
    .unwrap();
    require_clean(&interrupted);
    assert_eq!(
        interrupted.primary.as_deref(),
        Some("injected error after owned spawn")
    );
    assert!(!interrupted.status.unwrap().success());
    let late = run(
        &root,
        "late-completion",
        Command::new(&hang).arg("--late-success"),
        Duration::from_millis(10),
        Inject::DelayFirstObservation(Duration::from_millis(50)),
    )
    .unwrap();
    require_clean(&late);
    assert_eq!(late.primary.as_deref(), Some("child wall deadline"));
    assert!(
        late.status.unwrap().success(),
        "the deliberately late child must actually finish"
    );
    assert!(
        late.terminal_observed_elapsed
            .is_some_and(|elapsed| elapsed >= Duration::from_millis(10))
    );
    // Delete only this invocation's ordinary temporary directory after every
    // receipt established group absence and no remaining owner/thread handle.
    let late_cleanup = run(
        &root,
        "late-cleanup-observation",
        &mut Command::new(&hang).arg("--late-success"),
        WALL,
        Inject::DelayCleanupObservation {
            budget: Duration::from_millis(10),
            delay: Duration::from_millis(50),
        },
    )
    .unwrap();
    assert!(late_cleanup.primary.is_none(), "{late_cleanup:#?}");
    assert!(late_cleanup.status.is_some_and(|status| status.success()));
    assert!(
        late_cleanup
            .cleanup
            .iter()
            .any(|error| error == "final cleanup observation deadline")
    );
    assert!(
        late_cleanup.remaining.is_empty()
            && late_cleanup.pending_child.is_none()
            && late_cleanup.pending_readers.is_empty()
    );
    assert!(late_cleanup.cleanup_observed_elapsed >= Duration::from_millis(10));
    if std::env::var_os("HERMIT_GUARD_KEEP_TEST_EVIDENCE").is_none() {
        fs::remove_dir_all(&root).unwrap();
    }
}
