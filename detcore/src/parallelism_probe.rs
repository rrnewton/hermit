/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Measurement-only record of the parallelism a run could have had.
//!
//! Setting `HERMIT_PARALLELISM_PROFILE=<path>` for `hermit` makes the
//! scheduler append one JSON line per committed turn to `<path>`: the turn
//! number, the thread, its process and address space, the resources the turn
//! checked out with their permissions, and two work samples taken while the
//! thread is stopped at the commit: the thread's host CPU time
//! (`/proc/<tid>/schedstat`) and its logical time as the global clock last
//! accounted it. The work a thread does between two of its commits belongs to
//! the earlier commit's turn, so a reader recovers each turn's cost from
//! consecutive samples of one thread. Each turn request also writes a
//! `segment` line: the syscalls the thread completed since its previous
//! request without filing one (they ran in the thread's exclusive slot and no
//! declared resource covers them), the namespace paths those syscalls looked
//! up or changed, and the syscall filing this request. When a thread is
//! registered, a `spawn` line names its creator, its process and that
//! process's wait parent. `scripts/parallelism-profile.rs` builds the
//! happens-before graph from these lines and reports work, span and the
//! resources on the critical path.
//!
//! Only Hermit's own process enables the probe: `hermit` calls
//! [`enable_from_env`] before it creates any container, opening the file in
//! the host's filename namespace, and the tracer it forks inherits that. A
//! tool that runs inside a guest process never calls it, so nothing is ever
//! opened or written there, whatever the guest's environment holds; on such
//! backends the profile records only what Hermit's own process sees.
//!
//! Nothing here changes scheduling, time or guest-visible state: the probe only
//! reads, and every entry point returns at once when it is not enabled. The
//! host CPU samples are host timing; they are measurements, never inputs. A
//! write that fails mid-run ends the run through the scheduler's refusal exit
//! rather than leaving an incomplete profile that reads as complete.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::LineWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use serde_json::json;

use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::MmId;

/// The environment variable naming the profile's output file.
pub const PARALLELISM_PROFILE_ENV: &str = "HERMIT_PARALLELISM_PROFILE";

/// The profile's output, set only by [`enable`]. Line buffered: the run may
/// end without dropping the scheduler, and a container child forked after
/// enabling must not inherit buffered bytes.
static OUTPUT: OnceLock<Mutex<LineWriter<File>>> = OnceLock::new();

/// Enable the profile if `HERMIT_PARALLELISM_PROFILE` names a file, opening it
/// for append. Called once by `hermit`, in its own process, before any
/// container exists. Returns whether the profile is now enabled.
pub fn enable_from_env() -> Result<bool, String> {
    match std::env::var_os(PARALLELISM_PROFILE_ENV) {
        Some(path) => enable(Path::new(&path)).map(|()| true),
        None => Ok(false),
    }
}

/// Enable the profile, appending to `path`.
pub fn enable(path: &Path) -> Result<(), String> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| {
            format!(
                "{PARALLELISM_PROFILE_ENV}={}: cannot open for append: {error}",
                path.display()
            )
        })?;
    OUTPUT
        .set(Mutex::new(LineWriter::new(file)))
        .map_err(|_| format!("{PARALLELISM_PROFILE_ENV}: already enabled"))
}

/// Whether the profile is enabled in this process.
pub fn enabled() -> bool {
    OUTPUT.get().is_some()
}

fn line(value: serde_json::Value) {
    let Some(output) = OUTPUT.get() else {
        return;
    };
    let mut out = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let written = serde_json::to_writer(&mut *out, &value)
        .map_err(std::io::Error::from)
        .and_then(|()| out.write_all(b"\n"));
    if let Err(error) = written {
        // An incomplete profile would read as complete. End the run the way
        // the scheduler ends a refused one, rather than unwinding a thread
        // that holds the scheduler and leaving the guest waiting.
        crate::scheduler::exit_on_scheduler_refusal(&format!(
            "HERMIT_POLICY_REFUSAL class=policy-refusal cause=parallelism-profile: \
             {PARALLELISM_PROFILE_ENV}: write failed: {error}; the profile is incomplete"
        ));
    }
}

/// A namespace path an untracked syscall looked up or changed, resolved to an
/// absolute host path where the probe could resolve it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsTarget {
    syscall: &'static str,
    mutates: bool,
    /// `None` when the path or descriptor could not be resolved; a reader must
    /// then treat the operation as touching the whole file system.
    path: Option<String>,
}

/// The syscall a thread is in: entered, and not yet followed by another.
struct Active {
    name: &'static str,
    args: [u64; 6],
    proc_tid: i32,
    fs: Vec<FsTarget>,
    /// Whether it filed a request, which makes it a turn's own syscall rather
    /// than an untracked one.
    requested: bool,
}

/// Per thread: the untracked syscalls completed since the last request, and
/// the syscall in progress.
#[derive(Default)]
struct Segment {
    counts: BTreeMap<&'static str, u32>,
    fs: Vec<FsTarget>,
    active: Option<Active>,
}

static SEGMENTS: OnceLock<Mutex<HashMap<i32, Segment>>> = OnceLock::new();

fn segments() -> &'static Mutex<HashMap<i32, Segment>> {
    SEGMENTS.get_or_init(Default::default)
}

/// How a file-system syscall names what it touches, by argument position
/// (x86-64 Linux): `(dirfd argument or None for the cwd, path argument,
/// mutates)` for each path, or the descriptor argument of a descriptor-only
/// mutation.
#[derive(Debug)]
enum FsArgs {
    Paths(&'static [(Option<usize>, usize, bool)]),
    Fd(usize),
}

fn fs_args(name: &str) -> Option<FsArgs> {
    use FsArgs::*;
    Some(match name {
        "stat" | "lstat" | "access" | "readlink" | "chdir" | "statfs" => Paths(&[(None, 0, false)]),
        "unlink" | "mkdir" | "rmdir" | "chmod" | "chown" | "lchown" | "utime" | "utimes"
        | "truncate" | "mknod" => Paths(&[(None, 0, true)]),
        "rename" | "link" => Paths(&[(None, 0, true), (None, 1, true)]),
        // The first argument is the link's contents, not a path.
        "symlink" => Paths(&[(None, 1, true)]),
        "newfstatat" | "statx" | "faccessat" | "faccessat2" | "readlinkat" => {
            Paths(&[(Some(0), 1, false)])
        }
        "unlinkat" | "mkdirat" | "fchmodat" | "fchownat" | "mknodat" | "futimesat" => {
            Paths(&[(Some(0), 1, true)])
        }
        // A null path changes the descriptor's own file; resolved below.
        "utimensat" => Paths(&[(Some(0), 1, true)]),
        "renameat" | "renameat2" | "linkat" => Paths(&[(Some(0), 1, true), (Some(2), 3, true)]),
        "symlinkat" => Paths(&[(Some(1), 2, true)]),
        "ftruncate" | "fchmod" | "fchown" | "fallocate" => Fd(0),
        _ => return None,
    })
}

fn proc_link(proc_tid: i32, which: &str) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{proc_tid}/{which}"))
        .ok()
        .filter(|target| target.is_absolute())
}

/// Resolve a guest path against its directory descriptor or the thread's
/// current directory, through the thread's `/proc` entries. Lexical only:
/// symbolic links in the path itself are not followed.
fn resolve(proc_tid: i32, dirfd: Option<i64>, path: &str) -> Option<String> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Some(path.display().to_string());
    }
    let base = match dirfd {
        None | Some(-100) => proc_link(proc_tid, "cwd")?,
        Some(fd) => proc_link(proc_tid, &format!("fd/{fd}"))?,
    };
    Some(base.join(path).display().to_string())
}

/// The namespace targets of file-system syscall `name` with arguments
/// `args`, reading its path strings through `memory`.
pub fn fs_targets<M: MemoryAccess>(
    name: &'static str,
    args: [u64; 6],
    proc_tid: i32,
    memory: &M,
) -> Vec<FsTarget> {
    let Some(spec) = fs_args(name) else {
        return Vec::new();
    };
    let read = |ptr: u64| -> Option<String> {
        let addr = Addr::<u8>::from_raw(ptr as usize)?;
        let text = memory.read_cstring(addr).ok()?;
        Some(text.to_string_lossy().into_owned())
    };
    match spec {
        FsArgs::Fd(fd) => vec![FsTarget {
            syscall: name,
            mutates: true,
            path: proc_link(proc_tid, &format!("fd/{}", args[fd] as i32))
                .map(|p| p.display().to_string()),
        }],
        FsArgs::Paths(paths) => paths
            .iter()
            .map(|(dirfd, path, mutates)| {
                let dirfd = dirfd.map(|i| args[i] as i32 as i64);
                let resolved = match read(args[*path]) {
                    Some(text) => resolve(proc_tid, dirfd, &text),
                    // utimensat(fd, NULL, ...) changes the descriptor's file.
                    None if name == "utimensat" && args[*path] == 0 => dirfd
                        .and_then(|fd| proc_link(proc_tid, &format!("fd/{fd}")))
                        .map(|p| p.display().to_string()),
                    None => None,
                };
                FsTarget {
                    syscall: name,
                    mutates: *mutates,
                    path: resolved,
                }
            })
            .collect(),
    }
}

/// Move a finished syscall into its thread's segment, unless it filed a
/// request of its own.
fn finish(segment: &mut Segment, done: Active) {
    if !done.requested {
        *segment.counts.entry(done.name).or_default() += 1;
        segment.fs.extend(done.fs);
    }
}

/// Note that thread `tid` entered syscall `name` with arguments `args`;
/// `proc_tid` names the thread under `/proc`, and `fs` holds the namespace
/// targets of a file-system syscall ([`fs_targets`]). Called at the start of
/// the local syscall path, before anything in it can file a request, so a
/// request filed while handling this syscall belongs to it. A previous
/// syscall that never reached [`complete_syscall`] (its handler returned
/// early) is complete now.
pub fn note_syscall(
    tid: DetTid,
    name: &'static str,
    args: [u64; 6],
    proc_tid: i32,
    fs: Vec<FsTarget>,
) {
    if !enabled() {
        return;
    }
    let mut segments = segments()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let segment = segments.entry(tid.as_raw()).or_default();
    let next = Active {
        name,
        args,
        proc_tid,
        fs,
        requested: false,
    };
    if let Some(previous) = segment.active.replace(next) {
        finish(segment, previous);
    }
}

/// Note that thread `tid` finished its syscall, before the post-handler hook
/// can take a turn, so a turn taken there comes after the syscall.
pub fn complete_syscall(tid: DetTid) {
    if !enabled() {
        return;
    }
    let mut segments = segments()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(segment) = segments.get_mut(&tid.as_raw())
        && let Some(done) = segment.active.take()
    {
        finish(segment, done);
    }
}

/// Whether `resources` only asks the scheduler for a turn boundary (a
/// timeslice end, a hand-off, a priority change point or a yield) rather than
/// for anything the current syscall touches. Such a request does not make the
/// syscall in progress a turn's own syscall; `sched_yield` itself files one,
/// so it is counted with the untracked syscalls.
fn is_maintenance(resources: &Resources) -> bool {
    !resources.resources.is_empty()
        && resources.resources.keys().all(|id| match id {
            ResourceID::SleepUntil(time) => time.as_nanos() == 0,
            ResourceID::SchedYield | ResourceID::PriorityChangePoint(..) => true,
            _ => false,
        })
}

/// Count a trapped instruction `name` (such as `rdtsc`) that thread `tid`
/// executed without a turn, alongside its untracked syscalls.
pub fn note_event(tid: DetTid, name: &'static str) {
    if !enabled() {
        return;
    }
    let mut segments = segments()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *segments
        .entry(tid.as_raw())
        .or_default()
        .counts
        .entry(name)
        .or_default() += 1;
}

/// Syscalls whose first argument is the descriptor a polling retry waits on.
const FD_FIRST: &[&str] = &[
    "read", "write", "readv", "writev", "pread64", "pwrite64", "preadv", "pwritev", "preadv2",
    "pwritev2", "recvfrom", "sendto", "recvmsg", "sendmsg", "accept", "accept4", "connect",
    "splice", "tee",
];

/// Write the untracked syscalls thread `tid` completed since its previous
/// request `resources`, and name the syscall in progress, which this request
/// belongs to unless it only asks for a turn boundary (a yield, a timeslice
/// end or a priority change point). For a descriptor syscall also name what
/// the descriptor refers to (for a pipe, `pipe:[inode]`), and for an open its
/// flags; every request a syscall files (a retry loop files many) carries the
/// same identity. Called as the thread files a request, before the scheduler
/// can commit it.
pub fn flush_segment(tid: DetTid, resources: &Resources) {
    if !enabled() {
        return;
    }
    let maintenance = is_maintenance(resources);
    let (counts, fs, requesting) = {
        let mut segments = segments()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let segment = segments.entry(tid.as_raw()).or_default();
        let requesting = segment
            .active
            .as_mut()
            .filter(|_| !maintenance)
            .map(|active| {
                active.requested = true;
                (active.name, active.args, active.proc_tid)
            });
        (
            std::mem::take(&mut segment.counts),
            std::mem::take(&mut segment.fs),
            requesting,
        )
    };
    let target = requesting.and_then(|(name, args, proc_tid)| {
        FD_FIRST.contains(&name).then_some(())?;
        std::fs::read_link(format!("/proc/{proc_tid}/fd/{}", args[0] as i32))
            .ok()
            .map(|t| t.to_string_lossy().into_owned())
    });
    // An open's flags: whether it may create, truncate or write what its
    // `Path` turn (always declared read) names.
    let open_flags = requesting.and_then(|(name, args, _)| match name {
        "openat" => Some(args[2]),
        "open" => Some(args[1]),
        "creat" => Some((libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC) as u64),
        _ => None,
    });
    line(json!({
        "event": "segment",
        "tid": tid.as_raw(),
        "syscalls": counts,
        "maintenance": maintenance,
        "requesting": requesting.map(|(name, _, _)| name),
        "requesting_fd": target,
        "requesting_open_flags": open_flags,
        "fs": fs
            .into_iter()
            .map(|t| json!([t.syscall, if t.mutates { "W" } else { "R" }, t.path]))
            .collect::<Vec<_>>(),
    }));
}

/// What the probe last learned about a thread from its request.
#[derive(Clone, Copy)]
struct RequestNote {
    pid: DetPid,
    logical_time: LogicalTime,
}

/// The scheduler's part of the probe. One exists only while the profile is
/// enabled.
pub struct ParallelismProbe {
    notes: HashMap<DetTid, RequestNote>,
}

impl std::fmt::Debug for ParallelismProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelismProbe").finish_non_exhaustive()
    }
}

fn perm_name(perm: &Permission) -> &'static str {
    match perm {
        Permission::R => "R",
        Permission::W => "W",
        Permission::RW => "RW",
    }
}

/// The host CPU time of thread `tid`, in nanoseconds, from the first field of
/// `/proc/<tid>/schedstat`.
fn host_cpu_ns(tid: i32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{tid}/schedstat")).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

impl ParallelismProbe {
    /// The scheduler's recorder, or `None` when the profile is not enabled in
    /// this process.
    pub fn new_if_enabled() -> Option<Self> {
        if !enabled() {
            return None;
        }
        line(json!({"event": "start", "host_pid": std::process::id()}));
        Some(ParallelismProbe {
            notes: HashMap::new(),
        })
    }

    /// Note the process and logical time a thread's request carried.
    pub fn note_request(&mut self, tid: DetTid, pid: DetPid, logical_time: LogicalTime) {
        self.notes.insert(tid, RequestNote { pid, logical_time });
    }

    /// Record that `child` was registered by `creator`, in process `process`,
    /// whose wait parent (the process `wait` reaps it from, which
    /// `CLONE_PARENT` makes the creator's parent) is `wait_parent`.
    pub fn note_spawn(
        &mut self,
        creator: DetTid,
        child: DetTid,
        process: Option<DetPid>,
        wait_parent: Option<DetPid>,
    ) {
        line(json!({
            "event": "spawn",
            "creator": creator.as_raw(),
            "child": child.as_raw(),
            "process": process.map(|p| p.as_raw()),
            "wait_parent": wait_parent.map(|p| p.as_raw()),
        }));
    }

    /// Record a committed turn. `physical` is the host (process, thread) the
    /// scheduler registered for `tid`, if any.
    pub fn note_commit(
        &mut self,
        turn: u64,
        tid: DetTid,
        resources: &Resources,
        mm: Option<MmId>,
        physical: Option<(i32, i32)>,
    ) {
        // Without a registered host thread, a ptrace guest's scheduler ID is
        // its host thread ID.
        let (cpu_source, host_tid) = match physical {
            Some((_, host_tid)) => ("registered", host_tid),
            None => ("dettid", tid.as_raw()),
        };
        let cpu_ns = host_cpu_ns(host_tid);
        let note = self.notes.get(&tid).copied();
        let mut res: Vec<(String, &'static str, serde_json::Value)> = resources
            .resources
            .iter()
            .map(|(id, perm)| {
                let value = serde_json::to_value(id).unwrap_or(serde_json::Value::Null);
                (value.to_string(), perm_name(perm), value)
            })
            .collect();
        // HashMap order is arbitrary; sort so the file is stable.
        res.sort_by(|a, b| a.0.cmp(&b.0));
        let res: Vec<serde_json::Value> = res
            .into_iter()
            .map(|(_, perm, value)| json!([value, perm]))
            .collect();
        line(json!({
            "event": "commit",
            "turn": turn,
            "tid": tid.as_raw(),
            "pid": note.map(|n| n.pid.as_raw()),
            "mm": mm.map(|mm| serde_json::to_value(mm).unwrap_or(serde_json::Value::Null)),
            "host_tid": host_tid,
            "cpu_source": cpu_source,
            "cpu_ns": cpu_ns,
            "ltime_ns": note.map(|n| n.logical_time.as_nanos()),
            "poll": resources.poll_attempt,
            "fyi": resources.fyi,
            "res": res,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without `enable`, as in every ordinary run and in every process other
    /// than `hermit`'s own, the probe is off: no scheduler recorder exists,
    /// and the hooks record nothing, so no state changes and no file is
    /// written. The environment variable alone enables nothing.
    #[test]
    fn the_probe_is_off_and_inert_until_hermit_enables_it() {
        assert!(!enabled());
        assert!(ParallelismProbe::new_if_enabled().is_none());
        let tid = DetTid::from_raw(4242);
        note_syscall(tid, "read", [3, 0, 0, 0, 0, 0], 4242, Vec::new());
        note_event(tid, "rdtsc");
        complete_syscall(tid);
        flush_segment(tid, &Resources::new(tid));
        assert!(
            SEGMENTS.get().is_none(),
            "a disabled probe must not even create its segment table"
        );
    }

    #[test]
    fn mutation_targets_follow_the_linux_argument_positions() {
        let Some(FsArgs::Paths(symlink)) = fs_args("symlinkat") else {
            panic!("symlinkat names paths");
        };
        // symlinkat(target, newdirfd, linkpath): only the link is created.
        assert_eq!(symlink, &[(Some(1), 2, true)]);
        let Some(FsArgs::Paths(rename)) = fs_args("renameat2") else {
            panic!("renameat2 names paths");
        };
        assert_eq!(rename, &[(Some(0), 1, true), (Some(2), 3, true)]);
        assert!(matches!(fs_args("ftruncate"), Some(FsArgs::Fd(0))));
        assert!(fs_args("getpid").is_none());
    }

    /// A yield or a timeslice end is not the syscall's own request, so the
    /// syscall in progress keeps its place among the untracked ones; a
    /// resource request is the syscall's own.
    #[test]
    fn only_resource_requests_claim_the_syscall_in_progress() {
        let tid = DetTid::from_raw(7);
        let mut yield_request = Resources::new(tid);
        yield_request.resources.insert(
            ResourceID::SleepUntil(LogicalTime::from_nanos(0)),
            Permission::W,
        );
        assert!(is_maintenance(&yield_request));
        let mut sched_yield = Resources::new(tid);
        sched_yield
            .resources
            .insert(ResourceID::SchedYield, Permission::W);
        assert!(is_maintenance(&sched_yield));
        let mut sleep = Resources::new(tid);
        sleep.resources.insert(
            ResourceID::SleepUntil(LogicalTime::from_nanos(5)),
            Permission::W,
        );
        assert!(!is_maintenance(&sleep));
        assert!(!is_maintenance(&Resources::new(tid)));
    }

    #[test]
    fn absolute_paths_resolve_to_themselves() {
        assert_eq!(resolve(1, Some(7), "/a/b"), Some("/a/b".to_string()));
        // An unresolvable directory descriptor leaves the path unresolved.
        assert_eq!(resolve(i32::MAX, Some(7), "rel"), None);
    }
}
