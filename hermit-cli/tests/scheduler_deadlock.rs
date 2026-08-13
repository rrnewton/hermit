/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEADLOCK_HEADLINE: &str =
    "Deadlock detected: thread(s) waiting on futex, but no runnable threads left.";
const NATIVE_PASS: &str = "PASS: robust mutex waiter received EOWNERDEAD\n";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProcessIdentity {
    pid: libc::pid_t,
    starttime: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DescendantSnapshot {
    processes: BTreeMap<libc::pid_t, (ProcessIdentity, usize)>,
    tasks: BTreeMap<libc::pid_t, ProcessIdentity>,
}

impl DescendantSnapshot {
    fn empty() -> Self {
        Self {
            processes: BTreeMap::new(),
            tasks: BTreeMap::new(),
        }
    }
}

fn proc_stat_starttime(pid: libc::pid_t) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let comm_end = stat
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc stat comm"))?;
    let fields: Vec<&str> = stat[comm_end + 1..].split_whitespace().collect();
    fields
        .get(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc stat starttime"))?
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn process_identity(pid: libc::pid_t) -> io::Result<ProcessIdentity> {
    Ok(ProcessIdentity {
        pid,
        starttime: proc_stat_starttime(pid)?,
    })
}

fn same_process(identity: ProcessIdentity) -> bool {
    matches!(proc_stat_starttime(identity.pid), Ok(starttime) if starttime == identity.starttime)
}

fn numeric_directory_entries(path: &Path) -> io::Result<Vec<libc::pid_t>> {
    let mut ids = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && let Ok(id) = name.parse()
        {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn task_ids(pid: libc::pid_t) -> io::Result<Vec<libc::pid_t>> {
    numeric_directory_entries(Path::new(&format!("/proc/{pid}/task")))
}

fn task_children(pid: libc::pid_t, tid: libc::pid_t) -> io::Result<Vec<libc::pid_t>> {
    let contents = fs::read_to_string(format!("/proc/{pid}/task/{tid}/children"))?;
    let mut children = contents
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    children.sort_unstable();
    children.dedup();
    Ok(children)
}

fn token_processes(token_entry: &[u8]) -> io::Result<BTreeMap<libc::pid_t, ProcessIdentity>> {
    let mut matches = BTreeMap::new();
    for pid in numeric_directory_entries(Path::new("/proc"))? {
        let before = match process_identity(pid) {
            Ok(identity) => identity,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        let environ = match fs::read(format!("/proc/{pid}/environ")) {
            Ok(environ) => environ,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        if !environ
            .split(|byte| *byte == 0)
            .any(|entry| entry == token_entry)
        {
            continue;
        }
        if let Ok(after) = process_identity(pid)
            && after == before
        {
            matches.insert(pid, after);
        }
    }
    Ok(matches)
}

fn owned_processes(root: ProcessIdentity, token_entry: &[u8]) -> io::Result<DescendantSnapshot> {
    if !same_process(root) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "recorded Hermit root no longer exists",
        ));
    }

    let mut snapshot = DescendantSnapshot::empty();
    let mut pending = vec![(root.pid, 0usize)];
    let mut visited = BTreeMap::new();
    visited.insert(root.pid, root);

    for identity in token_processes(token_entry)?.into_values() {
        if identity == root {
            continue;
        }
        visited.insert(identity.pid, identity);
        snapshot.processes.insert(identity.pid, (identity, 1));
        pending.push((identity.pid, 1));
    }

    while let Some((pid, depth)) = pending.pop() {
        let tids = match task_ids(pid) {
            Ok(tids) => tids,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };

        if pid != root.pid {
            for tid in &tids {
                if let Ok(identity) = process_identity(*tid) {
                    snapshot.tasks.insert(*tid, identity);
                }
            }
        }

        for tid in tids {
            let children = match task_children(pid, tid) {
                Ok(children) => children,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for child in children {
                if visited.contains_key(&child) {
                    continue;
                }
                let identity = match process_identity(child) {
                    Ok(identity) => identity,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                visited.insert(child, identity);
                snapshot.processes.insert(child, (identity, depth + 1));
                pending.push((child, depth + 1));
            }
        }
    }

    Ok(snapshot)
}

fn direct_children(pid: libc::pid_t) -> io::Result<BTreeMap<libc::pid_t, ProcessIdentity>> {
    let mut children = BTreeMap::new();
    for tid in task_ids(pid)? {
        let task_children = match task_children(pid, tid) {
            Ok(children) => children,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for child in task_children {
            if let Ok(identity) = process_identity(child) {
                children.insert(child, identity);
            }
        }
    }
    Ok(children)
}

struct SubreaperGuard {
    previous: libc::c_int,
}

impl SubreaperGuard {
    fn install() -> io::Result<Self> {
        let mut previous = 0;
        let get_result = unsafe {
            libc::prctl(
                libc::PR_GET_CHILD_SUBREAPER,
                &mut previous as *mut libc::c_int,
                0,
                0,
                0,
            )
        };
        if get_result != 0 {
            return Err(io::Error::last_os_error());
        }
        if previous == 0 {
            let set_result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
            if set_result != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { previous })
    }
}

impl Drop for SubreaperGuard {
    fn drop(&mut self) {
        if self.previous == 0 {
            let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 0, 0, 0, 0) };
            if result != 0 {
                eprintln!(
                    "failed to restore child-subreaper state: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }
}

struct OwnedHermit {
    child: Child,
    root: ProcessIdentity,
    token_entry: Vec<u8>,
    preexisting_children: BTreeMap<libc::pid_t, ProcessIdentity>,
    descendants: BTreeMap<libc::pid_t, (ProcessIdentity, usize)>,
    armed: bool,
}

impl OwnedHermit {
    fn new(
        child: Child,
        root: ProcessIdentity,
        token_entry: Vec<u8>,
        preexisting_children: BTreeMap<libc::pid_t, ProcessIdentity>,
    ) -> Self {
        Self {
            child,
            root,
            token_entry,
            preexisting_children,
            descendants: BTreeMap::new(),
            armed: true,
        }
    }

    fn record(&mut self, snapshot: &DescendantSnapshot) {
        self.descendants.extend(snapshot.processes.clone());
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn reap_recorded_descendants(&self) {
        for (identity, _) in self.descendants.values() {
            let mut status = 0;
            unsafe {
                libc::waitpid(identity.pid, &mut status, libc::WNOHANG);
            }
        }
    }

    fn cleanup(&mut self) {
        if !self.armed {
            return;
        }

        if same_process(self.root)
            && let Ok(snapshot) = owned_processes(self.root, &self.token_entry)
        {
            self.record(&snapshot);
        }

        let self_pid = unsafe { libc::getpid() };
        if let Ok(adopted) = direct_children(self_pid) {
            for identity in adopted.into_values() {
                if identity != self.root
                    && self.preexisting_children.get(&identity.pid) != Some(&identity)
                {
                    self.descendants
                        .entry(identity.pid)
                        .or_insert((identity, 1));
                }
            }
        }

        let mut descendants: Vec<(ProcessIdentity, usize)> =
            self.descendants.values().copied().collect();
        descendants.sort_by_key(|(identity, depth)| (std::cmp::Reverse(*depth), identity.pid));
        for (identity, _) in descendants {
            signal_exact(identity, libc::SIGKILL);
        }
        signal_exact(self.root, libc::SIGKILL);
        let _ = self.child.wait();

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            self.reap_recorded_descendants();
            if self
                .descendants
                .values()
                .all(|(identity, _)| !same_process(*identity))
            {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OwnedHermit {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn signal_exact(identity: ProcessIdentity, signal: libc::c_int) {
    match proc_stat_starttime(identity.pid) {
        Ok(starttime) if starttime == identity.starttime => {
            let result = unsafe { libc::kill(identity.pid, signal) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    eprintln!(
                        "failed to signal recorded pid {} starttime {}: {error}",
                        identity.pid, identity.starttime
                    );
                }
            }
        }
        Ok(starttime) => eprintln!(
            "refusing to signal reused pid {}: recorded starttime {}, observed {}",
            identity.pid, identity.starttime, starttime
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!(
            "refusing to signal pid {} because its identity is unreadable: {error}",
            identity.pid
        ),
    }
}

fn read_ready_line(
    stdout: &mut ChildStdout,
    owned: &mut OwnedHermit,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        if line.ends_with(b"\n") {
            return Ok(line);
        }
        if let Some(status) = owned.try_wait()? {
            return Err(io::Error::other(format!(
                "Hermit exited before the guest was held: {status}"
            )));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Hermit did not reach the held guest state before the 10-second total bound",
            ));
        }
        let timeout = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(50));
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut pollfd = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let poll_result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if poll_result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll_result == 0 {
            continue;
        }
        let mut bytes = [0u8; 64];
        let count = stdout.read(&mut bytes)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "guest stdout closed before ready",
            ));
        }
        line.extend_from_slice(&bytes[..count]);
        if line.len() > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "guest wrote more than one short readiness line",
            ));
        }
    }
}

fn stable_descendants(
    root: ProcessIdentity,
    token_entry: &[u8],
    deadline: Instant,
) -> DescendantSnapshot {
    let mut previous: Option<DescendantSnapshot> = None;
    loop {
        let current = owned_processes(root, token_entry).unwrap_or_else(|error| {
            panic!("failed to enumerate the Hermit tracee subtree recursively: {error}")
        });
        if !current.processes.is_empty()
            && current.tasks.len() >= 3
            && previous.as_ref() == Some(&current)
        {
            return current;
        }
        if Instant::now() >= deadline {
            let previous_processes = previous.as_ref().map_or(0, |value| value.processes.len());
            let previous_tasks = previous.as_ref().map_or(0, |value| value.tasks.len());
            panic!(
                "tracee discovery did not stabilize before the 10-second total bound: \
                 current processes={}/tasks={} {:?}; previous processes={}/tasks={} {:?}",
                current.processes.len(),
                current.tasks.len(),
                current.processes.keys().collect::<Vec<_>>(),
                previous_processes,
                previous_tasks,
                previous
                    .as_ref()
                    .map(|value| value.processes.keys().collect::<Vec<_>>())
                    .unwrap_or_default()
            );
        }
        previous = Some(current);
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_exit(owned: &mut OwnedHermit, deadline: Instant) -> ExitStatus {
    loop {
        match owned
            .try_wait()
            .expect("failed to poll the recorded Hermit child")
        {
            Some(status) => return status,
            None if Instant::now() >= deadline => {
                panic!("scheduler deadlock diagnostic exceeded the 10-second total bound")
            }
            None => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

fn wait_for_teardown(
    owned: &mut OwnedHermit,
    snapshot: &DescendantSnapshot,
    deadline: Instant,
) -> (usize, usize, usize) {
    owned.record(snapshot);
    let self_pid = unsafe { libc::getpid() };
    loop {
        owned.reap_recorded_descendants();
        let live_processes = snapshot
            .processes
            .values()
            .filter(|(identity, _)| same_process(*identity))
            .count();
        let live_tasks = snapshot
            .tasks
            .values()
            .filter(|identity| same_process(**identity))
            .count();
        let adopted: BTreeMap<_, _> = direct_children(self_pid)
            .expect("failed to enumerate children adopted by the test subreaper");
        let adopted: BTreeMap<_, _> = adopted
            .into_iter()
            .filter(|entry| {
                let (pid, identity) = entry;
                *identity != owned.root && owned.preexisting_children.get(pid) != Some(identity)
            })
            .collect();
        for identity in adopted.values() {
            if *identity != owned.root {
                owned
                    .descendants
                    .entry(identity.pid)
                    .or_insert((*identity, 1));
            }
        }
        if live_processes == 0 && live_tasks == 0 && adopted.is_empty() {
            return (0, 0, 0);
        }
        if Instant::now() >= deadline {
            return (live_processes, live_tasks, adopted.len());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_root_identity(pid: libc::pid_t, deadline: Instant) -> ProcessIdentity {
    loop {
        match process_identity(pid) {
            Ok(identity) => return identity,
            Err(error) if error.kind() == io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("failed to bind Hermit pid {pid} to its starttime: {error}"),
        }
    }
}

#[test]
fn terminal_scheduler_deadlock_reports_and_tears_down_tracees() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scheduler-deadlock");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("robust_futex_test");

    let compile = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/bin/robust_futex_test.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile robust futex guest");
    assert!(
        compile.status.success(),
        "robust futex guest compilation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );

    let native = Command::new(&guest)
        .output()
        .expect("failed to run native robust futex control");
    assert_eq!(native.status.code(), Some(0), "native control must exit 0");
    assert_eq!(String::from_utf8_lossy(&native.stdout), NATIVE_PASS);
    assert!(
        native.stderr.is_empty(),
        "native control wrote stderr:\n{}",
        String::from_utf8_lossy(&native.stderr)
    );

    let _subreaper =
        SubreaperGuard::install().expect("failed to make the dedicated test process a subreaper");
    let self_pid = unsafe { libc::getpid() };
    let preexisting_children =
        direct_children(self_pid).expect("failed to record pre-spawn child identities");
    let token = format!(
        "{}-{}",
        self_pid,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock predates the Unix epoch")
            .as_nanos()
    );
    let token_entry = format!("HERMIT_SCHEDULER_DEADLOCK_TEST_TOKEN={token}").into_bytes();
    let mut stderr = tempfile::tempfile().expect("failed to create Hermit stderr capture");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .env("HERMIT_SCHEDULER_DEADLOCK_TEST_TOKEN", &token)
        .args(["run", "--backend=ptrace", "--strict", "--"])
        .arg(&guest)
        .arg("--wait-before-owner-exit")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .expect("failed to clone Hermit stderr capture"),
        ));

    let started = Instant::now();
    let deadline = started + TOTAL_TIMEOUT;
    let mut child = command.spawn().expect("failed to start Hermit");
    let root = wait_for_root_identity(child.id() as libc::pid_t, deadline);
    let mut stdin = child.stdin.take().expect("Hermit stdin was not piped");
    let mut stdout = child.stdout.take().expect("Hermit stdout was not piped");
    let mut owned = OwnedHermit::new(child, root, token_entry, preexisting_children);

    let ready = read_ready_line(&mut stdout, &mut owned, deadline)
        .expect("failed while waiting for the held robust-futex guest");
    assert_eq!(ready, b"ready\n", "unexpected guest readiness output");

    let snapshot = stable_descendants(root, &owned.token_entry, deadline);
    let process_count = snapshot.processes.len();
    let root_process_count = snapshot
        .processes
        .values()
        .filter(|(_, depth)| *depth == 1)
        .count();
    let descendant_process_count = process_count - root_process_count;
    let task_count = snapshot.tasks.len();
    assert!(
        process_count >= 1,
        "tracee process denominator must be nonzero, observed {process_count}"
    );
    assert!(
        root_process_count >= 1,
        "tracee root denominator must be nonzero, observed {root_process_count}"
    );
    assert!(
        task_count >= 3,
        "tracee task denominator must include the live threaded guest, observed {task_count}"
    );
    owned.record(&snapshot);

    let release_started = Instant::now();
    stdin
        .write_all(b"x")
        .expect("failed to release robust-futex owner thread");
    drop(stdin);

    let status = wait_for_exit(&mut owned, deadline);
    let release_elapsed = release_started.elapsed();
    let (live_processes, live_tasks, adopted_children) =
        wait_for_teardown(&mut owned, &snapshot, deadline);

    assert_eq!(
        status.code(),
        Some(1),
        "terminal scheduler deadlock must exit exactly 1, not timeout/signal/success: {status}"
    );
    assert!(
        started.elapsed() < TOTAL_TIMEOUT,
        "terminal scheduler deadlock exceeded the 10-second total bound"
    );
    assert_eq!(
        live_processes, 0,
        "recursive tracee process teardown left {live_processes}/{process_count} recorded process identities alive"
    );
    assert_eq!(
        live_tasks, 0,
        "recursive tracee task teardown left {live_tasks}/{task_count} recorded task identities alive"
    );
    assert_eq!(
        adopted_children, 0,
        "test subreaper retained {adopted_children} child process roots after teardown"
    );

    owned.disarm();
    let mut remaining_stdout = Vec::new();
    stdout
        .read_to_end(&mut remaining_stdout)
        .expect("failed to read remaining guest stdout after teardown");
    let mut all_stdout = ready;
    all_stdout.extend_from_slice(&remaining_stdout);

    stderr
        .seek(SeekFrom::Start(0))
        .expect("failed to rewind Hermit stderr capture");
    let mut stderr_bytes = Vec::new();
    stderr
        .read_to_end(&mut stderr_bytes)
        .expect("failed to read Hermit stderr capture");
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    assert!(
        stderr_text.lines().any(|line| line == DEADLOCK_HEADLINE),
        "missing exact scheduler deadlock headline\nstderr:\n{stderr_text}"
    );
    assert!(
        stderr_text
            .lines()
            .any(|line| line == "  run queue: 0 runnable"),
        "missing empty run-queue evidence\nstderr:\n{stderr_text}"
    );
    assert!(
        stderr_text
            .lines()
            .any(|line| line.starts_with("  futex waiters (") && line.ends_with("), by futex:")),
        "missing futex-waiter population evidence\nstderr:\n{stderr_text}"
    );
    assert!(
        !String::from_utf8_lossy(&all_stdout).contains(NATIVE_PASS.trim()),
        "Hermit unexpectedly took the native owner-death wake path"
    );

    eprintln!(
        "scheduler deadlock rc=1 release_to_exit={:.3}s total={:.3}s; tracee roots 0/{root_process_count}; nested process descendants 0/{descendant_process_count}; all tracee processes 0/{process_count}; tracee tasks 0/{task_count}; adopted children 0/0",
        release_elapsed.as_secs_f64(),
        started.elapsed().as_secs_f64()
    );
}
