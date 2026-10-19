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
//! process's wait parent; a commit that ends or waits for a process also
//! names the process that reaps it then. Once the scheduler has shut down, an
//! `end` line marks the profile complete. `scripts/parallelism-profile.rs`
//! builds the happens-before graph from these lines and reports work, span
//! and the resources on the critical path.
//!
//! Paths are resolved through the thread's `/proc` entries only where guest
//! thread IDs name host tasks (`procfs_inode::guest_tids_name_host_tasks`);
//! elsewhere a relative path or a descriptor stays unresolved and a thread
//! without a registered host thread has unknown CPU time.
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
        .map_err(|_| format!("{PARALLELISM_PROFILE_ENV}: already enabled"))?;
    RECORDER
        .set(&ProfileRecorder)
        .map_err(|_| format!("{PARALLELISM_PROFILE_ENV}: already enabled"))
}

/// What the hooks below call once the profile is enabled. Its only
/// implementation is reached through [`RECORDER`], which only [`enable`] sets,
/// and only `hermit`'s `main` calls that: an image that never calls it (a
/// Detcore runtime loaded into a guest, such as SaBRe's or LiteInst's) links
/// none of the probe's code, only each hook's check of `RECORDER`. Code size
/// there moves the guest's own mappings, which some in-guest backends are
/// sensitive to.
trait Recorder: Sync {
    fn note_syscall(&self, tid: DetTid, name: &'static str, args: [u64; 6], proc_tid: Option<i32>);
    fn note_targets(&self, tid: DetTid, read: &dyn Fn(u64) -> Option<String>);
    fn complete_syscall(&self, tid: DetTid);
    fn note_event(&self, tid: DetTid, name: &'static str);
    fn flush_segment(&self, tid: DetTid, resources: &Resources);
    fn scheduler_probe(&self, dettids_name_host_tasks: bool) -> Box<dyn SchedulerProbe>;
    fn note_end(&self);
}

static RECORDER: OnceLock<&'static dyn Recorder> = OnceLock::new();

fn recorder() -> Option<&'static dyn Recorder> {
    RECORDER.get().copied()
}

/// Whether the profile is enabled in this process.
pub fn enabled() -> bool {
    RECORDER.get().is_some()
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
    /// The thread's `/proc/<tid>` name, when guest thread IDs name host tasks.
    proc_tid: Option<i32>,
    /// Its namespace targets, resolved when it is dispatched ([`note_targets`]).
    fs: Vec<FsTarget>,
    /// The paths an open resolves, which name what its declared `Path`
    /// resource (the guest's spelling) refers to.
    opens: Vec<Option<String>>,
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
        "stat" | "lstat" | "access" | "readlink" | "chdir" | "statfs" | "getxattr"
        | "lgetxattr" | "listxattr" | "llistxattr" => Paths(&[(None, 0, false)]),
        "unlink" | "mkdir" | "rmdir" | "chmod" | "chown" | "lchown" | "utime" | "utimes"
        | "truncate" | "mknod" | "setxattr" | "lsetxattr" | "removexattr" | "lremovexattr" => {
            Paths(&[(None, 0, true)])
        }
        "rename" | "link" => Paths(&[(None, 0, true), (None, 1, true)]),
        // The first argument is the link's contents, not a path.
        "symlink" => Paths(&[(None, 1, true)]),
        "newfstatat" | "statx" | "faccessat" | "faccessat2" | "readlinkat" => {
            Paths(&[(Some(0), 1, false)])
        }
        "unlinkat" | "mkdirat" | "fchmodat" | "fchmodat2" | "fchownat" | "mknodat"
        | "futimesat" => Paths(&[(Some(0), 1, true)]),
        // A null path changes the descriptor's own file; resolved below.
        "utimensat" => Paths(&[(Some(0), 1, true)]),
        "renameat" | "renameat2" | "linkat" => Paths(&[(Some(0), 1, true), (Some(2), 3, true)]),
        "symlinkat" => Paths(&[(Some(1), 2, true)]),
        "ftruncate" | "fchmod" | "fchown" | "fallocate" | "fsetxattr" | "fremovexattr" => Fd(0),
        _ => return None,
    })
}

/// The path an open names, by argument position as in [`fs_args`]. Detcore
/// declares an `openat`'s path as a `Path` resource in the guest's spelling;
/// resolving it the same way as the untracked targets lets a reader give both
/// one identity.
fn open_args(name: &str) -> Option<(Option<usize>, usize)> {
    match name {
        "open" | "creat" => Some((None, 0)),
        "openat" | "openat2" => Some((Some(0), 1)),
        _ => None,
    }
}

fn proc_link(proc_tid: Option<i32>, which: &str) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{}/{which}", proc_tid?))
        .ok()
        .filter(|target| target.is_absolute())
}

/// Resolve a guest path against its directory descriptor or the thread's
/// current directory, through the thread's `/proc` entries. Lexical only:
/// symbolic links in the path itself are not followed. Without a `/proc` name
/// for the thread only an absolute path resolves.
fn resolve(proc_tid: Option<i32>, dirfd: Option<i64>, path: &str) -> Option<String> {
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

fn read_path<M: MemoryAccess>(memory: &M, ptr: u64) -> Option<String> {
    let addr = Addr::<u8>::from_raw(ptr as usize)?;
    let text = memory.read_cstring(addr).ok()?;
    Some(text.to_string_lossy().into_owned())
}

/// The namespace targets of file-system syscall `name` with arguments
/// `args`, reading its path strings with `read`.
fn fs_targets(
    name: &'static str,
    args: [u64; 6],
    proc_tid: Option<i32>,
    read: &dyn Fn(u64) -> Option<String>,
) -> Vec<FsTarget> {
    let Some(spec) = fs_args(name) else {
        return Vec::new();
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

/// What a request takes from its thread's segment: the untracked syscalls
/// completed since the previous request, their targets, and the syscall in
/// progress if the request is its own.
struct Flushed {
    counts: BTreeMap<&'static str, u32>,
    fs: Vec<FsTarget>,
    requesting: Option<Requesting>,
}

/// The identity of the syscall filing a request, kept across its retries.
#[derive(Clone)]
struct Requesting {
    name: &'static str,
    args: [u64; 6],
    proc_tid: Option<i32>,
    opens: Vec<Option<String>>,
}

impl Segment {
    /// Move a finished syscall into the segment, unless it filed a request of
    /// its own.
    fn finish(&mut self, done: Active) {
        if !done.requested {
            *self.counts.entry(done.name).or_default() += 1;
            self.fs.extend(done.fs);
        }
    }

    /// The thread entered a syscall; a previous one still in progress (its
    /// handler returned early) is finished now.
    fn enter(&mut self, next: Active) {
        if let Some(previous) = self.active.replace(next) {
            self.finish(previous);
        }
    }

    /// The syscall in progress finished.
    fn complete(&mut self) {
        if let Some(done) = self.active.take() {
            self.finish(done);
        }
    }

    /// The thread filed a request. Unless it is a maintenance request, the
    /// syscall in progress is its own and no longer counts as untracked.
    fn flush(&mut self, maintenance: bool) -> Flushed {
        let requesting = self.active.as_mut().filter(|_| !maintenance).map(|active| {
            active.requested = true;
            Requesting {
                name: active.name,
                args: active.args,
                proc_tid: active.proc_tid,
                opens: active.opens.clone(),
            }
        });
        Flushed {
            counts: std::mem::take(&mut self.counts),
            fs: std::mem::take(&mut self.fs),
            requesting,
        }
    }
}

/// Run `f` on thread `tid`'s segment.
fn with_segment<R>(tid: DetTid, f: impl FnOnce(&mut Segment) -> R) -> R {
    let mut segments = segments()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(segments.entry(tid.as_raw()).or_default())
}

/// The profile's [`Recorder`].
struct ProfileRecorder;

impl Recorder for ProfileRecorder {
    fn note_syscall(&self, tid: DetTid, name: &'static str, args: [u64; 6], proc_tid: Option<i32>) {
        with_segment(tid, |segment| {
            segment.enter(Active {
                name,
                args,
                proc_tid,
                fs: Vec::new(),
                opens: Vec::new(),
                requested: false,
            })
        });
    }

    fn note_targets(&self, tid: DetTid, read: &dyn Fn(u64) -> Option<String>) {
        with_segment(tid, |segment| {
            let Some(active) = segment.active.as_mut() else {
                return;
            };
            active.fs = fs_targets(active.name, active.args, active.proc_tid, read);
            active.opens = open_args(active.name)
                .map(|(dirfd, path)| {
                    let dirfd = dirfd.map(|i| active.args[i] as i32 as i64);
                    read(active.args[path]).and_then(|text| resolve(active.proc_tid, dirfd, &text))
                })
                .into_iter()
                .collect();
        });
    }

    fn complete_syscall(&self, tid: DetTid) {
        with_segment(tid, Segment::complete);
    }

    fn note_event(&self, tid: DetTid, name: &'static str) {
        with_segment(tid, |segment| {
            *segment.counts.entry(name).or_default() += 1;
        });
    }

    fn flush_segment(&self, tid: DetTid, resources: &Resources) {
        write_segment(tid, resources);
    }

    fn scheduler_probe(&self, dettids_name_host_tasks: bool) -> Box<dyn SchedulerProbe> {
        line(json!({"event": "start", "host_pid": std::process::id()}));
        Box::new(ProfileSchedulerProbe {
            notes: HashMap::new(),
            dettids_name_host_tasks,
        })
    }

    fn note_end(&self) {
        line(json!({"event": "end", "host_pid": std::process::id()}));
    }
}

/// Note that thread `tid` entered syscall `name` with arguments `args`;
/// `proc_tid` names the thread under `/proc`, or is `None` where guest thread
/// IDs do not name host tasks (`procfs_inode::guest_tids_name_host_tasks`), and
/// nothing is then read from `/proc`. Called at the start of the local syscall
/// path, before anything in it can file a request, so a request filed while
/// handling this syscall belongs to it. A previous syscall that never reached
/// [`complete_syscall`] (its handler returned early) is complete now.
pub fn note_syscall(tid: DetTid, name: &'static str, args: [u64; 6], proc_tid: Option<i32>) {
    if let Some(recorder) = recorder() {
        recorder.note_syscall(tid, name, args, proc_tid);
    }
}

/// Resolve the namespace targets of thread `tid`'s syscall in progress,
/// reading its path strings through `memory`. Called as the syscall is
/// dispatched to its handler, after every gate that can yield before an
/// untracked syscall runs (the pre-handler hook, the happens-before
/// checkpoint), so another process sharing the thread's cwd or descriptor
/// table cannot change what the paths name before it runs. A syscall that
/// yields after this files a request, which declares what it touches.
pub fn note_targets<M: MemoryAccess>(tid: DetTid, memory: &M) {
    if let Some(recorder) = recorder() {
        recorder.note_targets(tid, &|ptr| read_path(memory, ptr));
    }
}

/// Note that thread `tid` finished its syscall: its result is decided, and
/// nothing after this (a posthook happens-before checkpoint, the post-handler
/// hook) is part of it, so a turn taken there comes after the syscall.
pub fn complete_syscall(tid: DetTid) {
    if let Some(recorder) = recorder() {
        recorder.complete_syscall(tid);
    }
}

/// Whether `resources` only asks the scheduler for a turn boundary (a
/// timeslice end, a hand-off, a priority change point, a yield or a
/// happens-before checkpoint) rather than for anything the current syscall
/// touches. Such a request does not make the syscall in progress a turn's own
/// syscall, so its effects stay with the untracked ones; `sched_yield` itself
/// files one, so it is counted with the untracked syscalls.
fn is_maintenance(resources: &Resources) -> bool {
    !resources.resources.is_empty()
        && resources.resources.keys().all(|id| match id {
            ResourceID::SleepUntil(time) => time.as_nanos() == 0,
            ResourceID::SchedYield
            | ResourceID::PriorityChangePoint(..)
            | ResourceID::HappensBeforeCheckpoint(..)
            | ResourceID::HappensBeforeSyscallCheckpoint { .. }
            | ResourceID::HappensBeforePosthookInterrupted { .. } => true,
            _ => false,
        })
}

/// Count a trapped instruction `name` (such as `rdtsc`) that thread `tid`
/// executed without a turn, alongside its untracked syscalls.
pub fn note_event(tid: DetTid, name: &'static str) {
    if let Some(recorder) = recorder() {
        recorder.note_event(tid, name);
    }
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
/// end, a priority change point or a happens-before checkpoint). For a
/// descriptor syscall also name what the descriptor refers to
/// (for a pipe, `pipe:[inode]`), and for an open its flags and the path it
/// resolves; every request a syscall files (a retry loop files many) carries
/// the same identity. Called as the thread files a request, before the
/// scheduler can commit it.
pub fn flush_segment(tid: DetTid, resources: &Resources) {
    if let Some(recorder) = recorder() {
        recorder.flush_segment(tid, resources);
    }
}

fn write_segment(tid: DetTid, resources: &Resources) {
    let maintenance = is_maintenance(resources);
    let Flushed {
        counts,
        fs,
        requesting,
    } = with_segment(tid, |segment| segment.flush(maintenance));
    let target = requesting.as_ref().and_then(|r| {
        FD_FIRST.contains(&r.name).then_some(())?;
        proc_link(r.proc_tid, &format!("fd/{}", r.args[0] as i32))
            .map(|t| t.to_string_lossy().into_owned())
    });
    // An open's flags: whether it may create, truncate or write what its
    // `Path` turn (always declared read) names.
    let open_flags = requesting.as_ref().and_then(|r| match r.name {
        "openat" => Some(r.args[2]),
        "open" => Some(r.args[1]),
        "creat" => Some((libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC) as u64),
        _ => None,
    });
    line(json!({
        "event": "segment",
        "tid": tid.as_raw(),
        "syscalls": counts,
        "maintenance": maintenance,
        "requesting": requesting.as_ref().map(|r| r.name),
        "requesting_fd": target,
        "requesting_open_flags": open_flags,
        "requesting_paths": requesting.map(|r| r.opens).unwrap_or_default(),
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

/// The scheduler's part of the probe, created by [`scheduler_probe`].
pub trait SchedulerProbe: Send + std::fmt::Debug {
    /// Note the process and logical time a thread's request carried.
    fn note_request(&mut self, tid: DetTid, pid: DetPid, logical_time: LogicalTime);

    /// Record that `child` was registered by `creator`, in process `process`,
    /// whose wait parent (the process `wait` reaps it from, which
    /// `CLONE_PARENT` makes the creator's parent) is `wait_parent`.
    fn note_spawn(
        &mut self,
        creator: DetTid,
        child: DetTid,
        process: Option<DetPid>,
        wait_parent: Option<DetPid>,
    );

    /// Record a committed turn. `physical` is the host (process, thread) the
    /// scheduler registered for `tid`, if any. `wait_parents` gives, for each
    /// process the turn's `Exit` or `WaitPhysicalChild` names
    /// ([`wait_parent_subjects`]), the process that reaps it now, which orphan
    /// adoption may have changed since the process was created.
    fn note_commit(
        &mut self,
        turn: u64,
        tid: DetTid,
        resources: &Resources,
        mm: Option<MmId>,
        physical: Option<(i32, i32)>,
        wait_parents: &[(DetPid, Option<DetPid>)],
    );
}

/// The scheduler's recorder, or `None` when the profile is not enabled in this
/// process. `dettids_name_host_tasks` says whether a thread without a
/// registered host thread can be sampled through `/proc/<DetTid>`; where it
/// cannot, such a thread's CPU time is recorded as unknown.
pub fn scheduler_probe(dettids_name_host_tasks: bool) -> Option<Box<dyn SchedulerProbe>> {
    recorder().map(|recorder| recorder.scheduler_probe(dettids_name_host_tasks))
}

struct ProfileSchedulerProbe {
    notes: HashMap<DetTid, RequestNote>,
    /// Whether a thread's scheduler ID names its host task under `/proc` when
    /// the scheduler registered no host thread for it
    /// (`procfs_inode::guest_tids_name_host_tasks`).
    dettids_name_host_tasks: bool,
}

impl std::fmt::Debug for ProfileSchedulerProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileSchedulerProbe")
            .finish_non_exhaustive()
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

impl SchedulerProbe for ProfileSchedulerProbe {
    fn note_request(&mut self, tid: DetTid, pid: DetPid, logical_time: LogicalTime) {
        self.notes.insert(tid, RequestNote { pid, logical_time });
    }

    fn note_spawn(
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

    fn note_commit(
        &mut self,
        turn: u64,
        tid: DetTid,
        resources: &Resources,
        mm: Option<MmId>,
        physical: Option<(i32, i32)>,
        wait_parents: &[(DetPid, Option<DetPid>)],
    ) {
        let (cpu_source, host_tid) = match physical {
            Some((_, host_tid)) => ("registered", Some(host_tid)),
            None if self.dettids_name_host_tasks => ("dettid", Some(tid.as_raw())),
            None => ("unknown", None),
        };
        let cpu_ns = host_tid.and_then(host_cpu_ns);
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
            "wait_parents": wait_parents
                .iter()
                .map(|(process, parent)| json!([process.as_raw(), parent.map(|p| p.as_raw())]))
                .collect::<Vec<_>>(),
        }));
    }
}

/// The processes whose wait parent a commit of `resources` needs: those an
/// `Exit` or `WaitPhysicalChild` names.
pub fn wait_parent_subjects(resources: &Resources) -> Vec<DetPid> {
    let mut subjects: Vec<DetPid> = resources
        .resources
        .keys()
        .filter_map(|id| match id {
            ResourceID::Exit { process, .. } => Some(*process),
            ResourceID::WaitPhysicalChild(child) => Some(*child),
            _ => None,
        })
        .collect();
    subjects.sort();
    subjects.dedup();
    subjects
}

/// Record that the run finished with every line written: called once the
/// scheduler has shut down. A profile without this line is incomplete (the
/// run was killed, or a write failed and ended it).
pub fn note_end() {
    if let Some(recorder) = recorder() {
        recorder.note_end();
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
        assert!(scheduler_probe(true).is_none());
        let tid = DetTid::from_raw(4242);
        note_syscall(tid, "read", [3, 0, 0, 0, 0, 0], Some(4242));
        note_event(tid, "rdtsc");
        complete_syscall(tid);
        flush_segment(tid, &Resources::new(tid));
        note_end();
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

    /// Every x86-64 Linux syscall that changes the file-system namespace or
    /// a file's metadata or size by path or descriptor records at least one
    /// mutated target, so none can silently mean "no file-system effect".
    #[test]
    fn every_linux_file_system_mutation_records_a_target() {
        const MUTATIONS: &[&str] = &[
            "unlink",
            "unlinkat",
            "rename",
            "renameat",
            "renameat2",
            "mkdir",
            "mkdirat",
            "rmdir",
            "link",
            "linkat",
            "symlink",
            "symlinkat",
            "mknod",
            "mknodat",
            "chmod",
            "fchmod",
            "fchmodat",
            "fchmodat2",
            "chown",
            "fchown",
            "lchown",
            "fchownat",
            "utime",
            "utimes",
            "utimensat",
            "futimesat",
            "truncate",
            "ftruncate",
            "fallocate",
            "setxattr",
            "lsetxattr",
            "fsetxattr",
            "removexattr",
            "lremovexattr",
            "fremovexattr",
        ];
        for name in MUTATIONS {
            let mutates = match fs_args(name) {
                Some(FsArgs::Fd(_)) => true,
                Some(FsArgs::Paths(paths)) => paths.iter().any(|(_, _, mutates)| *mutates),
                None => false,
            };
            assert!(mutates, "{name} records no mutated target");
        }
    }

    fn active(name: &'static str, fs: Vec<FsTarget>) -> Active {
        Active {
            name,
            args: [3, 0, 0, 0, 0, 0],
            proc_tid: None,
            fs,
            opens: Vec::new(),
            requested: false,
        }
    }

    /// An untracked unlink with happens-before checkpoints before it and
    /// after it: neither checkpoint claims it, it completes before the
    /// posthook checkpoint, and that checkpoint's request carries it and its
    /// target.
    #[test]
    fn checkpoints_keep_an_untracked_syscall_and_its_target() {
        let unlinked = FsTarget {
            syscall: "unlink",
            mutates: true,
            path: Some("/b/x".to_string()),
        };
        let mut segment = Segment::default();
        segment.enter(active("unlink", vec![unlinked.clone()]));
        let before = segment.flush(true);
        assert!(before.requesting.is_none());
        assert!(before.counts.is_empty(), "the unlink has not run yet");
        segment.complete();
        let after = segment.flush(true);
        assert_eq!(after.counts.get("unlink"), Some(&1));
        assert_eq!(after.fs, vec![unlinked]);
    }

    /// A syscall that files several requests (a retry loop) names itself in
    /// each of them, and is not counted as untracked afterwards.
    #[test]
    fn a_retried_request_keeps_its_syscall() {
        let mut segment = Segment::default();
        segment.enter(active("read", Vec::new()));
        for _ in 0..2 {
            let flushed = segment.flush(false);
            let requesting = flushed.requesting.expect("the read's own request");
            assert_eq!((requesting.name, requesting.args[0]), ("read", 3));
        }
        segment.complete();
        assert!(segment.flush(false).counts.is_empty());
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
        let mut checkpoint = Resources::new(tid);
        checkpoint
            .resources
            .insert(ResourceID::HappensBeforeCheckpoint(9), Permission::R);
        assert!(is_maintenance(&checkpoint));
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
        assert_eq!(resolve(Some(1), Some(7), "/a/b"), Some("/a/b".to_string()));
        // An unresolvable directory descriptor leaves the path unresolved.
        assert_eq!(resolve(Some(i32::MAX), Some(7), "rel"), None);
        // Where guest thread IDs name no host task, nothing is read from
        // `/proc`: only an absolute path resolves.
        assert_eq!(resolve(None, None, "/a/b"), Some("/a/b".to_string()));
        assert_eq!(resolve(None, None, "rel"), None);
    }
}
