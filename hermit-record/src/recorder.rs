/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod fs;
mod mmap;
mod network;
mod random;
mod time;

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs as stdfs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::ExitStatus;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
#[cfg(target_arch = "x86_64")]
use reverie::Rdtsc;
#[cfg(target_arch = "x86_64")]
use reverie::RdtscResult;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::Close;
use reverie::syscalls::Fcntl;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::Lseek;
use reverie::syscalls::OFlag;
use reverie::syscalls::Openat;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use reverie::syscalls::Whence;
use serde::Deserialize;
use serde::Serialize;

use crate::event::Event;
use crate::event::ExecDependency;
use crate::event::ExecDescriptor;
use crate::event::ExecDescriptorAlias;
use crate::event::ExecEvent;
use crate::event::ExecImage;
use crate::event::ExecMaterialization;
use crate::event::ExecMaterializationBase;
use crate::event::ExecRequest;
use crate::event::ExecTarget;
use crate::event::InterruptedPoll;
use crate::event::MkdirEvent;
use crate::event::OpenEvent;
use crate::event::OpenMaterialization;
use crate::event::ReplayFdKind;
use crate::event::SyscallEvent;
use crate::event_stream::ChildEventStreamIds;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventStreamId;
use crate::event_stream::EventWriter;
use crate::event_stream::ThreadEnding;

const MAX_EXEC_DEPENDENCY_DEPTH: usize = 5;

#[derive(Default, Serialize, Deserialize)]
pub struct RecorderThreadState {
    events: EventWriter,
    /// The poll a signal just ended in the kernel, until this thread's next
    /// syscall (`InterruptedPoll`).
    #[serde(default)]
    interrupted_poll: Option<InterruptedPoll>,
    stream_id: EventStreamId,
    child_stream_ids: ChildEventStreamIds,
    pending_exec: Option<PreparedExec>,
    bootstrapped: bool,
    /// The physical output endpoints the root guest inherited, which every
    /// thread inherits from the thread that created it (`ContainerOutputs`).
    outputs: ContainerOutputs,
    /// The root guest's regular-file output descriptions, which every thread
    /// shares with the thread that created it (`OutputDescriptions`).
    #[serde(skip)]
    output_descriptions: std::sync::Arc<OutputDescriptions>,
    /// Whether the recorder's last sight of this thread was the delivery
    /// stop of a signal whose default action ends it (`FatalDeliveries`).
    #[serde(skip)]
    fatal_pending: bool,
}

/// Duplicates of the stdout and stderr open file descriptions the root guest
/// inherited, when they are regular files, against which a captured write is
/// classified as moving the description's offset
/// (`SyscallEvent::Write::advances_output_offset`). Replay applies that flag
/// to its duplicate of the root's description (`ReplayOutputs`), so the flag
/// must describe the root's description too, whichever process wrote. Each
/// process used its own creation-time descriptors 1 and 2 before: a child
/// forked after its parent redirected stdout into a pipe wrote through a saved
/// copy of the root's description with no advance recorded, and a child whose
/// descriptor 1 was a second description of the same file recorded an advance
/// that replay applied to the root's (review of
/// https://github.com/rrnewton/hermit/pull/4037).
#[derive(Default)]
struct OutputDescriptions {
    stdout: Mutex<Option<OwnedFd>>,
    stderr: Mutex<Option<OwnedFd>>,
    /// Every guest thread the recorder has seen, any of which may still hold
    /// a descriptor for these descriptions (`release_unreferenced`).
    threads: Mutex<std::collections::BTreeSet<i32>>,
}

impl OutputDescriptions {
    fn capture(pid: Pid) -> Self {
        Self {
            stdout: Mutex::new(duplicate_regular_output(pid, libc::STDOUT_FILENO)),
            stderr: Mutex::new(duplicate_regular_output(pid, libc::STDERR_FILENO)),
            threads: Mutex::new(std::collections::BTreeSet::from([pid.as_raw()])),
        }
    }

    /// Whether `candidate` is the root guest's `output_fd` description.
    fn matches(&self, output_fd: libc::c_int, candidate: &OwnedFd) -> bool {
        let output = match output_fd {
            libc::STDOUT_FILENO => &self.stdout,
            libc::STDERR_FILENO => &self.stderr,
            _ => return false,
        };
        let output = output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        output.as_ref().is_some_and(|target| {
            crate::fd::same_open_file_description(candidate.as_raw_fd(), target.as_raw_fd())
                .unwrap_or(false)
        })
    }

    /// Drops a duplicate once no guest thread holds its description, so the
    /// recorder does not keep a description alive (a lock on it, say) after
    /// the guest closed it. `caller` has just changed its descriptor table;
    /// the description stays while `caller` or any other live guest thread
    /// still holds it, as one process's table says nothing of another's.
    fn release_unreferenced(&self, caller: Pid) {
        for output in [&self.stdout, &self.stderr] {
            let mut output = output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(target) = output.as_ref() else {
                continue;
            };
            if guest_has_open_file_description(caller, target) {
                continue;
            }
            let mut threads = self
                .threads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            threads.retain(|thread| Path::new(&format!("/proc/{thread}")).exists());
            if !threads
                .iter()
                .any(|thread| guest_has_open_file_description(Pid::from_raw(*thread), target))
            {
                output.take();
            }
        }
    }
}

/// The identities of the stdout and stderr the root guest inherited: the only
/// endpoints whose writes are captured as output, which replay emits again
/// because a reader outside the container consumes them. Every process's
/// `Recorder::new` sees only that process's descriptors 1 and 2 at its
/// creation, which in a child forked after its parent redirected its own
/// stdout are a pipe inside the container (flex forks each filter so). A write
/// into such a pipe was captured as output, and replay wrote it into the live
/// pipe, whose reader replay serves from the recording: once the pipe was full,
/// the writer waited forever (https://github.com/rrnewton/hermit/issues/3964).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct ContainerOutputs {
    stdout: Option<OutputIdentity>,
    stderr: Option<OutputIdentity>,
}

impl ContainerOutputs {
    /// Whether `metadata` names the root guest's stdout, and whether it names
    /// its stderr.
    fn matching(&self, metadata: &std::fs::Metadata) -> (bool, bool) {
        (
            self.stdout
                .is_some_and(|identity| identity.matches(metadata)),
            self.stderr
                .is_some_and(|identity| identity.matches(metadata)),
        )
    }
}

impl RecorderThreadState {
    fn push_event(&mut self, event: Event) -> Result<(), bincode::error::EncodeError> {
        self.events.push_event(event)
    }

    /// Writes out and closes the thread's streams as the thread exits
    /// (`EventWriter::finish`), with their terminal record when the recorder
    /// saw how the thread ended (`ending`), or says why its recording is
    /// incomplete.
    /// The recorder then marks the recording as one replay refuses
    /// (`detcore::detlog::record_replay_refusal`, kept in its metadata), rather
    /// than let replay fail later on the cut stream, far from the cause.
    fn finish(self, ending: Option<ThreadEnding>) -> Result<(), String> {
        let stream_id = self.stream_id;
        self.events.finish(ending).map_err(|error| {
            format!(
                "the recording of thread stream {stream_id} could not be written in full: {error}"
            )
        })
    }

    fn push_debug_event(&mut self, event: DebugEvent) -> Result<(), bincode::error::EncodeError> {
        self.events.push_debug_event(event)
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum PreparedExec {
    Ready(ExecEvent),
    Unrepresentable(String),
}

struct ExecDependencyContext<'a> {
    pid: Pid,
    root: &'a OwnedFd,
    visited: &'a mut HashSet<(libc::dev_t, libc::ino_t)>,
    dependencies: &'a mut Vec<ExecDependency>,
}

fn exec_request_uses_live_magic_path(path: &[u8]) -> bool {
    [
        b"/proc/self/".as_slice(),
        b"/proc/thread-self/".as_slice(),
        b"/dev/fd/".as_slice(),
    ]
    .into_iter()
    .any(|prefix| path.starts_with(prefix))
}

fn parse_decimal_fd(bytes: &[u8]) -> Option<libc::c_int> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0_i32, |value, byte| {
        byte.is_ascii_digit()
            .then_some(())
            .and_then(|()| value.checked_mul(10))
            .and_then(|value| value.checked_add(i32::from(*byte - b'0')))
    })
}

fn exec_request_descriptor_fd(request: &ExecRequest) -> Option<libc::c_int> {
    if request.path.is_empty() && request.flags & libc::AT_EMPTY_PATH != 0 {
        return Some(request.dirfd);
    }
    [
        b"/proc/self/fd/".as_slice(),
        b"/proc/thread-self/fd/".as_slice(),
        b"/dev/fd/".as_slice(),
    ]
    .into_iter()
    .find_map(|prefix| request.path.strip_prefix(prefix).and_then(parse_decimal_fd))
}

fn split_fd_alias(bytes: &[u8]) -> Option<(libc::c_int, Option<&[u8]>)> {
    match bytes.iter().position(|byte| *byte == b'/') {
        Some(index) => Some((
            parse_decimal_fd(&bytes[..index])?,
            Some(&bytes[index + 1..]),
        )),
        None => Some((parse_decimal_fd(bytes)?, None)),
    }
}

fn exec_magic_materialization(request: &ExecRequest) -> Option<(ExecMaterializationBase, Vec<u8>)> {
    for prefix in [
        b"/proc/self/root/".as_slice(),
        b"/proc/thread-self/root/".as_slice(),
    ] {
        if let Some(path) = request.path.strip_prefix(prefix) {
            return Some((ExecMaterializationBase::Root, path.to_vec()));
        }
    }
    for prefix in [
        b"/proc/self/cwd/".as_slice(),
        b"/proc/thread-self/cwd/".as_slice(),
    ] {
        if let Some(path) = request.path.strip_prefix(prefix) {
            return Some((ExecMaterializationBase::Cwd, path.to_vec()));
        }
    }
    for prefix in [
        b"/proc/self/fd/".as_slice(),
        b"/proc/thread-self/fd/".as_slice(),
        b"/dev/fd/".as_slice(),
    ] {
        let Some(rest) = request.path.strip_prefix(prefix) else {
            continue;
        };
        let (fd, suffix) = split_fd_alias(rest)?;
        if let Some(path) = suffix {
            return Some((ExecMaterializationBase::DirectoryFd(fd), path.to_vec()));
        }
    }
    None
}

fn exec_request_is_live_executable(request: &ExecRequest) -> bool {
    matches!(
        request.path.as_slice(),
        b"/proc/self/exe" | b"/proc/thread-self/exe"
    )
}

fn same_guest_open_file_description(
    pid: Pid,
    left: libc::c_int,
    right: libc::c_int,
) -> io::Result<bool> {
    const KCMP_FILE: libc::c_int = 0;
    let comparison = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid.as_raw(),
            pid.as_raw(),
            KCMP_FILE,
            left,
            right,
        )
    };
    if comparison == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(comparison == 0)
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd
)]
struct OutputIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Eq, PartialEq)]
enum MkdirProbeDisposition {
    Directory(i64),
    NotDirectory,
}

fn classify_mkdir_probe(result: Result<i64, Errno>) -> Result<MkdirProbeDisposition, Errno> {
    match result {
        Ok(fd) => Ok(MkdirProbeDisposition::Directory(fd)),
        Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => {
            Ok(MkdirProbeDisposition::NotDirectory)
        }
        Err(error) => Err(error),
    }
}

fn require_mkdir_probe_closed(result: Result<i64, Errno>) -> Result<(), Errno> {
    match result {
        Ok(0) => Ok(()),
        Ok(_) => Err(Errno::EIO),
        Err(error) => Err(error),
    }
}

impl OutputIdentity {
    fn for_fd(pid: Pid, fd: i32) -> Option<Self> {
        let metadata = std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw())).ok()?;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        self.device == metadata.dev() && self.inode == metadata.ino()
    }
}

fn duplicate_regular_output(pid: Pid, fd: libc::c_int) -> Option<std::os::fd::OwnedFd> {
    let metadata = std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw())).ok()?;
    metadata
        .file_type()
        .is_file()
        .then(|| crate::fd::duplicate_guest_fd(pid, fd).ok())
        .flatten()
}
fn guest_has_open_file_description(pid: Pid, target: &std::os::fd::OwnedFd) -> bool {
    let entries = match std::fs::read_dir(format!("/proc/{}/fd", pid.as_raw())) {
        Ok(entries) => entries,
        Err(_) => return true,
    };
    let mut compared = false;
    let mut saw_any = false;
    for entry in entries.flatten() {
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::c_int>().ok())
        else {
            continue;
        };
        saw_any = true;
        let Ok(candidate) = crate::fd::duplicate_guest_fd(pid, fd) else {
            continue;
        };
        match crate::fd::same_open_file_description(candidate.as_raw_fd(), target.as_raw_fd()) {
            Ok(true) => return true,
            Ok(false) => compared = true,
            Err(error) => tracing::debug!(
                %error,
                fd,
                "could not compare guest fd while releasing captured output"
            ),
        }
    }
    saw_any && !compared
}

/// A Reverie tool that records syscalls. Note that only syscalls that cannot be
/// made deterministic are forwarded to this tool.
#[derive(Serialize, Deserialize)]
pub struct Recorder {
    // TODO: We'll need to keep track of file descriptors here in order to
    // determine if a file descriptor should be fully recorded or simply cached
    // with a reflink. We can use `fstatfs` to figure out if the target file
    // system supports reflinks or not. All other file systems will need their
    // file interactions to be recorded on the syscall level.

    // Keep track of the data directory. Each thread uses this path to open its
    // event stream.
    data: PathBuf,
    /// This process's descriptors 1 and 2 when it was created. Only the root
    /// guest's are the container's output endpoints, so only the root thread
    /// takes them (`RecorderThreadState::outputs`).
    stdout: Option<OutputIdentity>,
    stderr: Option<OutputIdentity>,
    /// The fatal signals whose delivery stop was the last the recorder saw
    /// of a thread of this process (`FatalDeliveries`).
    #[serde(skip)]
    fatal_deliveries: Mutex<FatalDeliveries>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            data: PathBuf::new(),
            stdout: None,
            stderr: None,
            fatal_deliveries: Default::default(),
        }
    }
}

/// A signal delivered at a delivery stop whose default action, which the
/// kernel runs next, ends the thread's group: the signal, its `si_code` when
/// the stop's siginfo could be read, and the stream of the thread it was
/// delivered to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FatalDelivery {
    signal: i32,
    code: Option<i32>,
    stream: EventStreamId,
}

/// For each thread of a process, the fatal delivery that was the last thing
/// the recorder saw of it, if it was one. A later event of the thread (a
/// syscall, an rdtsc, an exec, another delivery) clears it, so a delivery
/// counts only when nothing followed it but the thread's exit: a delivery
/// stop, then the exit, with no handler run and no return to user code
/// between. A handled or ignored delivery never counts.
///
/// A death no such stop preceded has no evidence here, and its streams get no
/// terminal record. SIGKILL takes no stop (kernel/signal.c:2919 at v6.16),
/// nor does a signal whose action is SA_IMMUTABLE, as the forced SIGSYS of a
/// seccomp kill is (force_sig_seccomp, kernel/signal.c:1811-1823; HANDLER_EXIT
/// sets it, :1308-1309), and a seccomp kill of one thread of several exits it
/// with no signal at all (do_exit(SIGSYS), kernel/seccomp.c:1352).
#[derive(Debug, Default)]
struct FatalDeliveries(HashMap<i32, FatalDelivery>);

impl FatalDeliveries {
    /// Thread `tid` reached a delivery stop that delivers `delivery`'s signal,
    /// whose default action ends the thread when `delivery` is `Some`.
    fn delivered(&mut self, tid: i32, delivery: Option<FatalDelivery>) {
        match delivery {
            Some(delivery) => self.0.insert(tid, delivery),
            None => self.0.remove(&tid),
        };
    }

    /// The recorder saw another event of thread `tid`.
    fn event(&mut self, tid: i32) {
        self.0.remove(&tid);
    }

    /// How thread `tid`, which ended with `status`, ended, as its terminal
    /// record tells replay, or `None` when the recorder did not see it end. An
    /// exit status is an orderly exit: `exit`, `exit_group`, or another
    /// thread's `execve`, which ends the others with code 0 (do_group_exit,
    /// kernel/exit.c at v6.16). A signal is how the thread ended only when it
    /// is the fatal delivery that was the last thing the recorder saw of this
    /// thread, or, for a thread that signal's group exit ended, of exactly
    /// one other thread of the process: the record names that thread.
    fn ending(&self, tid: i32, status: ExitStatus) -> Option<ThreadEnding> {
        let signal = match status {
            ExitStatus::Exited(code) => return Some(ThreadEnding::Exited(code)),
            ExitStatus::Signaled(signal, _) => signal as i32,
            ExitStatus::SignaledRaw(signal, _) => signal,
        };
        let delivery = match self.0.get(&tid) {
            Some(own) => (own.signal == signal).then_some(own)?,
            None => {
                let mut others = self.0.values().filter(|other| other.signal == signal);
                let delivery = others.next()?;
                others.next().is_none().then_some(delivery)?
            }
        };
        Some(ThreadEnding::Signaled {
            signal,
            code: delivery.code,
            delivered_to: delivery.stream.clone(),
        })
    }
}

/// Whether the kernel's next step for `signal`, delivered to a thread whose
/// `/proc/<tid>/status` is `status`, is its default action ending the group:
/// neither caught (`SigCgt`) nor ignored (`SigIgn`), with a default action
/// that terminates or dumps core rather than ignoring, stopping or continuing
/// (signal(7)). `None` when the status names neither mask.
fn ends_by_default(signal: i32, status: &str) -> Option<bool> {
    let mask = |field: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
            .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
    };
    let bit = 1u64.checked_shl(u32::try_from(signal - 1).ok()?)?;
    if (mask("SigCgt")? | mask("SigIgn")?) & bit != 0 {
        return Some(false);
    }
    Some(!matches!(
        signal,
        libc::SIGCHLD
            | libc::SIGCONT
            | libc::SIGSTOP
            | libc::SIGTSTP
            | libc::SIGTTIN
            | libc::SIGTTOU
            | libc::SIGURG
            | libc::SIGWINCH
    ))
}

impl Recorder {
    /// Thread `guest` reached the delivery stop of `signal`, which Detcore
    /// delivers: remembered as the thread's fatal delivery when the kernel's
    /// next step is its default action ending the group (`ends_by_default`).
    fn note_delivery<G: Guest<Self>>(&self, guest: &mut G, signal: i32) {
        let tid = guest.tid().as_raw();
        let fatal = std::fs::read_to_string(format!("/proc/{tid}/status"))
            .ok()
            .and_then(|status| ends_by_default(signal, &status))
            == Some(true);
        let delivery = fatal.then(|| FatalDelivery {
            signal,
            // `si_code` follows `si_signo` and `si_errno` in Linux's
            // `siginfo_t`.
            code: guest.signal_info().map(|info| {
                i32::from_ne_bytes(info[8..12].try_into().expect("siginfo holds si_code"))
            }),
            stream: guest.thread_state().stream_id.clone(),
        });
        guest.thread_state_mut().fatal_pending = delivery.is_some();
        self.fatal_deliveries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .delivered(tid, delivery);
    }

    /// The recorder sees another event of thread `guest`, which therefore
    /// did not die of a delivery it saw before.
    fn note_event<G: Guest<Self>>(&self, guest: &mut G) {
        if std::mem::take(&mut guest.thread_state_mut().fatal_pending) {
            self.fatal_deliveries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .event(guest.tid().as_raw());
        }
    }
}

#[reverie::tool]
impl Tool for Recorder {
    type GlobalState = detcore::GlobalState;
    type ThreadState = RecorderThreadState;

    fn new(pid: Pid, cfg: &<Self::GlobalState as GlobalTool>::Config) -> Self {
        Self {
            data: cfg.replay_data.as_ref().unwrap().clone(),
            stdout: OutputIdentity::for_fd(pid, libc::STDOUT_FILENO),
            stderr: OutputIdentity::for_fd(pid, libc::STDERR_FILENO),
            fatal_deliveries: Default::default(),
        }
    }

    fn init_thread_state(
        &self,
        child: Tid,
        parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        let stream_id = match parent {
            None => EventStreamId::root(),
            Some((_, state)) => state.child_stream_ids.next(&state.stream_id),
        };
        // We have to unwrap because there is no way to handle errors here.
        RecorderThreadState {
            events: EventWriter::create(&self.data, &stream_id).unwrap_or_else(|err| {
                panic!(
                    "Failed to create {:?} for recording thread {} (physical {}): {}",
                    self.data, stream_id, child, err
                )
            }),
            stream_id,
            child_stream_ids: ChildEventStreamIds::default(),
            pending_exec: None,
            interrupted_poll: None,
            bootstrapped: parent.is_some_and(|(_, state)| state.bootstrapped),
            outputs: match parent {
                None => ContainerOutputs {
                    stdout: self.stdout,
                    stderr: self.stderr,
                },
                Some((_, state)) => state.outputs,
            },
            output_descriptions: match parent {
                None => {
                    std::sync::Arc::new(OutputDescriptions::capture(Pid::from_raw(child.as_raw())))
                }
                Some((_, state)) => {
                    state
                        .output_descriptions
                        .threads
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(child.as_raw());
                    state.output_descriptions.clone()
                }
            },
            fatal_pending: false,
        }
    }

    fn subscriptions(_config: &<Self::GlobalState as GlobalTool>::Config) -> Subscription {
        hermit_record_format::subscriptions::record_and_replay()
    }

    async fn on_exit_thread<G: GlobalRPC<Self::GlobalState>>(
        &self,
        tid: Tid,
        _global_state: &G,
        thread_state: Self::ThreadState,
        exit_status: ExitStatus,
    ) -> Result<(), Error> {
        let ending = self
            .fatal_deliveries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ending(tid.as_raw(), exit_status);
        if let Err(reason) = thread_state.finish(ending) {
            tracing::error!("[tid {tid}] {reason}");
            detcore::detlog::record_replay_refusal(&reason);
        }
        Ok(())
    }

    /// Detcore reports each signal delivery it lets through, at its delivery
    /// stop, after it has decided it; the answer cannot suppress it.
    async fn handle_signal_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: reverie::Signal,
    ) -> Result<Option<reverie::Signal>, Errno> {
        self.note_delivery(guest, signal as i32);
        Ok(Some(signal))
    }

    /// As `handle_signal_event`, for a realtime signal.
    async fn handle_raw_signal_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        signal: i32,
    ) -> Result<Option<i32>, Errno> {
        self.note_delivery(guest, signal);
        Ok(Some(signal))
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        self.note_event(guest);
        let result = self.record_syscall(guest, syscall).await;
        // The handler returned, so the call is no longer in flight: a thread
        // that exits after this was not inside it (`StreamEnd::call_in_flight`).
        guest.thread_state_mut().events.call_returned();
        result
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        // A successful exec's handler never returns: committing its event
        // here ends the call.
        self.note_event(guest);
        guest.thread_state_mut().events.call_returned();
        self.commit_exec(guest).await
    }

    #[cfg(target_arch = "x86_64")]
    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        self.note_event(guest);
        self.record_rdtsc(guest, request).await
    }
}

impl Recorder {
    /// Records one syscall (`Tool::handle_syscall_event`): its record, then
    /// its handler, which writes its events when the call returns.
    async fn record_syscall<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        self.record_raw_syscall(guest, syscall);

        // The kernel's restart of a poll a signal ended there runs in place
        // (`InterruptedPoll`); any other syscall forgets the poll.
        if let Some(poll) = guest.thread_state_mut().interrupted_poll.take()
            && poll.restarted_by(&syscall)
        {
            return Ok(self.handle_restarted_poll(guest, syscall, poll).await?);
        }

        Ok(match syscall {
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Execve(_) | Syscall::Execveat(_) => self.handle_exec(guest, syscall).await,
            Syscall::Brk(_) => self.let_through(guest, syscall).await,
            Syscall::Mprotect(_) => self.let_through(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::ArchPrctl(_) => {
                // To properly handle arch_prctl, we should prevent calls from
                // using ARCH_SET_CPUID since we already do that for the
                // tracees. However, it is rare for programs to use
                // ARCH_SET_CPUID. For all other arch_prctl subfunctions, we
                // should let it through.
                self.let_through(guest, syscall).await
            }
            Syscall::Read(syscall) => self.handle_read(guest, syscall).await,
            Syscall::Pread64(syscall) => self.handle_pread64(guest, syscall).await,
            Syscall::Readv(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.len(),
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Preadv(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len(),
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Preadv2(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len() as usize,
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Recvfrom(syscall) => self.handle_recvfrom(guest, syscall).await,
            Syscall::Recvmsg(syscall) => self.handle_recvmsg(guest, syscall).await,
            Syscall::Write(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Sendfile(syscall) => self.handle_sendfile(guest, syscall).await,
            Syscall::Pwrite64(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Writev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev2(syscall) => self.handle_write_family(guest, syscall.into()).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#3598): faccessat/faccessat2 are pure queries;
            // recording their result keeps replay off the replay chroot's files.
            Syscall::Faccessat(_) | Syscall::Other(Sysno::faccessat2, _) => {
                self.handle_simple(guest, syscall).await
            }
            #[cfg(target_arch = "x86_64")]
            Syscall::Access(_) => self.handle_simple(guest, syscall).await,
            Syscall::Lseek(_) => self.handle_simple(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Stat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Fstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Lstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Newfstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            #[cfg(target_arch = "aarch64")]
            Syscall::Fstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Statfs(syscall) => {
                self.handle_statfs(guest, syscall.into(), syscall.buf())
                    .await
            }
            Syscall::Fstatfs(syscall) => {
                self.handle_statfs(guest, syscall.into(), syscall.buf())
                    .await
            }
            Syscall::Statx(syscall) => self.handle_statx(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Getdents(syscall) => self.handle_getdents(guest, syscall).await,
            Syscall::Getdents64(syscall) => self.handle_getdents64(guest, syscall).await,
            Syscall::Mmap(syscall) => self.handle_mmap(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Madvise(syscall) => self.handle_madvise(guest, syscall).await,
            Syscall::Munmap(_) => self.let_through(guest, syscall).await,
            Syscall::Openat(_) => self.handle_open(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Open(_) => self.handle_open(guest, syscall).await,
            Syscall::Close(_) => self.handle_fd_table_mutation(guest, syscall).await,
            Syscall::Openat2(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Mkdirat(_) => self.handle_mkdir(guest, syscall).await,
            Syscall::Mknodat(_)
            | Syscall::Fchownat(_)
            | Syscall::Linkat(_)
            | Syscall::Renameat(_)
            | Syscall::Renameat2(_)
            | Syscall::Symlinkat(_)
            | Syscall::Fchmodat(_)
            | Syscall::Utimensat(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#3598): legacy path mutations share the *at
            // forms' result-only recording.
            #[cfg(target_arch = "x86_64")]
            Syscall::Rename(_)
            | Syscall::Link(_)
            | Syscall::Symlink(_)
            | Syscall::Chmod(_)
            | Syscall::Chown(_)
            | Syscall::Lchown(_)
            | Syscall::Mknod(_)
            | Syscall::Rmdir(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#3598)
            Syscall::Chdir(_) | Syscall::Fchdir(_) => {
                self.handle_working_directory_change(guest, syscall).await
            }
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Getcwd(syscall) => self.handle_getcwd(guest, syscall).await,
            Syscall::Fadvise64(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601): descriptor syncs and metadata changes
            // record only their result. Replay returns it for a placeholder
            // descriptor and reapplies a change to a file in the replay root.
            Syscall::Fsync(_)
            | Syscall::Fdatasync(_)
            | Syscall::Syncfs(_)
            | Syscall::Fchmod(_)
            | Syscall::Fchown(_)
            | Syscall::Fsetxattr(_)
            | Syscall::Fremovexattr(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601): path xattr changes share the other
            // path mutations' result-only recording.
            Syscall::Setxattr(_)
            | Syscall::Lsetxattr(_)
            | Syscall::Removexattr(_)
            | Syscall::Lremovexattr(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3601)
            Syscall::Getxattr(_)
            | Syscall::Lgetxattr(_)
            | Syscall::Fgetxattr(_)
            | Syscall::Listxattr(_)
            | Syscall::Llistxattr(_)
            | Syscall::Flistxattr(_) => self.handle_xattr_query(guest, syscall).await,
            Syscall::Flock(_) => self.handle_simple(guest, syscall).await,
            Syscall::Ftruncate(syscall) => self.handle_ftruncate(guest, syscall).await,
            Syscall::Dup(_) => self.handle_simple(guest, syscall).await,
            Syscall::Dup3(_) => self.handle_fd_table_mutation(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Dup2(_) => self.handle_fd_table_mutation(guest, syscall).await,
            Syscall::Ioctl(syscall) => self.handle_ioctl(guest, syscall).await,
            Syscall::Socket(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-979): pidfd_open is an input-only,
            // fd-returning syscall (like socket): record its return value so
            // the pidfd allocation is captured and can be recreated/validated
            // on replay. Without this arm it fell through to live injection and
            // the fd side effect was neither recorded nor replayed.
            Syscall::PidfdOpen(_) => self.handle_simple(guest, syscall).await,
            Syscall::ClockGettime(syscall) => self.handle_clock_gettime(guest, syscall).await,
            Syscall::ClockGetres(syscall) => self.handle_clock_getres(guest, syscall).await,
            Syscall::Gettimeofday(syscall) => self.handle_gettimeofday(guest, syscall).await,
            Syscall::Settimeofday(_) => self.handle_simple(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Time(syscall) => self.handle_time(guest, syscall).await,
            Syscall::Setsockopt(_) => self.handle_simple(guest, syscall).await,
            Syscall::Fcntl(call) if crate::event::fcntl_output(&call).is_some() => {
                self.handle_fcntl_output(guest, call).await
            }
            // FIXME: Not all fcntl cases are simple.
            Syscall::Fcntl(_) => self.handle_simple(guest, syscall).await,
            // epoll_ctl's answer depends on the kind of its target, which
            // replay may stand in for with an eventfd placeholder, so the
            // answer is recorded (https://github.com/rrnewton/hermit/issues/31).
            Syscall::EpollCtl(_) => self.handle_simple(guest, syscall).await,
            Syscall::Connect(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-3871): Replay does not recreate a connection's
            // peer, so the calls that set up and tear down a connection are
            // served from the log like `connect`.
            Syscall::Bind(_) | Syscall::Listen(_) | Syscall::Shutdown(_) => {
                self.handle_simple(guest, syscall).await
            }
            Syscall::Accept(call) => self.handle_accept4(guest, call.into()).await,
            Syscall::Accept4(call) => self.handle_accept4(guest, call).await,
            Syscall::Sendto(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendmsg(_) => self.handle_simple(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Poll(syscall) => self.handle_poll(guest, syscall).await,
            Syscall::Ppoll(syscall) => self.handle_ppoll(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Select(syscall) => self.handle_select(guest, syscall).await,
            Syscall::Pselect6(syscall) => self.handle_pselect6(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::EpollWait(syscall) => self.handle_epoll_wait(guest, syscall).await,
            Syscall::EpollPwait(syscall) => self.handle_epoll_pwait(guest, syscall).await,
            Syscall::Other(Sysno::epoll_pwait2, _) => {
                self.handle_epoll_pwait2(guest, syscall).await
            }
            Syscall::Getsockopt(syscall) => self.handle_sockopt_family(guest, syscall.into()).await,
            Syscall::Getpeername(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getsockname(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getrandom(syscall) => self.handle_getrandom(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Readlink(syscall) => self.handle_readlink(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            #[cfg(target_arch = "x86_64")]
            Syscall::Mkdir(_) => self.handle_mkdir(guest, syscall).await,
            #[cfg(target_arch = "x86_64")]
            Syscall::Unlink(_) => self.handle_simple(guest, syscall).await,
            Syscall::Unlinkat(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Other(Sysno::close_range, _) => self.handle_close_range(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#2407): Preserve pidfd_getfd's returned
            // descriptor and errno in the record stream.
            Syscall::Other(Sysno::pidfd_getfd, _) => {
                self.handle_fd_table_mutation(guest, syscall).await
            }
            unsupported => return Ok(guest.inject(unsupported).await?),
        }?)
    }

    /// Commits a successful exec's event (`Tool::handle_post_exec`).
    // TODO-HUMAN-REVIEW(#2370)
    async fn commit_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        let result = match guest.thread_state_mut().pending_exec.take() {
            Some(PreparedExec::Ready(event)) => {
                self.record_event(guest, Ok(SyscallEvent::Exec(event)));
                Ok(())
            }
            Some(PreparedExec::Unrepresentable(reason)) => {
                tracing::error!(
                    thread = %guest.tid(),
                    %reason,
                    "recording cannot represent successful exec"
                );
                Err(Errno::EIO)
            }
            None => Ok(()),
        };
        guest.thread_state_mut().bootstrapped = true;
        guest
            .thread_state()
            .output_descriptions
            .clone()
            .release_unreferenced(guest.pid());
        result
    }

    /// Records an rdtsc (`Tool::handle_rdtsc_event`).
    #[cfg(target_arch = "x86_64")]
    async fn record_rdtsc<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        let result = RdtscResult::new(request);
        self.record_event(guest, Ok(SyscallEvent::Rdtsc(result)));
        Ok(result)
    }
}

impl Recorder {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit recorded physical-open classification.
    async fn handle_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let materialize = result
            .ok()
            .and_then(|fd| {
                std::fs::metadata(format!("/proc/{}/fd/{fd}", guest.pid().as_raw())).ok()
            })
            .map_or(OpenMaterialization::None, |metadata| {
                if metadata.file_type().is_dir() {
                    OpenMaterialization::Directory
                } else if metadata.file_type().is_file() {
                    OpenMaterialization::RegularFile
                } else {
                    OpenMaterialization::None
                }
            });
        self.record_event(
            guest,
            Ok(SyscallEvent::Open(OpenEvent {
                result,
                materialize,
            })),
        );
        result
    }

    // TODO-HUMAN-REVIEW(#2370)
    /// Records enough information to distinguish the two guest-visible
    /// `EEXIST` cases: an already-existing directory needs to be reconstructed
    /// in the fresh replay root, while a file or symlink must remain an error
    /// without directory materialization.
    async fn handle_mkdir<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let existing_directory = if result == Err(Errno::EEXIST) {
            self.mkdir_target_is_directory(guest, syscall)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "could not classify recorded mkdir EEXIST target without changing its meaning: {error}"
                    )
                })
        } else {
            false
        };
        self.record_event(
            guest,
            Ok(SyscallEvent::Mkdir(MkdirEvent {
                result,
                existing_directory,
            })),
        );
        result
    }

    /// Probes the final pathname component without following a symlink. `O_PATH`
    /// avoids adding read/search-permission requirements that the original
    /// `mkdir` did not have. The temporary descriptor is closed before the
    /// guest resumes, so the probe has no persistent fd-table side effect.
    async fn mkdir_target_is_directory<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<bool, Errno> {
        let result = match syscall {
            #[cfg(target_arch = "x86_64")]
            Syscall::Mkdir(call) => {
                let path = call.path().ok_or(Errno::EFAULT)?;
                guest
                    .inject_with_retry(
                        Openat::new()
                            .with_dirfd(libc::AT_FDCWD)
                            .with_path(Some(path))
                            .with_flags(
                                OFlag::O_PATH
                                    | OFlag::O_DIRECTORY
                                    | OFlag::O_NOFOLLOW
                                    | OFlag::O_CLOEXEC,
                            ),
                    )
                    .await
            }
            Syscall::Mkdirat(call) => {
                let path = call.path().ok_or(Errno::EFAULT)?;
                guest
                    .inject_with_retry(
                        Openat::new()
                            .with_dirfd(call.dirfd())
                            .with_path(Some(path))
                            .with_flags(
                                OFlag::O_PATH
                                    | OFlag::O_DIRECTORY
                                    | OFlag::O_NOFOLLOW
                                    | OFlag::O_CLOEXEC,
                            ),
                    )
                    .await
            }
            _ => unreachable!("mkdir directory probe called for {syscall:?}"),
        };
        match classify_mkdir_probe(result)? {
            MkdirProbeDisposition::Directory(fd) => {
                let closed = guest
                    .inject_with_retry(Close::new().with_fd(fd as libc::c_int))
                    .await;
                require_mkdir_probe_closed(closed)?;
                Ok(true)
            }
            MkdirProbeDisposition::NotDirectory => Ok(false),
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit guest FD replay classification.
    fn fd_replay_kind(&self, pid: Pid, fd: libc::c_int) -> ReplayFdKind {
        let path = format!("/proc/{}/fd/{fd}", pid.as_raw());
        if std::fs::read_link(&path)
            .ok()
            .is_some_and(|target| target == std::path::Path::new("anon_inode:[eventfd]"))
        {
            return ReplayFdKind::Eventfd;
        }

        if std::fs::metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_file())
        {
            ReplayFdKind::RegularFile
        } else {
            ReplayFdKind::None
        }
    }

    fn epoll_requires_replay_kernel_side_effect(&self, pid: Pid, fd: libc::c_int) -> bool {
        let Ok(fdinfo) = std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", pid.as_raw()))
        else {
            return false;
        };
        let targets = fdinfo.lines().filter_map(|line| {
            line.strip_prefix("tfd:")?
                .split_whitespace()
                .next()?
                .parse::<libc::c_int>()
                .ok()
        });
        let mut saw_target = false;
        for target in targets {
            saw_target = true;
            if self.fd_replay_kind(pid, target) == ReplayFdKind::None {
                return false;
            }
        }
        saw_target
    }

    /// Whether `candidate` is the root guest's `output_fd` description, which
    /// `guest`'s thread shares (`OutputDescriptions`).
    pub(super) fn output_ofd_matches<G: Guest<Self>>(
        guest: &G,
        output_fd: libc::c_int,
        candidate: &std::os::fd::OwnedFd,
    ) -> bool {
        guest
            .thread_state()
            .output_descriptions
            .matches(output_fd, candidate)
    }

    async fn handle_fd_table_mutation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        guest
            .thread_state()
            .output_descriptions
            .clone()
            .release_unreferenced(guest.pid());
        self.record_event(guest, result.map(SyscallEvent::Return));
        result
    }

    // TODO-HUMAN-REVIEW(#557): Audit close_range fd-table replay semantics.
    async fn handle_close_range<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let Syscall::Other(Sysno::close_range, args) = syscall else {
            unreachable!("handle_close_range called for {syscall:?}");
        };

        if args.arg2 & libc::CLOSE_RANGE_UNSHARE as usize != 0 {
            let result = Err(Errno::ENOSYS);
            self.record_event(guest, result.map(SyscallEvent::Return));
            return result;
        }

        self.handle_fd_table_mutation(guest, syscall).await
    }

    fn record_raw_syscall<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let debug_event = DebugEvent::new(syscall, &guest.memory());
        guest
            .thread_state_mut()
            .push_debug_event(debug_event)
            .unwrap();
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn handle_exec<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let prepared = match self.prepare_exec(guest, syscall).await {
            Ok(event) => PreparedExec::Ready(event),
            Err(error) => PreparedExec::Unrepresentable(error.to_string()),
        };
        assert!(
            guest
                .thread_state_mut()
                .pending_exec
                .replace(prepared)
                .is_none(),
            "nested pending exec on thread {}",
            guest.tid()
        );

        // A successful exec never returns here. Its pending event is committed
        // by handle_post_exec after Linux has replaced the image.
        let initial_root_exec = guest.is_root_thread() && !guest.thread_state().bootstrapped;
        let result = guest.inject(syscall).await;
        let pending = guest.thread_state_mut().pending_exec.take();
        assert!(pending.is_some(), "failed exec lost its pending event");
        let error = result.expect_err("successful exec unexpectedly returned to syscall handler");
        if initial_root_exec && error == Errno::ENOEXEC {
            panic!(
                "record/replay does not support execvpe shell fallback for an initial executable without a recognized format; add an explicit shebang"
            );
        }
        self.record_event(guest, Err(error));
        Err(error)
    }

    async fn prepare_exec<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> io::Result<ExecEvent> {
        let call = match syscall {
            Syscall::Execve(call) => call.into(),
            Syscall::Execveat(call) => call,
            _ => unreachable!("exec preparation called for {syscall:?}"),
        };
        let path_ptr = call
            .path()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
        let path = path_ptr
            .read(&guest.memory())
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        let request = ExecRequest {
            dirfd: call.dirfd(),
            path: path.as_os_str().as_bytes().to_vec(),
            flags: call.flags(),
        };
        let allowed_flags = libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW;
        if request.flags & !allowed_flags != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported execveat flags {:#x}", request.flags),
            ));
        }

        let pinned = if path.as_os_str().is_empty() {
            if request.flags & libc::AT_EMPTY_PATH == 0 {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            crate::record_replay_path::open_process_fd(guest.pid(), request.dirfd)?
        } else {
            self.pin_exec_path(guest, request.dirfd, path_ptr, request.flags)
                .await?
        };
        let pinned_identity = crate::record_replay_path::file_identity(pinned.as_raw_fd())?;
        if !crate::record_replay_path::is_regular_file(pinned_identity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "successful exec target would not be a regular file",
            ));
        }

        let root = crate::record_replay_path::open_process_root(guest.pid())?;
        let target = if let Some(fd) = exec_request_descriptor_fd(&request) {
            ExecTarget::RestoreDescriptor(
                self.capture_exec_descriptor(guest, fd, pinned_identity)
                    .await?,
            )
        } else if exec_request_is_live_executable(&request) {
            ExecTarget::VerifyLive
        } else {
            let (base, materialization_path) =
                if let Some(materialization) = exec_magic_materialization(&request) {
                    materialization
                } else if exec_request_uses_live_magic_path(&request.path) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "unsupported process-relative executable path {:?}",
                            path.as_os_str()
                        ),
                    ));
                } else if path.is_absolute() {
                    (
                        ExecMaterializationBase::Root,
                        path.as_os_str().as_bytes().to_vec(),
                    )
                } else if request.dirfd == libc::AT_FDCWD {
                    (
                        ExecMaterializationBase::Cwd,
                        path.as_os_str().as_bytes().to_vec(),
                    )
                } else {
                    (
                        ExecMaterializationBase::DirectoryFd(request.dirfd),
                        path.as_os_str().as_bytes().to_vec(),
                    )
                };
            let start = match &base {
                ExecMaterializationBase::Root => {
                    crate::record_replay_path::open_process_root(guest.pid())?
                }
                ExecMaterializationBase::Cwd => {
                    crate::record_replay_path::open_process_cwd(guest.pid())?
                }
                ExecMaterializationBase::DirectoryFd(fd) => {
                    crate::record_replay_path::open_process_directory_fd(guest.pid(), *fd)?
                }
            };
            let materialization_path =
                PathBuf::from(std::ffi::OsString::from_vec(materialization_path));
            let resolved = crate::record_replay_path::resolve_existing_path(
                &root,
                &start,
                &materialization_path,
                request.flags & libc::AT_SYMLINK_NOFOLLOW != 0,
            )?;
            let resolved_identity =
                crate::record_replay_path::file_identity(resolved.object.as_raw_fd())?;
            if resolved_identity.device != pinned_identity.device
                || resolved_identity.inode != pinned_identity.inode
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "descriptor path walk selected a different exec object",
                ));
            }
            ExecTarget::Materialize(ExecMaterialization {
                base,
                path: materialization_path.as_os_str().as_bytes().to_vec(),
                symlinks: resolved.symlinks,
            })
        };

        let (executable, snapshot) = self.snapshot_exec_object(&pinned)?;
        let mut dependencies = Vec::new();
        let mut visited = HashSet::new();
        let mut dependency_context = ExecDependencyContext {
            pid: guest.pid(),
            root: &root,
            visited: &mut visited,
            dependencies: &mut dependencies,
        };
        self.record_exec_dependencies(&snapshot, 0, &mut dependency_context)?;

        Ok(ExecEvent {
            request,
            executable,
            target,
            dependencies,
        })
    }

    async fn capture_exec_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target_fd: libc::c_int,
        target_identity: crate::record_replay_path::FileIdentity,
    ) -> io::Result<ExecDescriptor> {
        let status_flags = guest
            .inject_with_retry(Fcntl::new().with_fd(target_fd).with_cmd(FcntlCmd::F_GETFL))
            .await
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
            as libc::c_int;
        if status_flags & libc::O_PATH == 0 && status_flags & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "successful descriptor exec used unsupported access flags {status_flags:#x}"
                ),
            ));
        }

        let offset = match guest
            .inject_with_retry(
                Lseek::new()
                    .with_fd(target_fd)
                    .with_offset(0)
                    .with_whence(Whence::SEEK_CUR),
            )
            .await
        {
            Ok(offset) => Some(offset as libc::off_t),
            Err(Errno::EBADF | Errno::ESPIPE) => None,
            Err(error) => return Err(io::Error::from_raw_os_error(error.into_raw())),
        };

        let mut candidate_fds = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/{}/fd", guest.pid().as_raw()))? {
            let entry = entry?;
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::c_int>().ok())
            else {
                continue;
            };
            let metadata = match std::fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.dev() == target_identity.device && metadata.ino() == target_identity.inode {
                candidate_fds.push(fd);
            }
        }
        candidate_fds.sort_unstable();

        let mut aliases = Vec::new();
        for fd in candidate_fds {
            if fd != target_fd && !same_guest_open_file_description(guest.pid(), target_fd, fd)? {
                continue;
            }
            let descriptor_flags = guest
                .inject_with_retry(Fcntl::new().with_fd(fd).with_cmd(FcntlCmd::F_GETFD))
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                as libc::c_int;
            aliases.push(ExecDescriptorAlias {
                fd,
                descriptor_flags,
            });
        }
        if !aliases.iter().any(|alias| alias.fd == target_fd) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exec descriptor disappeared while its state was captured",
            ));
        }

        Ok(ExecDescriptor {
            target_fd,
            status_flags,
            offset,
            aliases,
        })
    }

    async fn pin_exec_path<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: libc::c_int,
        path: reverie::syscalls::PathPtr<'_>,
        flags: libc::c_int,
    ) -> io::Result<OwnedFd> {
        let mut open_flags = OFlag::O_PATH | OFlag::O_CLOEXEC;
        if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
            open_flags |= OFlag::O_NOFOLLOW;
        }
        let temporary = guest
            .inject_with_retry(
                Openat::new()
                    .with_dirfd(dirfd)
                    .with_path(Some(path))
                    .with_flags(open_flags),
            )
            .await
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        let pinned =
            crate::record_replay_path::open_process_fd(guest.pid(), temporary as libc::c_int);
        let close = guest
            .inject_with_retry(Close::new().with_fd(temporary as libc::c_int))
            .await;
        if close != Ok(0) {
            return Err(io::Error::other(format!(
                "could not close temporary exec descriptor: {close:?}"
            )));
        }
        pinned
    }

    fn snapshot_exec_object(&self, object: &OwnedFd) -> io::Result<(ExecImage, PathBuf)> {
        let before = crate::record_replay_path::file_identity(object.as_raw_fd())?;
        if !crate::record_replay_path::is_regular_file(before) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exec snapshot source is not a regular file",
            ));
        }
        let mut input = crate::record_replay_path::open_readable_fd(object.as_raw_fd())?;
        let snapshots = self.data.join(crate::consts::EXEC_FILES_NAME);
        stdfs::create_dir_all(&snapshots)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&snapshots)?;
        io::copy(&mut input, &mut temporary)?;
        temporary.flush()?;
        let after = crate::record_replay_path::file_identity(object.as_raw_fd())?;
        if before != after {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exec source changed while it was being snapshotted",
            ));
        }

        let digest = detcore::Digest::digest_path(temporary.path())?;
        let snapshot = snapshots.join(digest.to_string());
        match temporary.persist_noclobber(&snapshot) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                if detcore::Digest::digest_path(&snapshot)? != digest {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("recorded exec snapshot collision at {snapshot:?}"),
                    ));
                }
            }
            Err(error) => return Err(error.error),
        }

        Ok((
            ExecImage {
                digest,
                mode: before.mode & 0o7777,
            },
            snapshot,
        ))
    }

    fn record_exec_dependencies(
        &self,
        snapshot: &Path,
        depth: usize,
        context: &mut ExecDependencyContext<'_>,
    ) -> io::Result<()> {
        if depth > MAX_EXEC_DEPENDENCY_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many executable interpreter levels",
            ));
        }
        let mut header = Vec::new();
        stdfs::File::open(snapshot)?
            .take(256)
            .read_to_end(&mut header)?;
        let Some(dependency) = (if let Some(shebang) = crate::Shebang::from_buf(&header) {
            Some(shebang.interpreter().to_path_buf())
        } else {
            crate::interp::elf_get_interp(snapshot)
        }) else {
            return Ok(());
        };
        let request = ExecRequest {
            dirfd: libc::AT_FDCWD,
            path: dependency.as_os_str().as_bytes().to_vec(),
            flags: 0,
        };
        if exec_request_descriptor_fd(&request).is_some()
            || exec_request_is_live_executable(&request)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "descriptor-relative executable interpreter is unsupported: {dependency:?}"
                ),
            ));
        }
        let (base, path) = if let Some(materialization) = exec_magic_materialization(&request) {
            materialization
        } else if exec_request_uses_live_magic_path(&request.path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported process-relative executable interpreter: {dependency:?}"),
            ));
        } else if dependency.is_absolute() {
            (ExecMaterializationBase::Root, request.path)
        } else {
            (ExecMaterializationBase::Cwd, request.path)
        };
        let start = match &base {
            ExecMaterializationBase::Root => {
                crate::record_replay_path::open_process_root(context.pid)?
            }
            ExecMaterializationBase::Cwd => {
                crate::record_replay_path::open_process_cwd(context.pid)?
            }
            ExecMaterializationBase::DirectoryFd(fd) => {
                crate::record_replay_path::open_process_directory_fd(context.pid, *fd)?
            }
        };
        self.record_exec_dependency_from_start(&start, base, path, depth, context)
    }

    fn record_exec_dependency_from_start(
        &self,
        start: &OwnedFd,
        base: ExecMaterializationBase,
        path: Vec<u8>,
        depth: usize,
        context: &mut ExecDependencyContext<'_>,
    ) -> io::Result<()> {
        let dependency = PathBuf::from(std::ffi::OsString::from_vec(path.clone()));
        let resolved = crate::record_replay_path::resolve_existing_path(
            context.root,
            start,
            &dependency,
            false,
        )?;
        let identity = crate::record_replay_path::file_identity(resolved.object.as_raw_fd())?;
        if !context.visited.insert((identity.device, identity.inode)) {
            return Ok(());
        }
        let (image, dependency_snapshot) = self.snapshot_exec_object(&resolved.object)?;
        self.record_exec_dependencies(&dependency_snapshot, depth + 1, context)?;
        context.dependencies.push(ExecDependency {
            base,
            path,
            image,
            symlinks: resolved.symlinks,
        });
        Ok(())
    }

    fn record_event<G: Guest<Self>>(&self, guest: &mut G, event: Result<SyscallEvent, Errno>) {
        // Record the event.
        guest
            .thread_state_mut()
            .push_event(Event { event })
            // TODO: Log errors instead of panicking.
            .unwrap();
    }

    /// Called for syscalls to explicitly let through. This should only be called
    /// for syscalls that cannot be recorded and are necessary for the program to
    /// function correctly. Examples of syscalls that fall into this category are
    /// ones that help with memory management (e.g., `brk`, `mprotect`, `mmap`,
    /// or `munmap`) or process management (e.g., `fork`, `vfork`, `clone`).
    ///
    /// For these syscalls, we don't really need to record anything, but we
    /// record their arguments to detect any desynchronization.
    async fn let_through<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        guest.inject(syscall).await
    }

    /// Handles a syscall whose only value we care about is the return value
    /// (i.e., simple syscalls).
    ///
    /// For recording, this means we only record the return value of the syscall.
    /// For replay, this means we substitute the return value in lieu of actually
    /// performing the injection.
    ///
    /// The syscall must have two properties satisfied for this to be called:
    ///  1. The syscall must only have "input" arguments. That is, all arguments
    ///     must either be values or const pointers.
    ///  2. The execution of the program must not depend on anything else other
    ///     than the return value of the syscall. For example, `mmap` would violate
    ///     this rule since it affects later memory access.
    ///
    /// There are many syscalls who satisfy these two requirements.
    /// Records an `fcntl` that writes its result through its argument
    /// (`crate::event::fcntl_output`): its status, and on success the bytes it
    /// wrote, which replay writes back.
    async fn handle_fcntl_output<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: reverie::syscalls::Fcntl,
    ) -> Result<i64, Errno> {
        use reverie::syscalls::MemoryAccess;
        let (buf, len) = crate::event::fcntl_output(&call)
            .unwrap_or_else(|| unreachable!("fcntl output recording called for {call:?}"));
        let result = guest.inject(call).await;
        self.record_event(guest, result.map(SyscallEvent::Return));
        if result.is_ok() {
            let mut bytes = vec![0; len];
            let read = buf
                .ok_or(Errno::EFAULT)
                .and_then(|buf| guest.memory().read_exact(buf, &mut bytes));
            self.record_event(guest, read.map(|()| SyscallEvent::Bytes(bytes)));
        }
        result
    }

    async fn handle_simple<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(guest, result.map(SyscallEvent::Return));

        result
    }
}

#[cfg(test)]
mod exec_path_tests {
    use super::*;

    #[test]
    fn process_relative_magic_paths_are_classified_without_controller_lookup() {
        for path in [
            b"/proc/self/exe".as_slice(),
            b"/proc/thread-self/exe".as_slice(),
            b"/proc/self/fd/7".as_slice(),
            b"/dev/fd/7".as_slice(),
        ] {
            assert!(exec_request_uses_live_magic_path(path));
        }
        for path in [
            b"/proc/123/exe".as_slice(),
            b"/tmp/proc/self/exe".as_slice(),
            b"relative/proc/self/exe".as_slice(),
        ] {
            assert!(!exec_request_uses_live_magic_path(path));
        }
    }
}

#[cfg(test)]
mod mkdir_probe_tests {
    use super::*;

    #[test]
    fn resource_and_permission_failures_are_not_classified_as_non_directories() {
        for error in [Errno::EMFILE, Errno::ENFILE, Errno::EACCES, Errno::EPERM] {
            assert_eq!(classify_mkdir_probe(Err(error)), Err(error));
        }
    }

    #[test]
    fn file_and_symlink_probe_errors_are_not_directories() {
        for error in [Errno::ENOENT, Errno::ENOTDIR, Errno::ELOOP] {
            assert_eq!(
                classify_mkdir_probe(Err(error)),
                Ok(MkdirProbeDisposition::NotDirectory)
            );
        }
    }

    #[test]
    fn probe_close_must_succeed_exactly() {
        assert_eq!(require_mkdir_probe_closed(Ok(0)), Ok(()));
        assert_eq!(require_mkdir_probe_closed(Ok(1)), Err(Errno::EIO));
        assert_eq!(
            require_mkdir_probe_closed(Err(Errno::EINTR)),
            Err(Errno::EINTR)
        );
    }
}

#[cfg(test)]
mod finish_tests {
    use super::*;

    /// A handle that sends nothing: exiting a thread needs no RPC.
    struct NoRpc(detcore::Config);

    #[reverie::tool]
    impl GlobalRPC<detcore::GlobalState> for NoRpc {
        async fn send_rpc(
            &self,
            _message: <detcore::GlobalState as GlobalTool>::Request,
        ) -> <detcore::GlobalState as GlobalTool>::Response {
            unreachable!("a thread's exit sends no RPC")
        }

        fn config(&self) -> &detcore::Config {
            &self.0
        }
    }

    /// The status masks of a thread that catches `caught` and ignores
    /// `ignored`, as `/proc/<tid>/status` lists them.
    fn status(caught: &[i32], ignored: &[i32]) -> String {
        let mask = |signals: &[i32]| {
            signals
                .iter()
                .fold(0u64, |mask, signal| mask | 1 << (signal - 1))
        };
        format!(
            "Name:\tguest\nSigBlk:\t0000000000000000\nSigIgn:\t{:016x}\nSigCgt:\t{:016x}\n",
            mask(ignored),
            mask(caught)
        )
    }

    /// A signal's delivery ends the thread's group next only when it is
    /// neither caught nor ignored and its default action terminates.
    #[test]
    fn only_an_uncaught_terminating_signal_ends_the_group_by_default() {
        assert_eq!(
            ends_by_default(libc::SIGSEGV, &status(&[], &[])),
            Some(true)
        );
        assert_eq!(
            ends_by_default(libc::SIGRTMIN() + 1, &status(&[], &[])),
            Some(true)
        );
        assert_eq!(
            ends_by_default(libc::SIGSYS, &status(&[libc::SIGSYS], &[])),
            Some(false)
        );
        assert_eq!(
            ends_by_default(libc::SIGTERM, &status(&[], &[libc::SIGTERM])),
            Some(false)
        );
        assert_eq!(
            ends_by_default(libc::SIGCHLD, &status(&[], &[])),
            Some(false)
        );
        assert_eq!(
            ends_by_default(libc::SIGTSTP, &status(&[], &[])),
            Some(false)
        );
        assert_eq!(ends_by_default(libc::SIGSEGV, "Name:\tguest\n"), None);
    }

    /// An exit status is an orderly exit. A signal is how a thread ended
    /// only when its fatal delivery was the last thing the recorder saw of
    /// the thread, or, for a thread its group exit ended, of exactly one other
    /// thread; the record names the thread it was delivered to.
    #[test]
    fn a_thread_whose_last_sight_was_a_fatal_delivery_has_an_ending() {
        let (faulted, waiter) = (7, 8);
        let stream = EventStreamId::root();
        let delivery = |signal| FatalDelivery {
            signal,
            code: Some(1),
            stream: stream.clone(),
        };
        let mut deliveries = FatalDeliveries::default();
        assert_eq!(
            deliveries.ending(waiter, ExitStatus::Exited(3)),
            Some(ThreadEnding::Exited(3))
        );
        let segv = ExitStatus::Signaled(reverie::Signal::SIGSEGV, true);
        assert_eq!(deliveries.ending(waiter, segv), None);
        deliveries.delivered(faulted, Some(delivery(libc::SIGSEGV)));
        let ending = Some(ThreadEnding::Signaled {
            signal: libc::SIGSEGV,
            code: Some(1),
            delivered_to: stream.clone(),
        });
        assert_eq!(deliveries.ending(waiter, segv), ending);
        assert_eq!(deliveries.ending(faulted, segv), ending);
        // SIGKILL takes no delivery stop: a death by it is not this one.
        let kill = ExitStatus::Signaled(reverie::Signal::SIGKILL, false);
        assert_eq!(deliveries.ending(faulted, kill), None);
        assert_eq!(deliveries.ending(waiter, kill), None);
    }

    /// A SIGSYS the guest handled, then a death by SIGSYS that took no
    /// delivery stop (a seccomp kill's forced, SA_IMMUTABLE SIGSYS): the
    /// handled delivery was never a fatal one, so neither the killed thread
    /// nor its waiters get an ending, and replay refuses rather than parks.
    #[test]
    fn a_handled_sigsys_then_a_death_with_no_stop_has_no_ending() {
        let (killed, waiter) = (7, 8);
        let handled = ends_by_default(libc::SIGSYS, &status(&[libc::SIGSYS], &[])) == Some(true);
        let mut deliveries = FatalDeliveries::default();
        deliveries.delivered(
            killed,
            handled.then(|| FatalDelivery {
                signal: libc::SIGSYS,
                code: None,
                stream: EventStreamId::root(),
            }),
        );
        let sys = ExitStatus::Signaled(reverie::Signal::SIGSYS, true);
        assert_eq!(deliveries.ending(killed, sys), None);
        assert_eq!(deliveries.ending(waiter, sys), None);
    }

    /// A fatal delivery the thread outlived, its handler having run and the
    /// thread gone on to a later event, is forgotten: a later death of that
    /// thread, or of its group, by the same signal, has no ending.
    #[test]
    fn a_delivery_a_later_event_followed_is_no_evidence_of_a_death() {
        let (thread, waiter) = (7, 8);
        let mut deliveries = FatalDeliveries::default();
        deliveries.delivered(
            thread,
            Some(FatalDelivery {
                signal: libc::SIGTERM,
                code: Some(0),
                stream: EventStreamId::root(),
            }),
        );
        deliveries.event(thread);
        let term = ExitStatus::Signaled(reverie::Signal::SIGTERM, false);
        assert_eq!(deliveries.ending(thread, term), None);
        assert_eq!(deliveries.ending(waiter, term), None);
    }

    /// A thread whose streams cannot be written out in full when it exits
    /// leaves its recording marked as one replay must refuse, with the reason.
    /// Its writer is on `/dev/full`, whose every write fails with ENOSPC, as a
    /// full disk's would at the stream's last write.
    #[test]
    fn a_thread_stream_that_cannot_be_written_in_full_refuses_replay() {
        let full = || {
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap()
        };
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data.path().join("thread")).unwrap();
        let mut state = RecorderThreadState {
            events: EventWriter::for_test(full(), full())
                .with_terminal_record(data.path(), &EventStreamId::root().child(3)),
            stream_id: EventStreamId::root().child(3),
            child_stream_ids: ChildEventStreamIds::default(),
            pending_exec: None,
            bootstrapped: true,
            outputs: ContainerOutputs::default(),
            output_descriptions: Default::default(),
            interrupted_poll: None,
            fatal_pending: false,
        };
        state
            .push_event(Event {
                event: Ok(SyscallEvent::Return(0)),
            })
            .unwrap();
        assert_eq!(detcore::detlog::replay_refusal(), None);
        let recorder = Recorder::default();
        let rpc = NoRpc(detcore::Config::default());
        let exit = recorder.on_exit_thread(Tid::from_raw(7), &rpc, state, ExitStatus::Exited(0));
        let mut exit = std::pin::pin!(exit);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(result) = exit.as_mut().poll(&mut context) else {
            panic!("exiting a thread waited");
        };
        result.unwrap();
        let refusal = detcore::detlog::replay_refusal().expect("no replay refusal recorded");
        let stream = EventStreamId::root().child(3);
        assert!(
            refusal.contains(&format!(
                "thread stream {stream} could not be written in full"
            )) && refusal.contains("No space left on device"),
            "{refusal}"
        );
        // Nor does the thread get a terminal record, though it exited in
        // order, so replay parks none of its calls.
        let end = stream.file_path(data.path(), ".end");
        assert!(
            !end.exists(),
            "a terminal record was written after a failed write"
        );
    }
}
