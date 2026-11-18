/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Host-fork timelines: copies of a run taken at a scheduler turn (PROTOTYPE).
//!
//! With [`TIMELINE_FORK_ENV`] set to `turn=N,copies=K,dir=DIR`, the scheduler
//! stops at its first drain after committing turn N, where every guest thread
//! is parked (the point `--signal-at` uses), and the run forks K copies of
//! itself, one at a time. Each copy (a *timeline*) repairs what `fork()` does
//! not copy, through the hooks its backend installed
//! ([`install_backend_hooks`]), and then runs the rest of the guest as the
//! original would have. The parent (the *template*) waits for each copy in
//! turn and then continues its own run.
//!
//! Each timeline's standard output and standard error go to
//! `DIR/timeline-<k>.stdout` and `.stderr`; it starts with its own copy of
//! every regular file's and directory's position. `DIR/template.json` records
//! the template's output offsets at the fork, so a check can compare the
//! template's prefix followed by a timeline's suffix with a fresh run.
//!
//! None of this writes to the run's INFO log: what it measures goes to files
//! in DIR, so a timeline's log can be compared with a fresh run's.

use std::io::Write;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// The environment variable that asks for timelines.
pub const TIMELINE_FORK_ENV: &str = "HERMIT_TIMELINE_FORK_AT";

/// Where and how many copies to take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkSpec {
    /// The first committed turn at or after which the copies are taken.
    pub turn: u64,
    /// How many copies, run one after another.
    pub copies: usize,
    /// Where each copy's output and measurements go.
    pub dir: PathBuf,
}

impl ForkSpec {
    /// Parses `turn=N,copies=K,dir=DIR`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut turn = None;
        let mut copies = 1;
        let mut dir = None;
        for part in text.split(',') {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| format!("{TIMELINE_FORK_ENV}: {part:?} is not key=value"))?;
            match key {
                "turn" => turn = Some(value.parse().map_err(|e| format!("turn: {e}"))?),
                "copies" => copies = value.parse().map_err(|e| format!("copies: {e}"))?,
                "dir" => dir = Some(PathBuf::from(value)),
                other => return Err(format!("{TIMELINE_FORK_ENV}: unknown key {other:?}")),
            }
        }
        Ok(Self {
            turn: turn.ok_or_else(|| format!("{TIMELINE_FORK_ENV}: turn= is required"))?,
            copies,
            dir: dir.ok_or_else(|| format!("{TIMELINE_FORK_ENV}: dir= is required"))?,
        })
    }
}

/// The spec in [`TIMELINE_FORK_ENV`], if set and valid; an invalid one is
/// reported and ignored.
pub fn spec_from_env() -> Option<ForkSpec> {
    let text = std::env::var(TIMELINE_FORK_ENV).ok()?;
    match ForkSpec::parse(&text) {
        Ok(spec) => Some(spec),
        Err(error) => {
            tracing::warn!("{error}");
            None
        }
    }
}

/// What a backend does around the fork. Each returns a one-line description
/// of what it did, for the measurements.
#[derive(Clone, Copy)]
pub struct BackendHooks {
    /// The backend's name.
    pub name: &'static str,
    /// In the template, before the first fork: read what the copies need.
    pub prepare: fn() -> Result<String, String>,
    /// In each copy, before anything else: rebuild what fork did not copy.
    pub repair: fn() -> Result<String, String>,
}

static HOOKS: OnceLock<BackendHooks> = OnceLock::new();

/// Installs the running backend's hooks, once per process.
pub fn install_backend_hooks(hooks: BackendHooks) {
    let _ = HOOKS.set(hooks);
}

/// Descriptors, besides standard output and standard error, that each
/// timeline writes to a file of its own (`DIR/timeline-<k>.<name>`).
static OUTPUTS: std::sync::Mutex<Vec<(RawFd, &'static str)>> = std::sync::Mutex::new(Vec::new());

/// Gives each timeline its own copy of what the run writes to `fd` (the
/// run's `--log-file`, say), named `name`; the template keeps the original.
pub fn register_output(fd: RawFd, name: &'static str) {
    OUTPUTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((fd, name));
}

fn outputs() -> Vec<(RawFd, &'static str)> {
    let mut outputs = vec![(1, "stdout"), (2, "stderr")];
    outputs.extend(
        OUTPUTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied(),
    );
    outputs
}

static TIMELINE: AtomicI64 = AtomicI64::new(-1);
static TIMELINE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// This process's timeline number, when it is a copy.
pub fn timeline_index() -> Option<usize> {
    let index = TIMELINE.load(Ordering::Relaxed);
    (index >= 0).then_some(index as usize)
}

/// Ends a timeline's process with `code` once its run is over, recording the
/// code, without running the destructors of what it shares with the
/// template (its temporary files, its CPU lease). Returns in the template.
pub fn exit_timeline(code: i32) {
    let Some(index) = timeline_index() else {
        return;
    };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    if let Some(dir) = TIMELINE_DIR.get() {
        let _ = std::fs::write(
            dir.join(format!("timeline-{index}.exit")),
            format!("{code}\n"),
        );
    }
    // SAFETY: _exit ends the process at once; nothing runs after it.
    unsafe { libc::_exit(code) };
}

/// Whether two descriptors of this process name one open file description.
fn same_description(a: RawFd, b: RawFd) -> bool {
    const KCMP_FILE: libc::c_long = 0;
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() } as libc::c_long;
    // SAFETY: kcmp compares two descriptors of this process; no memory.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid,
            pid,
            KCMP_FILE,
            a as libc::c_long,
            b as libc::c_long,
        )
    };
    rc == 0
}

fn open_fds() -> Vec<RawFd> {
    let mut fds: Vec<RawFd> = std::fs::read_dir("/proc/self/fd")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_string_lossy().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    fds.sort_unstable();
    fds
}

/// A regular file or directory descriptor whose position each timeline
/// gets its own copy of.
struct FileRecord {
    fd: RawFd,
    status: libc::c_int,
    offset: libc::off_t,
}

fn is_output(fd: RawFd) -> bool {
    outputs()
        .iter()
        .any(|&(output, _)| fd == output || same_description(fd, output))
}

fn record_files() -> Vec<FileRecord> {
    let mut records = Vec::new();
    for fd in open_fds() {
        if fd <= 2 || is_output(fd) {
            continue;
        }
        let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{fd}")) else {
            continue;
        };
        if target.to_string_lossy().starts_with("/memfd:") {
            continue;
        }
        // SAFETY: fstat of a descriptor into a zeroed buffer.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::fstat(fd, &mut stat) } < 0 {
            continue;
        }
        let kind = stat.st_mode & libc::S_IFMT;
        if kind != libc::S_IFREG && kind != libc::S_IFDIR {
            continue;
        }
        // SAFETY: fcntl and lseek on a live descriptor.
        let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: as above.
        let offset = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
        if status < 0 || offset < 0 {
            continue;
        }
        records.push(FileRecord { fd, status, offset });
    }
    records
}

fn dup_onto(from: RawFd, onto: RawFd) -> bool {
    // SAFETY: fcntl on a live descriptor.
    let flags = unsafe { libc::fcntl(onto, libc::F_GETFD) };
    let cloexec = if flags >= 0 && flags & libc::FD_CLOEXEC != 0 {
        libc::O_CLOEXEC
    } else {
        0
    };
    // SAFETY: both are descriptors of this process; dup3 replaces `onto`.
    unsafe { libc::dup3(from, onto, cloexec) >= 0 }
}

/// Gives this timeline its own open file description for each recorded
/// file, at the template's position, with its status flags.
fn reopen_files(records: &[FileRecord]) -> usize {
    let mut reopened = 0;
    for record in records {
        let access = record.status & libc::O_ACCMODE;
        let flags = access
            | (record.status & (libc::O_APPEND | libc::O_NONBLOCK | libc::O_DIRECTORY))
            | libc::O_CLOEXEC;
        let path = format!("/proc/self/fd/{}\0", record.fd);
        // SAFETY: a NUL-terminated path; a new descriptor.
        let fd = unsafe { libc::open(path.as_ptr().cast(), flags) };
        if fd < 0 {
            continue;
        }
        // SAFETY: lseek on the new descriptor.
        unsafe { libc::lseek(fd, record.offset, libc::SEEK_SET) };
        if dup_onto(fd, record.fd) {
            reopened += 1;
        }
        // SAFETY: the duplicate lives on behind record.fd.
        unsafe { libc::close(fd) };
    }
    reopened
}

/// Sends every descriptor that shares one of the run's outputs (standard
/// output, standard error, each registered output) to this timeline's own
/// file for it. A descriptor goes with the first output it shares.
fn redirect_outputs(dir: &std::path::Path, index: usize) -> std::io::Result<()> {
    let fds = open_fds();
    let mut claimed: Vec<RawFd> = Vec::new();
    let mut groups = Vec::new();
    for (output, name) in outputs() {
        let targets: Vec<RawFd> = fds
            .iter()
            .copied()
            .filter(|&fd| !claimed.contains(&fd) && (fd == output || same_description(fd, output)))
            .collect();
        claimed.extend(&targets);
        groups.push((name, targets));
    }
    for (name, targets) in groups {
        if targets.is_empty() {
            continue;
        }
        let file = std::fs::File::create(dir.join(format!("timeline-{index}.{name}")))?;
        let raw = std::os::fd::AsRawFd::as_raw_fd(&file);
        for fd in targets {
            if !dup_onto(raw, fd) {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn offset_of(fd: RawFd) -> i64 {
    // SAFETY: lseek on a descriptor; -1 for a pipe or terminal.
    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }
}

fn micros(since: Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

/// Takes the copies `spec` asks for, at committed turn `turn`. Returns in the
/// template once every copy has exited, and in each copy once it is
/// repaired, so that the caller's scheduler goes on with the run.
pub(crate) fn fork_copies(spec: &ForkSpec, turn: u64) {
    let Some(hooks) = HOOKS.get().copied() else {
        tracing::warn!("{TIMELINE_FORK_ENV}: this backend installed no host-fork hooks");
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&spec.dir) {
        tracing::warn!("{TIMELINE_FORK_ENV}: {}: {error}", spec.dir.display());
        return;
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    let prepare_started = Instant::now();
    let prepared = (hooks.prepare)();
    let prepare_us = micros(prepare_started);
    let prepared = match prepared {
        Ok(description) => description,
        Err(error) => {
            let _ = std::fs::write(spec.dir.join("refused.txt"), format!("{error}\n"));
            tracing::warn!("{TIMELINE_FORK_ENV}: capture refused: {error}");
            return;
        }
    };
    let records_started = Instant::now();
    let records = record_files();
    let records_us = micros(records_started);
    let offsets: serde_json::Map<String, serde_json::Value> = outputs()
        .into_iter()
        .map(|(fd, name)| (format!("{name}_offset"), offset_of(fd).into()))
        .collect();
    let mut children = Vec::new();
    for index in 0..spec.copies {
        let forked = Instant::now();
        // SAFETY: every guest thread is parked and the scheduler runs on the
        // run's only supervisor thread (the backend's prepare hook refused
        // otherwise), so the copy has every thread the run needs.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            become_timeline(spec, index, turn, &hooks, &records, forked);
            return;
        }
        let fork_us = micros(forked);
        if pid < 0 {
            let error = std::io::Error::last_os_error();
            children.push(serde_json::json!({ "index": index, "error": error.to_string() }));
            break;
        }
        let mut status = 0;
        let waited = loop {
            // SAFETY: waits for the copy just forked.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break waited;
        };
        let wall_us = micros(forked);
        if waited != pid {
            // A copy left running would read the template's guest memory
            // while the template changes it: stop it before going on.
            let error = std::io::Error::last_os_error();
            // SAFETY: signals and reaps the copy forked above.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            children.push(
                serde_json::json!({ "index": index, "pid": pid, "wait_error": error.to_string() }),
            );
            break;
        }
        children.push(serde_json::json!({
            "index": index,
            "pid": pid,
            "fork_us": fork_us,
            "wall_us": wall_us,
            "exit_code": libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)),
            "signal": libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status)),
        }));
    }
    let summary = serde_json::json!({
        "backend": hooks.name,
        "turn_requested": spec.turn,
        "turn_committed": turn,
        "copies": spec.copies,
        "prepare_us": prepare_us,
        "prepare": prepared,
        "file_records": records.len(),
        "file_records_us": records_us,
        "offsets": offsets,
        "children": children,
    });
    let _ = std::fs::write(
        spec.dir.join("template.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        ),
    );
}

fn become_timeline(
    spec: &ForkSpec,
    index: usize,
    turn: u64,
    hooks: &BackendHooks,
    records: &[FileRecord],
    forked: Instant,
) {
    let started = Instant::now();
    TIMELINE.store(index as i64, Ordering::Relaxed);
    let _ = TIMELINE_DIR.set(spec.dir.clone());
    let fail = |what: &str, error: String| -> ! {
        let _ = std::fs::write(
            spec.dir.join(format!("timeline-{index}.error")),
            format!("{what}: {error}\n"),
        );
        // SAFETY: _exit ends the process at once.
        unsafe { libc::_exit(97) };
    };
    let repair_started = Instant::now();
    let repaired = match (hooks.repair)() {
        Ok(description) => description,
        Err(error) => fail("backend repair", error),
    };
    let repair_us = micros(repair_started);
    let files_started = Instant::now();
    let reopened = reopen_files(records);
    let files_us = micros(files_started);
    if let Err(error) = redirect_outputs(&spec.dir, index) {
        fail("redirecting output", error.to_string());
    }
    let summary = serde_json::json!({
        "index": index,
        // SAFETY: getpid has no preconditions.
        "pid": unsafe { libc::getpid() },
        "turn_committed": turn,
        "fork_to_child_us": started.duration_since(forked).as_micros() as u64,
        "repair_us": repair_us,
        "repair": repaired,
        "files_reopened": reopened,
        "files_us": files_us,
        "ready_us": micros(forked),
    });
    let _ = std::fs::write(
        spec.dir.join(format!("timeline-{index}.json")),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_spec() {
        assert_eq!(
            ForkSpec::parse("turn=40,copies=3,dir=/tmp/x").unwrap(),
            ForkSpec {
                turn: 40,
                copies: 3,
                dir: PathBuf::from("/tmp/x"),
            }
        );
        assert!(ForkSpec::parse("copies=3,dir=/tmp/x").is_err());
        assert!(ForkSpec::parse("turn=4,dir=/tmp/x,colour=red").is_err());
    }
}
