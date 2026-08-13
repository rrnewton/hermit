/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DescendantSnapshot {
    processes: BTreeMap<libc::pid_t, (ProcessIdentity, usize)>,
    tasks: BTreeMap<libc::pid_t, ProcessIdentity>,
}

fn proc_stat_parent_and_starttime(pid: libc::pid_t) -> io::Result<(libc::pid_t, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let comm_end = stat
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc stat comm"))?;
    let fields: Vec<&str> = stat[comm_end + 1..].split_whitespace().collect();
    let parent = fields
        .get(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc stat parent"))?
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let starttime = fields
        .get(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc stat starttime"))?
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((parent, starttime))
}

fn proc_stat_starttime(pid: libc::pid_t) -> io::Result<u64> {
    proc_stat_parent_and_starttime(pid).map(|(_, starttime)| starttime)
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

fn owned_processes(root: ProcessIdentity) -> io::Result<DescendantSnapshot> {
    if !same_process(root) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "recorded Hermit root no longer exists",
        ));
    }

    let mut snapshot = DescendantSnapshot::default();
    let mut pending = vec![(root.pid, 0usize)];
    let mut visited = BTreeMap::new();
    visited.insert(root.pid, root);

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

        for child in numeric_directory_entries(Path::new("/proc"))? {
            if visited.contains_key(&child) {
                continue;
            }
            let (parent, starttime) = match proc_stat_parent_and_starttime(child) {
                Ok(values) => values,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => continue,
            };
            if parent != pid {
                continue;
            }
            let identity = ProcessIdentity {
                pid: child,
                starttime,
            };
            let after = match proc_stat_parent_and_starttime(child) {
                Ok(values) => values,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => continue,
            };
            if after != (pid, starttime) || !same_process(visited[&pid]) {
                continue;
            }
            visited.insert(child, identity);
            snapshot.processes.insert(child, (identity, depth + 1));
            pending.push((child, depth + 1));
        }
    }

    Ok(snapshot)
}

fn direct_children(pid: libc::pid_t) -> io::Result<BTreeMap<libc::pid_t, ProcessIdentity>> {
    let mut children = BTreeMap::new();
    for child in numeric_directory_entries(Path::new("/proc"))? {
        if let Ok((parent, starttime)) = proc_stat_parent_and_starttime(child)
            && parent == pid
        {
            children.insert(
                child,
                ProcessIdentity {
                    pid: child,
                    starttime,
                },
            );
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

struct OwnedProcess {
    identity: ProcessIdentity,
    depth: usize,
    pidfd: OwnedFd,
}

impl OwnedProcess {
    fn open(identity: ProcessIdentity, depth: usize) -> io::Result<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
        if !same_process(identity) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "pid {} changed identity while its pidfd was opened",
                    identity.pid
                ),
            ));
        }
        Ok(Self {
            identity,
            depth,
            pidfd,
        })
    }
}

struct OwnedHermit {
    child: Child,
    root: OwnedProcess,
    preexisting_children: BTreeMap<libc::pid_t, ProcessIdentity>,
    descendants: BTreeMap<libc::pid_t, OwnedProcess>,
    armed: bool,
}

impl OwnedHermit {
    fn new(
        mut child: Child,
        preexisting_children: BTreeMap<libc::pid_t, ProcessIdentity>,
    ) -> io::Result<Self> {
        let pid = child.id() as libc::pid_t;
        let identity = match process_identity(pid) {
            Ok(identity) => identity,
            Err(error) => {
                terminate_direct_child(&mut child, Duration::from_secs(1), "unbound Hermit child");
                return Err(error);
            }
        };
        let root = match OwnedProcess::open(identity, 0) {
            Ok(root) => root,
            Err(error) => {
                terminate_direct_child(
                    &mut child,
                    Duration::from_secs(1),
                    "Hermit child without a pidfd",
                );
                return Err(error);
            }
        };
        Ok(Self {
            child,
            root,
            preexisting_children,
            descendants: BTreeMap::new(),
            armed: true,
        })
    }

    fn record(&mut self, snapshot: &DescendantSnapshot) -> io::Result<()> {
        let mut processes: Vec<_> = snapshot.processes.values().copied().collect();
        processes.sort_by_key(|(identity, depth)| (*depth, identity.pid));
        for (identity, depth) in processes {
            if !self.descendants.contains_key(&identity.pid) {
                let process = OwnedProcess::open(identity, depth)?;
                let (parent_pid, starttime) = proc_stat_parent_and_starttime(identity.pid)?;
                let parent = if depth == 1 {
                    &self.root
                } else {
                    self.descendants.get(&parent_pid).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "recorded pid {} has unrecorded parent {parent_pid}",
                                identity.pid
                            ),
                        )
                    })?
                };
                if parent.identity.pid != parent_pid
                    || parent.depth + 1 != depth
                    || starttime != identity.starttime
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "recorded pid {} no longer has the observed parent/depth",
                            identity.pid
                        ),
                    ));
                }
                self.descendants.insert(identity.pid, process);
            }
        }
        Ok(())
    }

    fn record_direct_child(
        &mut self,
        identity: ProcessIdentity,
        parent_pid: libc::pid_t,
    ) -> io::Result<()> {
        if let Entry::Vacant(entry) = self.descendants.entry(identity.pid) {
            let process = OwnedProcess::open(identity, 1)?;
            let (observed_parent, starttime) = proc_stat_parent_and_starttime(identity.pid)?;
            if observed_parent != parent_pid || starttime != identity.starttime {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "pid {} is no longer a direct child of {parent_pid}",
                        identity.pid
                    ),
                ));
            }
            entry.insert(process);
        }
        Ok(())
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn reap_recorded_descendants(&self) {
        for process in self.descendants.values() {
            let mut status = 0;
            unsafe {
                libc::waitpid(process.identity.pid, &mut status, libc::WNOHANG);
            }
        }
    }

    fn cleanup(&mut self) {
        if !self.armed {
            return;
        }

        let deadline = Instant::now() + Duration::from_secs(1);

        if same_process(self.root.identity)
            && let Ok(snapshot) = owned_processes(self.root.identity)
        {
            let _ = self.record(&snapshot);
        }

        let mut descendants: Vec<&OwnedProcess> = self.descendants.values().collect();
        descendants.sort_by_key(|process| (std::cmp::Reverse(process.depth), process.identity.pid));
        for process in descendants {
            if let Err(error) = signal_pidfd(process, libc::SIGKILL) {
                eprintln!(
                    "cleanup failed to signal recorded pid {} through its pidfd: {error}",
                    process.identity.pid
                );
            }
        }
        if let Err(error) = signal_pidfd(&self.root, libc::SIGKILL) {
            eprintln!(
                "cleanup failed to signal Hermit pid {} through its pidfd: {error}",
                self.root.identity.pid
            );
        }

        let self_pid = unsafe { libc::getpid() };
        loop {
            let _ = self.child.try_wait();
            let mut adopted_count = 0;
            if let Ok(adopted) = direct_children(self_pid) {
                for identity in adopted.into_values() {
                    if identity == self.root.identity
                        || self.preexisting_children.get(&identity.pid) == Some(&identity)
                    {
                        continue;
                    }
                    adopted_count += 1;
                    if let Err(error) = self.record_direct_child(identity, self_pid) {
                        eprintln!(
                            "cleanup could not bind adopted pid {} to its pidfd: {error}",
                            identity.pid
                        );
                    }
                }
            }

            for process in self.descendants.values() {
                if same_process(process.identity)
                    && let Err(error) = signal_pidfd(process, libc::SIGKILL)
                {
                    eprintln!(
                        "cleanup failed to signal recorded pid {} through its pidfd: {error}",
                        process.identity.pid
                    );
                }
            }
            self.reap_recorded_descendants();
            let live_descendants = self
                .descendants
                .values()
                .filter(|process| same_process(process.identity))
                .count();
            let root_alive = same_process(self.root.identity);
            if !root_alive && live_descendants == 0 && adopted_count == 0 {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "ERROR: cleanup deadline expired with Hermit root alive={root_alive}, \
                     {live_descendants} recorded descendant process(es) alive, and \
                     {adopted_count} adopted child process root(s) observed"
                );
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

fn signal_pidfd(process: &OwnedProcess, signal: libc::c_int) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            process.pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

fn terminate_direct_child(child: &mut Child, bound: Duration, description: &str) {
    // An exact unreaped direct child cannot have a reused PID.
    if let Err(error) = child.kill()
        && error.raw_os_error() != Some(libc::ESRCH)
    {
        eprintln!("failed to signal {description}: {error}");
    }
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                eprintln!(
                    "ERROR: {description} did not exit within the {}-second cleanup bound",
                    bound.as_secs()
                );
                return;
            }
            Err(error) => {
                eprintln!("ERROR: failed to reap {description}: {error}");
                return;
            }
        }
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

fn stable_descendants(root: ProcessIdentity, deadline: Instant) -> DescendantSnapshot {
    let mut previous: Option<DescendantSnapshot> = None;
    loop {
        let current = owned_processes(root).unwrap_or_else(|error| {
            panic!("failed to enumerate the Hermit tracee subtree recursively: {error}")
        });
        if current.processes.values().any(|(_, depth)| *depth > 1)
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
    owned
        .record(snapshot)
        .expect("failed to bind recorded descendants to pidfds");
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
                *identity != owned.root.identity
                    && owned.preexisting_children.get(pid) != Some(identity)
            })
            .collect();
        for identity in adopted.values() {
            if *identity != owned.root.identity {
                owned
                    .record_direct_child(*identity, self_pid)
                    .expect("failed to bind adopted child to a pidfd");
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

fn bounded_output(command: &mut Command, bound: Duration, description: &str) -> io::Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    let mut child = command.spawn()?;
    let deadline = Instant::now() + bound;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_direct_child(
                &mut child,
                Duration::from_secs(1),
                &format!("timed-out {description}"),
            );
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{description} exceeded the {}-second bound",
                    bound.as_secs()
                ),
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout.read_to_end(&mut stdout_bytes)?;
    stderr.read_to_end(&mut stderr_bytes)?;
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

fn read_to_end_bounded(stdout: &mut ChildStdout, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "guest stdout did not close before the 10-second total bound",
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
        let mut chunk = [0u8; 256];
        let count = stdout.read(&mut chunk)?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..count]);
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

    let mut compiler = Command::new("cc");
    compiler
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/bin/robust_futex_test.c"))
        .arg("-o")
        .arg(&guest);
    let compile = bounded_output(
        &mut compiler,
        TOTAL_TIMEOUT,
        "robust-futex guest compilation",
    )
    .expect("failed to compile robust futex guest");
    assert!(
        compile.status.success(),
        "robust futex guest compilation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );

    let native = bounded_output(
        &mut Command::new(&guest),
        TOTAL_TIMEOUT,
        "native robust-futex control",
    )
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
    let mut stderr = tempfile::tempfile().expect("failed to create Hermit stderr capture");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
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
    let child = command.spawn().expect("failed to start Hermit");
    let mut owned = OwnedHermit::new(child, preexisting_children)
        .expect("failed to bind the Hermit child to a pidfd");
    let mut stdin = owned
        .child
        .stdin
        .take()
        .expect("Hermit stdin was not piped");
    let mut stdout = owned
        .child
        .stdout
        .take()
        .expect("Hermit stdout was not piped");

    let ready = read_ready_line(&mut stdout, &mut owned, deadline)
        .expect("failed while waiting for the held robust-futex guest");
    assert_eq!(ready, b"ready\n", "unexpected guest readiness output");

    let snapshot = stable_descendants(owned.root.identity, deadline);
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
        descendant_process_count >= 1,
        "recursive tracee process denominator must be nonzero, observed {descendant_process_count}"
    );
    assert!(
        task_count >= 3,
        "tracee task denominator must include the live threaded guest, observed {task_count}"
    );
    owned
        .record(&snapshot)
        .expect("failed to bind the held tracee subtree to pidfds");

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

    let remaining_stdout = read_to_end_bounded(&mut stdout, deadline)
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

    owned.disarm();

    eprintln!(
        "scheduler deadlock rc=1 release_to_exit={:.3}s total={:.3}s; tracee roots 0/{root_process_count}; nested process descendants 0/{descendant_process_count}; all tracee processes 0/{process_count}; tracee tasks 0/{task_count}; 0 adopted child process roots at the post-exit observation",
        release_elapsed.as_secs_f64(),
        started.elapsed().as_secs_f64()
    );
}
