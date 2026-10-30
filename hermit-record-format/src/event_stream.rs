/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::fs;
use std::io;
use std::io::BufRead as _;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use reverie::Errno;
use reverie::syscalls::Displayable;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::event::Event;

/// Stable event-stream identity derived from the guest process tree.
///
/// Each component after the root is the parent's deterministic child ordinal.
/// Unlike a process-local counter, this remains collision-free when Reverie
/// forks the tool itself and each process receives its own tool instance.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventStreamId(Vec<u64>);

impl EventStreamId {
    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn child(&self, ordinal: u64) -> Self {
        let mut path = self.0.clone();
        path.push(ordinal);
        Self(path)
    }

    /// The path of one of this stream's files in the recording at `path`:
    /// its events for `suffix` "", its syscall records for ".debug", and its
    /// terminal record (`StreamEnd`) for ".end".
    pub fn file_path(&self, path: &Path, suffix: &str) -> PathBuf {
        path.join("thread")
            .join(format!("{}{suffix}", self.file_name()))
    }

    fn file_name(&self) -> String {
        let mut identity = b"hermit-event-stream-v1\0".to_vec();
        for ordinal in &self.0 {
            identity.extend_from_slice(&ordinal.to_be_bytes());
        }
        format!("stream-{}", detcore::Digest::new(&identity))
    }
}

impl fmt::Display for EventStreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", detcore::ROOT_DETPID.as_raw())?;
        for ordinal in &self.0 {
            write!(f, ".{ordinal}")?;
        }
        Ok(())
    }
}

/// Allocates child ordinals within one deterministic parent stream.
///
/// The counter belongs to the parent thread state rather than the Recorder or
/// Replayer process instance. When Reverie forks a tool process, the child
/// receives its already-assigned stream identity and a fresh descendant
/// counter, while the continuing parent retains the incremented counter.
#[derive(Default)]
pub struct ChildEventStreamIds(AtomicU64);

impl ChildEventStreamIds {
    pub fn next(&self, parent: &EventStreamId) -> EventStreamId {
        let ordinal = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("record/replay child stream ordinal overflow"));
        parent.child(ordinal)
    }
}

#[derive(Serialize, Deserialize)]
struct EventStreamHeader {
    stream_id: EventStreamId,
}

fn decode_stream_header(
    reader: &mut io::BufReader<fs::File>,
    expected: &EventStreamId,
) -> io::Result<()> {
    let header: EventStreamHeader =
        bincode::serde::decode_from_std_read(reader, bincode::config::legacy()).map_err(
            |error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid event-stream identity header: {error}"),
                )
            },
        )?;
    if header.stream_id != *expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "event-stream identity mismatch: expected {expected}, found {}",
                header.stream_id
            ),
        ));
    }
    Ok(())
}

fn encode_stream_header(
    writer: &mut io::BufWriter<fs::File>,
    stream_id: &EventStreamId,
) -> io::Result<u64> {
    bincode::serde::encode_into_std_write(
        EventStreamHeader {
            stream_id: stream_id.clone(),
        },
        writer,
        bincode::config::legacy(),
    )
    .map(|length| length as u64)
    .map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to encode event-stream identity header: {error}"),
        )
    })
}

impl Serialize for ChildEventStreamIds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0.load(Ordering::Relaxed))
    }
}

impl<'de> Deserialize<'de> for ChildEventStreamIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self(AtomicU64::new(u64::deserialize(deserializer)?)))
    }
}

/// What the recorder knew of one thread's streams when the thread exited in
/// order: how much it wrote, and the call the thread was in. A thread whose
/// recording ends inside a call is parked in replay only on this evidence
/// (`EventReader::check_unreturned_call`): a stream that merely ends at a
/// record boundary looks the same whether the thread died inside its call or
/// the recording was cut short there.
///
/// The recorder writes it beside the thread's streams, as
/// `stream-<digest>.end` (`EventWriter::finish`), only when it saw how the
/// thread ended (`ThreadEnding`): an orderly exit, by `exit`, `exit_group` or
/// another thread's `execve` (an exit status), or a fatal signal whose
/// delivery stop was the last thing it saw of the thread it was delivered
/// to; and only after both stream files were written out, synced and closed
/// without error. A death that took no delivery stop (SIGKILL, an
/// SA_IMMUTABLE signal), a recording whose recorder died, and a stream whose
/// last write failed have none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEnd {
    /// The stream this describes.
    pub stream_id: EventStreamId,
    /// The syscall records written, and the syscall file's length.
    pub syscalls: u64,
    pub syscall_bytes: u64,
    /// The events written, and the event file's length.
    pub events: u64,
    pub event_bytes: u64,
    /// How the thread ended.
    pub ending: ThreadEnding,
    /// The number of the syscall record, counted from 1 as
    /// `EventReader::count` counts, of the call the thread was in when it
    /// exited: the recorder wrote its record when the call began, and its
    /// handler never returned, so none of its events were written. `None`
    /// when no call of the thread was in the recorder: a thread that died in
    /// a call before the recorder saw it has none either.
    pub call_in_flight: Option<u64>,
}

/// How a thread whose stream has a terminal record ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadEnding {
    /// It exited in order, with this exit code.
    Exited(i32),
    /// A fatal signal ended its thread group: the signal, its `si_code` when
    /// the stop's siginfo could be read, and the stream of the thread it was
    /// delivered to, whose delivery stop, its default action ending the group
    /// next, was the last thing the recorder saw of that thread. Linux ends
    /// every other thread of the group with that signal (`do_group_exit`,
    /// kernel/exit.c).
    Signaled {
        signal: i32,
        code: Option<i32>,
        delivered_to: EventStreamId,
    },
}

/// The first bytes of a `StreamEnd` file, before its encoding. A file without
/// them is refused, as is one in a later format: readers refuse what they do
/// not define.
const STREAM_END_MAGIC: &[u8] = b"hermit-stream-end-v1\0";

impl StreamEnd {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut bytes = STREAM_END_MAGIC.to_vec();
        bincode::serde::encode_into_std_write(self, &mut bytes, bincode::config::legacy())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let body = bytes
            .strip_prefix(STREAM_END_MAGIC)
            .ok_or("it is not a terminal record this replayer defines")?;
        let (end, length): (Self, usize) =
            bincode::serde::decode_from_slice(body, bincode::config::legacy())
                .map_err(|error| format!("it cannot be decoded: {error}"))?;
        if length != body.len() {
            return Err(format!(
                "it holds {} bytes past its record",
                body.len() - length
            ));
        }
        Ok(end)
    }
}

/// An event to help with debugging, but is not actually necessary for the
/// functionality of record/replay.
#[derive(Debug, Serialize, Deserialize)]
pub struct DebugEvent {
    /// The raw syscall.
    syscall: (Sysno, SyscallArgs),

    /// The pretty, displayable version of the syscall.
    pretty: String,

    /// Exec pathname bytes and lookup context. Raw syscall registers only hold
    /// a pointer, so this snapshot prevents a changed failed exec request from
    /// silently consuming an event that happened to return the same errno.
    exec_request: Option<DebugExecRequest>,
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DebugExecRequest {
    dirfd: libc::c_int,
    path: Result<Vec<u8>, Errno>,
    flags: libc::c_int,
}

impl DebugEvent {
    /// Constructs a new `DebugEvent`.
    pub fn new<M: MemoryAccess>(syscall: Syscall, memory: &M) -> Self {
        let exec_request = match syscall {
            Syscall::Execve(call) => {
                let call: reverie::syscalls::Execveat = call.into();
                Some(DebugExecRequest {
                    dirfd: call.dirfd(),
                    path: call
                        .path()
                        .ok_or(Errno::EFAULT)
                        .and_then(|path| path.read(memory))
                        .map(|path| path.as_os_str().as_bytes().to_vec()),
                    flags: call.flags(),
                })
            }
            Syscall::Execveat(call) => Some(DebugExecRequest {
                dirfd: call.dirfd(),
                path: call
                    .path()
                    .ok_or(Errno::EFAULT)
                    .and_then(|path| path.read(memory))
                    .map(|path| path.as_os_str().as_bytes().to_vec()),
                flags: call.flags(),
            }),
            _ => None,
        };
        Self {
            syscall: syscall.into_parts(),
            pretty: format!("{}", syscall.display(memory)),
            exec_request,
        }
    }

    /// Returns the syscall associated with this debug event.
    pub fn syscall(&self) -> Syscall {
        Syscall::from_raw(self.syscall.0, self.syscall.1)
    }

    pub fn exec_request_matches(&self, other: &Self) -> bool {
        self.exec_request == other.exec_request
    }

    /// A debug event naming SYSNO with no arguments. Test support only, for the
    /// tests here and in the hermit library.
    #[doc(hidden)]
    pub fn for_test(sysno: Sysno, pretty: &str) -> Self {
        Self {
            syscall: (sysno, SyscallArgs::new(0, 0, 0, 0, 0, 0)),
            pretty: pretty.to_owned(),
            exec_request: None,
        }
    }
}

impl fmt::Display for DebugEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.pretty)
    }
}

/// The number of argument registers the x86-64 kernel actually reads for
/// `sysno`. Registers beyond this are not part of the syscall's ABI and can hold
/// arbitrary leftover values in the guest, so they must be excluded when
/// comparing a replayed syscall against its recorded counterpart.
///
/// Reverie stores all six raw registers in every typed syscall and derives
/// `PartialEq` over them, so a naive `Syscall == Syscall` compares unused
/// registers too. That produces false desyncs for any syscall with fewer than
/// six arguments (e.g. `statfs`, which uses two).
///
/// Returns `None` for syscalls without an entry; callers then fall back to
/// comparing all six registers (the conservative pre-existing behavior). Only
/// syscalls that record/replay subscribes to (see [`crate::subscriptions::record_and_replay`])
/// flow through the comparator, so those are the ones covered here.
///
/// These are true kernel arities, which for a few syscalls exceed the number of
/// typed fields reverie exposes (`open`, `openat`, `ioctl`, `socket`, and
/// `fcntl` fold meaningful arguments into typed enums). Using the kernel arity
/// -- not the typed field count -- guarantees we never zero a meaningful
/// argument and therefore never mask a genuine divergence.
fn kernel_arg_count(sysno: Sysno) -> Option<u8> {
    // Every name is qualified: a bare name that the target's table lacks
    // would be a catch-all binding, not a pattern.
    Some(match sysno {
        Sysno::close | Sysno::chdir | Sysno::fchdir | Sysno::dup => 1,
        Sysno::getcwd
        | Sysno::set_robust_list
        | Sysno::fstat
        | Sysno::clock_gettime
        | Sysno::clock_getres
        | Sysno::gettimeofday
        | Sysno::settimeofday
        | Sysno::statfs
        | Sysno::fstatfs
        | Sysno::ftruncate
        | Sysno::kill
        | Sysno::listen
        | Sysno::shutdown
        | Sysno::rt_sigpending => 2,
        Sysno::mprotect
        | Sysno::read
        | Sysno::readv
        | Sysno::write
        | Sysno::writev
        | Sysno::lseek
        | Sysno::getdents64
        | Sysno::dup3
        | Sysno::ioctl
        | Sysno::socket
        | Sysno::fcntl
        | Sysno::connect
        | Sysno::sendmsg
        | Sysno::getpeername
        | Sysno::getsockname
        | Sysno::getrandom
        | Sysno::unlinkat
        | Sysno::execve
        | Sysno::close_range
        | Sysno::tgkill
        | Sysno::faccessat
        | Sysno::accept
        | Sysno::bind => 3,
        Sysno::pread64
        | Sysno::pwrite64
        | Sysno::fadvise64
        | Sysno::openat
        | Sysno::faccessat2
        | Sysno::accept4 => 4,
        Sysno::statx
        | Sysno::pwritev
        | Sysno::preadv
        | Sysno::ppoll
        | Sysno::setsockopt
        | Sysno::getsockopt
        | Sysno::execveat
        | Sysno::prctl => 5,
        Sysno::recvfrom | Sysno::sendto | Sysno::pwritev2 | Sysno::preadv2 | Sysno::mmap => 6,
        #[cfg(target_arch = "x86_64")]
        Sysno::time | Sysno::unlink | Sysno::rmdir => 1,
        #[cfg(target_arch = "x86_64")]
        Sysno::access
        | Sysno::stat
        | Sysno::lstat
        | Sysno::dup2
        | Sysno::mkdir
        | Sysno::rename
        | Sysno::link
        | Sysno::symlink
        | Sysno::chmod => 2,
        #[cfg(target_arch = "x86_64")]
        Sysno::getdents
        | Sysno::poll
        | Sysno::readlink
        | Sysno::open
        | Sysno::chown
        | Sysno::lchown
        | Sysno::mknod => 3,
        #[cfg(target_arch = "x86_64")]
        Sysno::newfstatat => 4,
        #[cfg(target_arch = "aarch64")]
        Sysno::fstatat => 4,
        _ => return None,
    })
}

/// Returns `syscall` with any argument registers beyond its kernel arity zeroed.
/// This makes the record/replay desync comparison ignore unused registers, which
/// are not part of the syscall and may legitimately differ between record and
/// replay. Syscalls without a known arity are returned unchanged (all six
/// registers still compared).
pub fn normalize_unused_args(syscall: Syscall) -> Syscall {
    let (sysno, args) = syscall.into_parts();
    let Some(used) = kernel_arg_count(sysno) else {
        return syscall;
    };
    let mut raw = [
        args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
    ];
    for reg in raw.iter_mut().skip(usize::from(used)) {
        *reg = 0;
    }
    Syscall::from_raw(
        sysno,
        SyscallArgs::new(raw[0], raw[1], raw[2], raw[3], raw[4], raw[5]),
    )
}

/// A stream of syscall events.
#[derive(Serialize, Deserialize)]
pub struct EventReader {
    // The file where events are stored.
    //
    // NOTE: This field isn't serializable/deserializable, so we have to skip it
    // for now. With an in-guest backend, we'd need to implement this manually
    // to support state migration.
    #[serde(skip, default = "default_reader")]
    reader: io::BufReader<fs::File>,

    // The file where raw syscalls are stored. This is used for detecting
    // desynchronization bugs. This is stored in a separate file so that we can
    // easily turn this on or off to shift the balance on debuggability and
    // performance.
    #[serde(skip, default = "default_reader")]
    debug_events: io::BufReader<fs::File>,

    // The number of events read so far. Useful for debugging purposes.
    pub count: u64,

    /// The number of events (not syscall records) read so far.
    #[serde(default)]
    pub events_read: u64,

    /// The stream's identity and the path of its event file, without the
    /// `.debug` and `.end` extensions of its other files.
    #[serde(skip)]
    stream_id: EventStreamId,
    #[serde(skip)]
    path: PathBuf,
}

fn default_reader() -> io::BufReader<fs::File> {
    unimplemented!("Serialization is not yet implemented")
}

impl EventReader {
    /// Opens an existing event stream.
    pub fn open(path: &Path, stream_id: &EventStreamId) -> io::Result<Self> {
        let file_name = stream_id.file_name();
        let events_path = path.join("thread").join(&file_name);
        let mut reader = io::BufReader::new(fs::File::open(&events_path)?);
        let mut debug_events = io::BufReader::new(fs::File::open(
            path.join("thread").join(format!("{file_name}.debug")),
        )?);
        decode_stream_header(&mut reader, stream_id)?;
        decode_stream_header(&mut debug_events, stream_id)?;
        Ok(Self {
            reader,
            debug_events,
            count: 0,
            events_read: 0,
            stream_id: stream_id.clone(),
            path: events_path,
        })
    }

    /// Checks that the recording says this thread exited inside the call
    /// being replayed, with nothing of it recorded, before replay parks the
    /// thread there: that the stream has a terminal record (`StreamEnd`), so
    /// the thread exited in order and its streams were written out in full,
    /// that it counts exactly the syscall records and events read (both
    /// streams have been read to their end), and that it names this call as
    /// the one the thread was in. `recorded` says whether the call has a
    /// syscall record, the last one read; a call without one began after the
    /// recorder's last record, and the terminal record must name no call in
    /// flight. Returns the terminal record, or why the thread cannot be parked.
    pub fn check_unreturned_call(&self, recorded: bool) -> Result<StreamEnd, String> {
        let stream = &self.stream_id;
        let end_path = with_suffix(&self.path, ".end");
        let bytes = match fs::read(&end_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(format!(
                    "the recording has no terminal record for thread stream {stream}. The \
                     recorder writes one only when it saw how the thread ended, an orderly exit \
                     (exit, exit_group or another thread's execve) or the delivery stop of the \
                     fatal signal its group died of, and both its stream files were written in \
                     full, so this thread died of a signal that took no delivery stop (SIGKILL), \
                     its recording was cut short, or its last write failed"
                ));
            }
            Err(error) => {
                return Err(format!(
                    "the terminal record of thread stream {stream} cannot be read: {error}"
                ));
            }
        };
        let end = StreamEnd::decode(&bytes).map_err(|reason| {
            format!("the terminal record of thread stream {stream} is refused: {reason}")
        })?;
        if end.stream_id != *stream {
            return Err(format!(
                "the terminal record of thread stream {stream} names stream {}",
                end.stream_id
            ));
        }
        let lengths = (
            fs::metadata(with_suffix(&self.path, ".debug")).map(|metadata| metadata.len()),
            fs::metadata(&self.path).map(|metadata| metadata.len()),
        );
        let (Ok(syscall_bytes), Ok(event_bytes)) = lengths else {
            return Err(format!(
                "the stream files of thread stream {stream} cannot be measured"
            ));
        };
        if (end.syscalls, end.events, end.syscall_bytes, end.event_bytes)
            != (self.count, self.events_read, syscall_bytes, event_bytes)
        {
            return Err(format!(
                "the recording of thread stream {stream} was cut short: its terminal record \
                 counts {} syscall records and {} events in {} and {} bytes, and replay found \
                 {} and {} in {} and {} bytes",
                end.syscalls,
                end.events,
                end.syscall_bytes,
                end.event_bytes,
                self.count,
                self.events_read,
                syscall_bytes,
                event_bytes
            ));
        }
        let this_call = recorded.then_some(self.count);
        if let ThreadEnding::Signaled {
            signal,
            delivered_to,
            ..
        } = &end.ending
            && delivered_to == stream
        {
            return Err(format!(
                "thread stream {stream} died of signal {signal} delivered to it, by its terminal \
                 record, which replay should have delivered there, and replay is past it, in {}",
                match this_call {
                    Some(call) => format!("syscall record {call}"),
                    None => "a call the recorder never saw".to_owned(),
                }
            ));
        }
        if end.call_in_flight != this_call {
            let named = |call: Option<u64>| match call {
                Some(call) => format!("syscall record {call}"),
                None => "no call the recorder saw".to_owned(),
            };
            return Err(format!(
                "thread stream {stream} exited inside {}, by its terminal record, and replay \
                 is in {}",
                named(end.call_in_flight),
                named(this_call)
            ));
        }
        Ok(end)
    }

    /// Reads the next event from the stream. Returns an error if there are no
    /// more events to consume.
    pub fn next_event(&mut self) -> Result<Event, bincode::error::DecodeError> {
        let event =
            bincode::serde::decode_from_std_read(&mut self.reader, bincode::config::legacy())?;
        self.events_read += 1;
        Ok(event)
    }

    /// Reads the next event from the stream, or `None` when the stream ends
    /// cleanly, at an event boundary. A recorder writes a call's events when
    /// the call returns, so a stream that ends there was cut short by the
    /// thread's death inside its last call. A stream that ends inside an event
    /// is an error, as with `next_event`.
    pub fn next_event_or_end(&mut self) -> Result<Option<Event>, bincode::error::DecodeError> {
        match self.reader.fill_buf() {
            Ok([]) => Ok(None),
            Ok(_) => self.next_event().map(Some),
            Err(inner) => Err(bincode::error::DecodeError::Io {
                inner,
                additional: 0,
            }),
        }
    }

    /// Whether every recorded syscall in the syscall stream has been read: the
    /// call being replayed was this thread's last.
    pub fn syscalls_exhausted(&mut self) -> io::Result<bool> {
        Ok(self.debug_events.fill_buf()?.is_empty())
    }

    /// Whether every recorded event has been read.
    pub fn events_exhausted(&mut self) -> io::Result<bool> {
        Ok(self.reader.fill_buf()?.is_empty())
    }

    /// Reads the next syscall from the syscall stream, or `None` when the stream
    /// ends cleanly, at a record boundary: the thread died in a call before the
    /// recorder saw it. A stream that ends inside a record is an error.
    pub fn next_debug_event_or_end(
        &mut self,
    ) -> Result<Option<DebugEvent>, bincode::error::DecodeError> {
        match self.debug_events.fill_buf() {
            Ok([]) => Ok(None),
            Ok(_) => self.next_debug_event().map(Some),
            Err(inner) => Err(bincode::error::DecodeError::Io {
                inner,
                additional: 0,
            }),
        }
    }

    /// Reads the next syscall from the syscall stream.
    pub fn next_debug_event(&mut self) -> Result<DebugEvent, bincode::error::DecodeError> {
        let debug_event = bincode::serde::decode_from_std_read(
            &mut self.debug_events,
            bincode::config::legacy(),
        )?;
        self.count += 1;
        Ok(debug_event)
    }
}

impl Default for EventReader {
    fn default() -> Self {
        panic!("Thread state should be explicitly initialized in init_thread_state")
    }
}

/// A stream of syscall events.
#[derive(Serialize, Deserialize)]
pub struct EventWriter {
    // The file where events are stored.
    //
    // NOTE: This field isn't serializable/deserializable, so we have to skip it
    // for now. With an in-guest backend, we'd need to implement this manually
    // to support state migration.
    #[serde(skip, default = "default_writer")]
    writer: io::BufWriter<fs::File>,

    // The file where syscalls are stored. This is used for debugging purposes.
    #[serde(skip, default = "default_writer")]
    debug_events: io::BufWriter<fs::File>,

    /// What the stream files hold so far, for the terminal record
    /// (`StreamEnd`): the syscall records and events written, and their
    /// files' lengths.
    #[serde(default)]
    syscalls: u64,
    #[serde(default)]
    syscall_bytes: u64,
    #[serde(default)]
    events: u64,
    #[serde(default)]
    event_bytes: u64,
    /// Whether the last syscall record's call is still in its handler: set
    /// by the record, cleared by `call_returned`.
    #[serde(default)]
    call_in_flight: bool,

    /// The stream's identity and the path of its event file, where the
    /// terminal record goes beside it. `None` for a test writer.
    #[serde(skip)]
    end: Option<(EventStreamId, PathBuf)>,
}

fn default_writer() -> io::BufWriter<fs::File> {
    unimplemented!("Serialization is not yet implemented")
}

impl EventWriter {
    /// Creates a new event stream.
    pub fn create(path: &Path, stream_id: &EventStreamId) -> io::Result<Self> {
        let path = path.join("thread");

        fs::create_dir_all(&path)?;
        let file_name = stream_id.file_name();

        let create = |path| {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
        };

        let events_path = path.join(&file_name);
        let mut writer = io::BufWriter::new(create(events_path.clone())?);
        let mut debug_events = io::BufWriter::new(create(path.join(format!("{file_name}.debug")))?);
        let event_bytes = encode_stream_header(&mut writer, stream_id)?;
        let syscall_bytes = encode_stream_header(&mut debug_events, stream_id)?;
        Ok(Self {
            writer,
            debug_events,
            syscalls: 0,
            syscall_bytes,
            events: 0,
            event_bytes,
            call_in_flight: false,
            end: Some((stream_id.clone(), events_path)),
        })
    }

    /// Writes an event to the end of the stream.
    pub fn push_event(&mut self, event: Event) -> Result<(), bincode::error::EncodeError> {
        let length = bincode::serde::encode_into_std_write(
            &event,
            &mut self.writer,
            bincode::config::legacy(),
        )?;
        self.events += 1;
        self.event_bytes += length as u64;
        Ok(())
    }

    /// Writes a debug event to the end of the stream: the record of a call
    /// that begins, which is in flight until `call_returned`.
    pub fn push_debug_event(
        &mut self,
        event: DebugEvent,
    ) -> Result<(), bincode::error::EncodeError> {
        let length = bincode::serde::encode_into_std_write(
            &event,
            &mut self.debug_events,
            bincode::config::legacy(),
        )?;
        self.syscalls += 1;
        self.syscall_bytes += length as u64;
        self.call_in_flight = true;
        Ok(())
    }

    /// The handler of the last recorded call returned (or the exec it made
    /// was committed), so the call is no longer in flight.
    pub fn call_returned(&mut self) {
        self.call_in_flight = false;
    }

    /// Writes out what both streams still buffer and closes their files,
    /// reporting the first error. Dropping the writer instead would flush and
    /// close them too, but would ignore every error, so a stream cut short by a
    /// full disk (ENOSPC), a file-size limit or an I/O error at its last write,
    /// or by an error close(2) reports for writes it could not complete, went
    /// unnoticed until replay failed on it.
    ///
    /// `ending` is how the thread ended, when the recorder saw it, and `None`
    /// otherwise. Only then, and only once both files were written out,
    /// synced and closed without error, the stream's terminal record
    /// (`StreamEnd`) is written beside it, synced under a temporary name and
    /// renamed into place, so that replay never finds a partial one, nor a
    /// record whose streams a crash of the host could still cut short.
    pub fn finish(self, ending: Option<ThreadEnding>) -> io::Result<()> {
        let Self {
            writer,
            debug_events,
            syscalls,
            syscall_bytes,
            events,
            event_bytes,
            call_in_flight,
            end,
        } = self;
        let terminal = ending.zip(end);
        let sync = terminal.is_some();
        let written = finish_file(writer, sync);
        let debug_written = finish_file(debug_events, sync);
        written.and(debug_written)?;
        let Some((ending, (stream_id, events_path))) = terminal else {
            return Ok(());
        };
        let record = StreamEnd {
            stream_id,
            syscalls,
            syscall_bytes,
            events,
            event_bytes,
            ending,
            call_in_flight: call_in_flight.then_some(syscalls),
        };
        let path = with_suffix(&events_path, ".end");
        let temporary = with_suffix(&events_path, ".end.partial");
        // The temporary file is synced and closed with its close checked
        // before the rename: a close that reports an error for writes it could
        // not complete leaves no terminal record.
        let result = record
            .encode()
            .and_then(|bytes| {
                use std::io::Write as _;
                let mut file = fs::File::create(&temporary)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                close_file(file)
            })
            .and_then(|()| fs::rename(&temporary, &path));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    /// An event writer over `events` and `debug_events`, with no stream
    /// header. Test support only, for the tests here and in the recorder.
    #[doc(hidden)]
    pub fn for_test(events: fs::File, debug_events: fs::File) -> Self {
        Self {
            writer: io::BufWriter::new(events),
            debug_events: io::BufWriter::new(debug_events),
            syscalls: 0,
            syscall_bytes: 0,
            events: 0,
            event_bytes: 0,
            call_in_flight: false,
            end: None,
        }
    }

    /// This test writer with its terminal record written for stream
    /// `stream_id` of the recording at `path`, as a created writer's is. Test
    /// support only.
    #[doc(hidden)]
    pub fn with_terminal_record(mut self, path: &Path, stream_id: &EventStreamId) -> Self {
        self.end = Some((
            stream_id.clone(),
            path.join("thread").join(stream_id.file_name()),
        ));
        self
    }
}

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Flushes `writer`, syncs its file to storage when `sync` asks, and closes
/// it, reporting an error from any. A failed flush leaves the rest unwritten:
/// the writer is taken apart rather than dropped, since its drop would write
/// the rest again and could leave a complete file behind a reported error.
fn finish_file(writer: io::BufWriter<fs::File>, sync: bool) -> io::Result<()> {
    let file = match writer.into_inner() {
        Ok(file) => file,
        Err(error) => {
            let (error, writer) = error.into_parts();
            let (_file, _unwritten) = writer.into_parts();
            return Err(error);
        }
    };
    if sync {
        file.sync_all()?;
    }
    close_file(file)
}

/// Closes `file`, reporting the error close(2) reports, which dropping the
/// file would ignore.
fn close_file(file: fs::File) -> io::Result<()> {
    use std::os::fd::IntoRawFd as _;
    let fd = file.into_raw_fd();
    // SAFETY: `fd` came from `into_raw_fd`, so it is open and nothing else
    // owns it. Linux releases the descriptor before it flushes the file, so it
    // is released even when close fails, and an EINTR must not be retried
    // (`file_close_fd` before `filp_flush` in fs/open.c).
    if unsafe { libc::close(fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Default for EventWriter {
    fn default() -> Self {
        panic!("Thread state should be explicitly initialized in init_thread_state")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;

    use reverie::syscalls::Syscall;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::ChildEventStreamIds;
    use super::EventStreamId;
    use super::kernel_arg_count;
    use super::normalize_unused_args;

    fn raw(sysno: Sysno, args: SyscallArgs) -> Syscall {
        Syscall::from_raw(sysno, args)
    }

    /// `finish` writes out what a stream buffers and reports an error
    /// writing it, which dropping the writer would ignore: here `/dev/full`,
    /// whose every write fails with ENOSPC, as a full disk's would.
    #[test]
    fn finishing_a_stream_reports_the_error_of_its_last_write() {
        use super::DebugEvent;
        use super::EventReader;
        use super::EventWriter;
        use crate::event::Event;
        use crate::event::SyscallEvent;

        let dir = tempfile::tempdir().unwrap();
        let id = EventStreamId::root();
        let mut writer = EventWriter::create(dir.path(), &id).unwrap();
        writer
            .push_debug_event(DebugEvent::for_test(Sysno::read, "read"))
            .unwrap();
        writer
            .push_event(Event {
                event: Ok(SyscallEvent::Return(3)),
            })
            .unwrap();
        // Buffered so far: the files hold nothing yet.
        let path = dir.path().join("thread").join(id.file_name());
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        writer.finish(None).unwrap();
        let mut reader = EventReader::open(dir.path(), &id).unwrap();
        assert!(matches!(
            reader.next_event().unwrap(),
            Event {
                event: Ok(SyscallEvent::Return(3))
            }
        ));

        let full = || {
            fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap()
        };
        let mut writer = EventWriter::for_test(full(), full());
        writer
            .push_event(Event {
                event: Ok(SyscallEvent::Return(3)),
            })
            .unwrap();
        let error = writer.finish(None).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC), "{error}");
    }

    /// A thread that exits in order inside a call leaves a terminal record
    /// naming that call and counting both streams; replay parks it only when
    /// the record matches what it read (`check_unreturned_call`). Without
    /// the record (a signal ended the thread), with a stream cut at a record
    /// boundary, or with replay in another call, it is refused.
    #[test]
    fn a_terminal_record_names_the_call_a_thread_exited_in() {
        use super::DebugEvent;
        use super::EventReader;
        use super::EventWriter;
        use super::StreamEnd;
        use super::ThreadEnding;
        use crate::event::Event;
        use crate::event::SyscallEvent;

        let write = |dir: &std::path::Path, ending: Option<ThreadEnding>| {
            let id = EventStreamId::root().child(0);
            let mut writer = EventWriter::create(dir, &id).unwrap();
            writer
                .push_debug_event(DebugEvent::for_test(Sysno::read, "read"))
                .unwrap();
            writer
                .push_event(Event {
                    event: Ok(SyscallEvent::Return(3)),
                })
                .unwrap();
            writer.call_returned();
            // The last call, which never returned: its record, no event.
            writer
                .push_debug_event(DebugEvent::for_test(Sysno::poll, "poll"))
                .unwrap();
            writer.finish(ending).unwrap();
            id
        };
        let read_all = |dir: &std::path::Path, id: &EventStreamId| {
            let mut reader = EventReader::open(dir, id).unwrap();
            while reader.next_debug_event_or_end().unwrap().is_some() {}
            while reader.next_event_or_end().unwrap().is_some() {}
            reader
        };

        let dir = tempfile::tempdir().unwrap();
        let id = write(dir.path(), Some(ThreadEnding::Exited(0)));
        let path = dir.path().join("thread").join(id.file_name());
        let reader = read_all(dir.path(), &id);
        let end = reader.check_unreturned_call(true).unwrap();
        assert_eq!(
            end,
            StreamEnd {
                stream_id: id.clone(),
                syscalls: 2,
                syscall_bytes: fs::metadata(super::with_suffix(&path, ".debug"))
                    .unwrap()
                    .len(),
                events: 1,
                event_bytes: fs::metadata(&path).unwrap().len(),
                ending: ThreadEnding::Exited(0),
                call_in_flight: Some(2),
            }
        );
        // A call after the last record is not the one the thread exited in.
        let error = reader.check_unreturned_call(false).unwrap_err();
        assert!(error.contains("exited inside syscall record 2"), "{error}");

        // The syscall stream cut at the boundary before the poll's record:
        // replay finds one record where the terminal record counts two.
        let debug = super::with_suffix(&path, ".debug");
        let boundary = fs::metadata(&debug).unwrap().len()
            - bincode::serde::encode_to_vec(
                DebugEvent::for_test(Sysno::poll, "poll"),
                bincode::config::legacy(),
            )
            .unwrap()
            .len() as u64;
        fs::OpenOptions::new()
            .write(true)
            .open(&debug)
            .unwrap()
            .set_len(boundary)
            .unwrap();
        let error = read_all(dir.path(), &id)
            .check_unreturned_call(false)
            .unwrap_err();
        assert!(
            error.contains("was cut short") && error.contains("counts 2 syscall records"),
            "{error}"
        );

        // A thread whose last call returned before it exited names no call
        // in flight: replay may park it only in a call the recorder never saw.
        let returned = tempfile::tempdir().unwrap();
        let id = EventStreamId::root().child(0);
        let mut writer = EventWriter::create(returned.path(), &id).unwrap();
        writer
            .push_debug_event(DebugEvent::for_test(Sysno::read, "read"))
            .unwrap();
        writer
            .push_event(Event {
                event: Ok(SyscallEvent::Return(3)),
            })
            .unwrap();
        writer.call_returned();
        writer.finish(Some(ThreadEnding::Exited(0))).unwrap();
        let reader = read_all(returned.path(), &id);
        assert_eq!(
            reader.check_unreturned_call(false).unwrap().call_in_flight,
            None
        );
        let error = reader.check_unreturned_call(true).unwrap_err();
        assert!(error.contains("exited inside no call"), "{error}");

        // A thread waiting in its call when a fatal signal the recorder saw
        // delivered to another thread ended the group is parked on its record,
        // which names the signal; the thread the signal was delivered to died
        // of it there, so replay never parks it.
        let crashed = |delivered_to: EventStreamId| {
            let dir = tempfile::tempdir().unwrap();
            let ending = ThreadEnding::Signaled {
                signal: libc::SIGSEGV,
                code: Some(1),
                delivered_to,
            };
            let id = write(dir.path(), Some(ending.clone()));
            (dir, id, ending)
        };
        let (waiter, id, ending) = crashed(EventStreamId::root());
        let end = read_all(waiter.path(), &id)
            .check_unreturned_call(true)
            .unwrap();
        assert_eq!((end.ending, end.call_in_flight), (ending, Some(2)));
        let (faulted, id, _) = crashed(EventStreamId::root().child(0));
        let error = read_all(faulted.path(), &id)
            .check_unreturned_call(true)
            .unwrap_err();
        assert!(
            error.contains("died of signal 11 delivered to it"),
            "{error}"
        );

        // A thread the recorder did not see end (SIGKILL, a dead recorder)
        // leaves no terminal record.
        let killed = tempfile::tempdir().unwrap();
        let id = write(killed.path(), None);
        let error = read_all(killed.path(), &id)
            .check_unreturned_call(true)
            .unwrap_err();
        assert!(error.contains("has no terminal record"), "{error}");

        // A terminal record in a format this reader does not define.
        let corrupt = tempfile::tempdir().unwrap();
        let id = write(corrupt.path(), Some(ThreadEnding::Exited(0)));
        let end_path =
            super::with_suffix(&corrupt.path().join("thread").join(id.file_name()), ".end");
        let mut bytes = fs::read(&end_path).unwrap();
        bytes[super::STREAM_END_MAGIC.len() - 2] = b'2';
        fs::write(&end_path, bytes).unwrap();
        let error = read_all(corrupt.path(), &id)
            .check_unreturned_call(true)
            .unwrap_err();
        assert!(error.contains("is refused"), "{error}");
    }

    /// A stream whose last write fails gets no terminal record, so a thread
    /// that exited inside a call is not parked on a stream that may have lost
    /// records: `/dev/full` fails every write with ENOSPC, as a full disk does.
    #[test]
    fn a_stream_that_cannot_be_written_in_full_gets_no_terminal_record() {
        use super::EventWriter;
        use crate::event::Event;
        use crate::event::SyscallEvent;

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("thread")).unwrap();
        let id = EventStreamId::root().child(0);
        let full = || {
            fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap()
        };
        let mut writer =
            EventWriter::for_test(full(), full()).with_terminal_record(dir.path(), &id);
        writer
            .push_event(Event {
                event: Ok(SyscallEvent::Return(3)),
            })
            .unwrap();
        let error = writer
            .finish(Some(super::ThreadEnding::Exited(0)))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC), "{error}");
        let end = dir
            .path()
            .join("thread")
            .join(format!("{}.end", id.file_name()));
        assert!(
            !end.exists(),
            "a terminal record was written after a failed write"
        );
    }

    /// A stream that ends at an event boundary ends cleanly (`None`), as one
    /// whose thread died inside its last call does; one that ends inside an
    /// event is an error. The syscall stream says when the last call was read,
    /// and ends cleanly too for a thread that died before the recorder saw its
    /// call.
    #[test]
    fn a_stream_cut_at_an_event_boundary_ends_cleanly() {
        use std::io::Write as _;

        use super::DebugEvent;
        use super::EventReader;
        use super::EventWriter;
        use crate::event::Event;
        use crate::event::SyscallEvent;

        let dir = tempfile::tempdir().unwrap();
        let id = EventStreamId::root();
        {
            let mut writer = EventWriter::create(dir.path(), &id).unwrap();
            writer
                .push_debug_event(DebugEvent::for_test(Sysno::read, "read"))
                .unwrap();
            writer
                .push_event(Event {
                    event: Ok(SyscallEvent::Return(3)),
                })
                .unwrap();
            // The last call, which never returned: its syscall, no event.
            writer
                .push_debug_event(DebugEvent::for_test(Sysno::poll, "poll"))
                .unwrap();
        }
        let mut reader = EventReader::open(dir.path(), &id).unwrap();
        assert!(reader.next_debug_event_or_end().unwrap().is_some());
        assert!(!reader.syscalls_exhausted().unwrap());
        assert!(!reader.events_exhausted().unwrap());
        assert!(matches!(
            reader.next_event_or_end().unwrap(),
            Some(Event {
                event: Ok(SyscallEvent::Return(3))
            })
        ));
        assert!(reader.events_exhausted().unwrap());
        assert_eq!(reader.events_read, 1);
        assert!(reader.next_debug_event_or_end().unwrap().is_some());
        assert!(reader.syscalls_exhausted().unwrap());
        assert!(reader.next_event_or_end().unwrap().is_none());
        // A thread that died in a call before the recorder saw it.
        assert!(reader.next_debug_event_or_end().unwrap().is_none());
        assert_eq!(reader.count, 2);

        // Two bytes of a further event: cut inside it.
        let path = dir.path().join("thread").join(id.file_name());
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(&[0, 0])
            .unwrap();
        let mut reader = EventReader::open(dir.path(), &id).unwrap();
        assert!(reader.next_event_or_end().unwrap().is_some());
        assert!(reader.next_event_or_end().is_err());
    }

    #[test]
    fn unused_args_do_not_cause_desync() {
        // statfs(path, buf) uses two arguments; registers 2..6 are unused and may
        // hold arbitrary leftover values. The raw compare (the bug) sees them as
        // different, but the normalized compare must not.
        let clean = raw(Sysno::statfs, SyscallArgs::new(0x1000, 0x2000, 0, 0, 0, 0));
        let garbage = raw(
            Sysno::statfs,
            SyscallArgs::new(0x1000, 0x2000, 0xdead, 0xbeef, 0, 0xcafe),
        );
        assert_ne!(clean, garbage, "raw comparison should differ (the bug)");
        assert_eq!(
            normalize_unused_args(clean),
            normalize_unused_args(garbage),
            "normalized statfs must ignore unused argument registers"
        );
    }

    #[test]
    fn close_range_ignores_unused_args() {
        let clean = raw(
            Sysno::close_range,
            SyscallArgs::new(3, 9, libc::CLOSE_RANGE_CLOEXEC as usize, 0, 0, 0),
        );
        let garbage = raw(
            Sysno::close_range,
            SyscallArgs::new(
                3,
                9,
                libc::CLOSE_RANGE_CLOEXEC as usize,
                0xdead,
                0xbeef,
                0xcafe,
            ),
        );
        assert_eq!(normalize_unused_args(clean), normalize_unused_args(garbage));
    }

    #[test]
    fn meaningful_args_are_not_masked() {
        // fcntl(fd, cmd, arg) uses three arguments -- reverie exposes only two
        // typed fields, but the third register is real data. A difference there
        // must still be reported (guards against masking real divergences).
        let a = raw(Sysno::fcntl, SyscallArgs::new(3, 4, 0x800, 0, 0, 0));
        let b = raw(Sysno::fcntl, SyscallArgs::new(3, 4, 0x0, 0, 0, 0));
        assert_ne!(
            normalize_unused_args(a),
            normalize_unused_args(b),
            "fcntl's third argument is meaningful and must not be zeroed"
        );
    }

    #[test]
    fn delegated_syscalls_have_kernel_arities() {
        assert_eq!(kernel_arg_count(Sysno::kill), Some(2));
        assert_eq!(kernel_arg_count(Sysno::ftruncate), Some(2));
        assert_eq!(kernel_arg_count(Sysno::listen), Some(2));
        assert_eq!(kernel_arg_count(Sysno::shutdown), Some(2));
        assert_eq!(kernel_arg_count(Sysno::accept), Some(3));
        assert_eq!(kernel_arg_count(Sysno::bind), Some(3));
        assert_eq!(kernel_arg_count(Sysno::accept4), Some(4));
        assert_eq!(kernel_arg_count(Sysno::rt_sigpending), Some(2));
        assert_eq!(kernel_arg_count(Sysno::tgkill), Some(3));
        assert_eq!(kernel_arg_count(Sysno::prctl), Some(5));
    }

    #[test]
    fn path_query_and_mutation_syscalls_have_kernel_arities() {
        // faccessat takes no flags argument; glibc leaves the fourth register
        // unset, so it must not be compared. faccessat2 is the flags form.
        assert_eq!(kernel_arg_count(Sysno::faccessat), Some(3));
        assert_eq!(kernel_arg_count(Sysno::faccessat2), Some(4));
        assert_eq!(kernel_arg_count(Sysno::chdir), Some(1));
        assert_eq!(kernel_arg_count(Sysno::getcwd), Some(2));
        assert_eq!(kernel_arg_count(Sysno::set_robust_list), Some(2));
        assert_eq!(kernel_arg_count(Sysno::rmdir), Some(1));
        assert_eq!(kernel_arg_count(Sysno::rename), Some(2));
        assert_eq!(kernel_arg_count(Sysno::link), Some(2));
        assert_eq!(kernel_arg_count(Sysno::symlink), Some(2));
        assert_eq!(kernel_arg_count(Sysno::chmod), Some(2));
        assert_eq!(kernel_arg_count(Sysno::chown), Some(3));
        assert_eq!(kernel_arg_count(Sysno::lchown), Some(3));
        assert_eq!(kernel_arg_count(Sysno::mknod), Some(3));
    }

    #[test]
    fn unknown_syscall_compares_all_registers() {
        // A syscall without an arity entry keeps the conservative behavior of
        // comparing every register.
        let a = raw(Sysno::getpid, SyscallArgs::new(0, 0, 0xaa, 0, 0, 0));
        let b = raw(Sysno::getpid, SyscallArgs::new(0, 0, 0xbb, 0, 0, 0));
        assert_ne!(normalize_unused_args(a), normalize_unused_args(b));
    }

    #[test]
    fn pedigree_stream_ids_are_stable_and_collision_free() {
        let root = EventStreamId::root();
        let recording = ChildEventStreamIds::default();
        let replay = ChildEventStreamIds::default();
        let first = recording.next(&root);
        let second = recording.next(&root);
        assert_eq!(first.to_string(), "3.0");
        assert_eq!(second.to_string(), "3.1");
        assert_ne!(first, second);
        assert_eq!(first, replay.next(&root));
        assert_eq!(second, replay.next(&root));

        let serialized = serde_json::to_vec(&recording).expect("serialize child ordinal");
        let restored: ChildEventStreamIds =
            serde_json::from_slice(&serialized).expect("restore child ordinal");
        assert_eq!(restored.next(&root).to_string(), "3.2");

        let grandchildren = ChildEventStreamIds::default();
        assert_eq!(grandchildren.next(&first).to_string(), "3.0.0");
    }

    #[test]
    fn deeply_nested_stream_names_remain_bounded() {
        let mut stream_id = EventStreamId::root();
        for ordinal in 0..125 {
            stream_id = stream_id.child(ordinal);
        }
        assert_eq!(stream_id.file_name().len(), "stream-".len() + 64);
        assert!(stream_id.file_name().len() <= 255);
    }

    #[test]
    fn duplicate_stream_identity_refuses_instead_of_overwriting() {
        let data = tempfile::tempdir().expect("create recording directory");
        let stream_id = EventStreamId::root().child(0);
        let first = super::EventWriter::create(data.path(), &stream_id).expect("create stream");
        let error = super::EventWriter::create(data.path(), &stream_id)
            .err()
            .expect("duplicate stream must refuse");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        drop(first);
    }

    #[test]
    fn mismatched_stream_header_refuses_instead_of_aliasing() {
        let data = tempfile::tempdir().expect("create recording directory");
        let expected = EventStreamId::root().child(0);
        let different = EventStreamId::root().child(1);
        let writer = super::EventWriter::create(data.path(), &expected).expect("create stream");
        drop(writer);

        let file_name = expected.file_name();
        let mut file = io::BufWriter::new(
            fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(data.path().join("thread").join(file_name))
                .expect("open stream for corruption"),
        );
        super::encode_stream_header(&mut file, &different).expect("write mismatched header");
        drop(file);

        let error = super::EventReader::open(data.path(), &expected)
            .err()
            .expect("mismatched stream identity must refuse");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("identity mismatch"));
    }
}
