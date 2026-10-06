/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Module contains macroses that help tracing DETLOG entires for the purpose of verifiying determinism
//! ['detlog'] can be used to write a deterministic log entry at INFO level
//! ['detlog_debug] can be use to write a deterministic log entry at DEBUG level

use std::fmt;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

/// Delimits the machine-readable record appended to a human DETLOG message.
///
/// The human text remains available to people and historical readers. Current
/// verification consumes the JSON after this delimiter for event class and
/// position rather than recovering those facts from prose.
pub const RECORD_SEPARATOR: &str = " DETLOG_RECORD=";

/// Current schema written beside each structured DETLOG event.
pub const RECORD_SCHEMA: u32 = 1;

/// Producer-owned facts needed by log comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DetLogEvent {
    /// A deterministic record outside the syscall-specific classes below.
    Other,
    /// A syscall entered but not yet completed.
    Syscall,
    /// A completed syscall, carrying Detcore's own counter.
    SyscallResult {
        /// Number of syscalls completed by guest threads at this point.
        finished_syscall_number: u64,
    },
    /// A scheduler turn committed to the guest.
    SchedulerCommit {
        /// Detcore's scheduler turn number.
        scheduler_turn: u64,
        /// Committed virtual time at this turn, in nanoseconds.
        virtual_nanoseconds: u64,
        /// Whether this turn is host-timing-sensitive internal I/O polling.
        internal_io_poll: bool,
        /// Whether this turn reads the guest runtime's `/proc/self/maps`.
        runtime_maps_read: bool,
    },
    /// Per-turn committed-time bookkeeping excluded from deterministic comparison.
    SchedulerCommittedTime,
    /// The scheduler found no runnable thread and took its established kick path.
    SchedulerEmptyQueueKick,
}

/// Versioned serialized form appended to the human log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetLogRecord {
    /// Serialized record schema.
    pub schema: u32,
    /// Closed event payload.
    pub event: DetLogEvent,
}

impl DetLogRecord {
    /// Construct a record in the current schema.
    pub fn new(event: DetLogEvent) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            event,
        }
    }

    /// Split a human record from its producer-owned structured suffix.
    ///
    /// An absent suffix is the historical format. Once the delimiter is
    /// present, malformed JSON or another schema is an error rather than an
    /// invitation to fall back to the human text.
    pub fn split(message: &str) -> Result<(&str, Option<Self>), String> {
        let Some((human, encoded)) = message.rsplit_once(RECORD_SEPARATOR) else {
            return Ok((message, None));
        };
        let record: Self = serde_json::from_str(encoded)
            .map_err(|error| format!("malformed DETLOG record: {error}"))?;
        if record.schema != RECORD_SCHEMA {
            return Err(format!(
                "unsupported DETLOG record schema {}; expected {}",
                record.schema, RECORD_SCHEMA
            ));
        }
        Ok((human, Some(record)))
    }
}

/// Serialize one event for appending to its human log message.
#[doc(hidden)]
pub fn record_suffix(event: DetLogEvent) -> String {
    let encoded = serde_json::to_string(&DetLogRecord::new(event))
        .expect("DETLOG record serialization cannot fail");
    format!("{RECORD_SEPARATOR}{encoded}")
}

/// A process-local sink for deterministic INFO records: the emitting module's path (the
/// target `tracing` would give the record), the record suffix, and the message.
pub type DetlogForwarder = for<'a> fn(&str, &str, fmt::Arguments<'a>);

/// Which emitting modules a forwarded process may send DETLOG records from.
///
/// WHY THIS IS NEEDED. In-process, `detlog!` asks `tracing` whether INFO is
/// enabled at its own callsite, whose target is the emitting module's path, so
/// a target-scoped filter such as `RUST_LOG=warn,detcore::random=info` keeps
/// the `detcore::random` records and drops the rest. A tool running in another
/// process (SaBRe's guest plugin) has no subscriber to ask, so the coordinator
/// must hand it the same per-target answer. A single yes/no for the whole
/// `detcore` target either drops the scoped records or emits records the
/// coordinator's filter suppresses; both make the backends' logs disagree.
///
/// The policy holds the coordinator's answer at a default target that no
/// directive names and at every target a directive names. Its answer for a
/// record is the entry for the longest named target that is a string prefix of
/// the record's target, else the default. That is `tracing-subscriber`'s rule
/// for target directives: the most specific matching directive decides, and
/// any target a directive could match through is itself an entry here.
///
/// Limit: directives that select by span (`[syscall.intercept]=info`, or a
/// field filter with a value, which matches span fields) decide per record
/// from the spans the emitting thread has entered. Those spans exist only in
/// the process where the coordinator's subscriber runs (under ptrace,
/// reverie-ptrace enters `syscall.intercept` around each Detcore callback); a
/// forwarded process has none, so this per-target answer, taken once at
/// launch, cannot reproduce them. Target directives and field-presence
/// filters are reproduced exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardPolicy {
    default: bool,
    targets: Vec<(String, bool)>,
}

impl ForwardPolicy {
    /// Forward every record. This is the policy the historical encoding `1`
    /// denoted, and the one a coordinator logging all of Detcore at INFO sends.
    pub fn all() -> Self {
        Self {
            default: true,
            targets: Vec::new(),
        }
    }

    /// Ask the current `tracing` subscriber which targets it takes INFO
    /// records from.
    ///
    /// `directives` is the filter text the subscriber was built from (for the
    /// CLI, `RUST_LOG`). Only the target names are read from it; whether each is
    /// enabled is asked of the subscriber, so level overrides added after the
    /// text (`--log`) are honoured. Unparseable or extra names are harmless:
    /// probing a target no directive names returns that target's true answer.
    pub fn from_current_subscriber(directives: &str) -> Self {
        Self::from_probe(directives, info_enabled_for_target)
    }

    /// Build a policy from `directives`' target names and an INFO probe.
    pub fn from_probe(directives: &str, info_enabled: impl Fn(&str) -> bool) -> Self {
        let mut targets: Vec<(String, bool)> = Vec::new();
        for directive in directives.split(',') {
            let target = directive
                .split(['[', '='])
                .next()
                .unwrap_or_default()
                .trim();
            if target.is_empty() || targets.iter().any(|(named, _)| named == target) {
                continue;
            }
            targets.push((target.to_owned(), info_enabled(target)));
        }
        Self {
            // No directive target is empty, so only directives that name no
            // target (the default level) can match this one.
            default: info_enabled(""),
            targets,
        }
    }

    /// Whether any record could be forwarded under this policy.
    pub fn forwards_any(&self) -> bool {
        self.default || self.targets.iter().any(|(_, enabled)| *enabled)
    }

    /// Whether a record emitted by module `target` is forwarded.
    pub fn forwards(&self, target: &str) -> bool {
        self.targets
            .iter()
            .filter(|(named, _)| target.starts_with(named.as_str()))
            .max_by_key(|(named, _)| named.len())
            .map_or(self.default, |(_, enabled)| *enabled)
    }

    /// Encode for an environment variable: the default (`1` or `0`) followed by
    /// `,target=1` or `,target=0` per named target. Directive targets cannot
    /// contain `,` or `=`. `1` alone is the historical forward-everything value.
    pub fn encode(&self) -> String {
        let mut encoded = String::from(if self.default { "1" } else { "0" });
        for (target, enabled) in &self.targets {
            encoded.push(',');
            encoded.push_str(target);
            encoded.push_str(if *enabled { "=1" } else { "=0" });
        }
        encoded
    }

    /// Decode [`ForwardPolicy::encode`]'s output, refusing anything else.
    pub fn decode(encoded: &str) -> Result<Self, String> {
        fn flag(text: &str) -> Option<bool> {
            match text {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            }
        }
        let mut entries = encoded.split(',');
        let default = entries
            .next()
            .and_then(flag)
            .ok_or_else(|| format!("DETLOG forwarding policy {encoded:?} lacks a 0/1 default"))?;
        let mut targets = Vec::new();
        for entry in entries {
            let parsed = entry
                .split_once('=')
                .filter(|(target, _)| !target.is_empty())
                .and_then(|(target, enabled)| Some((target.to_owned(), flag(enabled)?)));
            let Some(parsed) = parsed else {
                return Err(format!(
                    "DETLOG forwarding policy {encoded:?} has malformed entry {entry:?}"
                ));
            };
            targets.push(parsed);
        }
        Ok(Self { default, targets })
    }
}

/// Whether the current subscriber would log a `detlog!` record emitted in the
/// module `target`. In-process that record passes two checks: `detlog!`'s own
/// `tracing::enabled!(INFO)`, a hint with no fields, and then the
/// `tracing::info!` event itself, which carries one field, the message. A
/// directive with a field filter (`detcore::random[{name}]=warn`) applies to
/// the hint whatever its fields but to the event only when the event has that
/// field, so the two checks can disagree; the record is logged only when both
/// pass, and so this answers true only when both probes do.
pub fn info_enabled_for_target(target: &str) -> bool {
    use tracing::Metadata;
    use tracing::callsite::DefaultCallsite;
    use tracing::field::FieldSet;
    use tracing::level_filters::LevelFilter;
    use tracing::metadata::Kind;

    // The hint `tracing::enabled!` builds: no fields.
    static HINT_CALLSITE: DefaultCallsite = DefaultCallsite::new(&HINT_METADATA);
    static HINT_METADATA: Metadata<'static> = Metadata::new(
        "detlog forwarding hint probe",
        "detcore::detlog",
        tracing::Level::INFO,
        None,
        None,
        None,
        FieldSet::new(&[], tracing::callsite::Identifier(&HINT_CALLSITE)),
        Kind::HINT,
    );
    // `detlog!` events carry exactly one field, the formatted message.
    static PROBE_CALLSITE: DefaultCallsite = DefaultCallsite::new(&PROBE_METADATA);
    static PROBE_METADATA: Metadata<'static> = Metadata::new(
        "detlog forwarding probe",
        "detcore::detlog",
        tracing::Level::INFO,
        None,
        None,
        None,
        FieldSet::new(&["message"], tracing::callsite::Identifier(&PROBE_CALLSITE)),
        Kind::EVENT,
    );

    if tracing::level_filters::STATIC_MAX_LEVEL < LevelFilter::INFO
        || LevelFilter::current() < LevelFilter::INFO
    {
        return false;
    }
    let hint = Metadata::new(
        "detlog forwarding hint probe",
        target,
        tracing::Level::INFO,
        None,
        None,
        Some(target),
        FieldSet::new(&[], tracing::callsite::Identifier(&HINT_CALLSITE)),
        Kind::HINT,
    );
    let event = Metadata::new(
        "detlog forwarding probe",
        target,
        tracing::Level::INFO,
        None,
        None,
        Some(target),
        FieldSet::new(&["message"], tracing::callsite::Identifier(&PROBE_CALLSITE)),
        Kind::EVENT,
    );
    tracing::dispatcher::get_default(|dispatch| dispatch.enabled(&hint) && dispatch.enabled(&event))
}

static FORWARDER: OnceLock<(DetlogForwarder, ForwardPolicy)> = OnceLock::new();

/// Installs a process-local sink for deterministic INFO records.
///
/// Backends whose tool runs in another process can use this to transport the
/// same records that are normally observed through the coordinator's tracing
/// subscriber. `policy` is the coordinator's per-target answer; records from
/// other modules are not forwarded. Only the first sink installed in a process
/// is retained.
pub fn set_forwarder(
    forwarder: DetlogForwarder,
    policy: ForwardPolicy,
) -> Result<(), (DetlogForwarder, ForwardPolicy)> {
    FORWARDER.set((forwarder, policy))
}

/// The line a forwarded record travels as: the record as the coordinator's tracing
/// subscriber would print it, naming the module that emitted it (a forwarded
/// `detcore::tool_local` record must not read as `detcore` and differ from the same
/// record logged through tracing), then a newline.
#[doc(hidden)]
pub fn forwarded_line(target: &str, record_suffix: &str, message: fmt::Arguments<'_>) -> Vec<u8> {
    format!("INFO {target}: DETLOG {message}{record_suffix}\n").into_bytes()
}

/// Calls `operation` and restores the calling thread's `errno` afterwards. An
/// in-guest tool runs on the guest's own thread, so a failed forwarding write must
/// not change what the guest reads there.
fn preserving_errno(operation: impl FnOnce()) {
    // SAFETY: `__errno_location` returns the calling thread's live errno slot.
    let errno = unsafe { libc::__errno_location() };
    // SAFETY: as above; the slot is valid for the life of the thread.
    let saved = unsafe { *errno };
    operation();
    // SAFETY: as above.
    unsafe { *errno = saved };
}

/// A [`DetlogForwarder`] for a tool that runs inside the guest process and shares the
/// guest's standard error: it writes each record as one [`forwarded_line`] there, where
/// Hermit's verification separates the records from the guest's own output by their
/// text.
///
/// It uses one raw `write` per record, so records from several guest threads cannot
/// interleave, and it avoids `tracing`'s thread-local dispatcher, which libc's final
/// `exit_group` can reach after Rust thread-local destruction has begun. A write error
/// drops the record. The calling thread's `errno` is preserved.
pub fn forward_to_stderr(target: &str, record_suffix: &str, message: fmt::Arguments<'_>) {
    preserving_errno(|| {
        let line = forwarded_line(target, record_suffix, message);
        let mut rest = line.as_slice();
        while !rest.is_empty() {
            // SAFETY: `rest` is a live, initialized byte slice of the given length.
            let written = unsafe {
                libc::write(
                    libc::STDERR_FILENO,
                    rest.as_ptr().cast::<libc::c_void>(),
                    rest.len(),
                )
            };
            if written > 0 {
                rest = &rest[written as usize..];
            } else if written == 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return;
            }
        }
    });
}

/// Starts the line [`send_forwarded_record`] sends in place of a record it could not
/// deliver, followed by the record's length and the error number. Its presence means
/// the forwarded records are incomplete, so verification must not compare them.
pub const FORWARDING_LOSS_NOTICE: &str = "HERMIT_DETLOG_RECORD_LOST";

/// The message an in-guest runtime sends on the forwarding socket when it has to
/// give the socket up (reverie-liteinst's `reserve_tool_output_fd` retirement
/// message): a [`FORWARDING_LOSS_NOTICE`], since no later record can arrive.
pub const FORWARDING_RETIRED_NOTICE: &[u8] = b"HERMIT_DETLOG_RECORD_LOST 0 0 socket retired\n";

/// Losses whose notice could not be sent either; the next message that gets through
/// is preceded by a notice for them, so a later record cannot hide them.
static UNREPORTED_LOSSES: AtomicU64 = AtomicU64::new(0);

/// Sends one record as one [`forwarded_line`] message on `socket`, a message-oriented
/// socket (such as one end of a `SOCK_SEQPACKET` pair) that carries nothing but records,
/// from which Hermit's verification moves them into the run's log.
///
/// It uses one raw `send` with `MSG_NOSIGNAL`, so a socket whose reader has gone never
/// raises SIGPIPE in the guest. A record that cannot be sent (too large for the socket,
/// or any other error) is replaced by a short [`FORWARDING_LOSS_NOTICE`] message, so the
/// loss is visible rather than silent; if that notice cannot be sent either, a notice
/// precedes the next message that can. The calling thread's `errno` is preserved.
pub fn send_forwarded_record(
    socket: libc::c_int,
    target: &str,
    record_suffix: &str,
    message: fmt::Arguments<'_>,
) {
    let send = |bytes: &[u8]| loop {
        // SAFETY: `bytes` is a live, initialized byte slice of the given length.
        let sent = unsafe {
            libc::send(
                socket,
                bytes.as_ptr().cast::<libc::c_void>(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if sent >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    let notice = |length: usize, errno: i32| format!("{FORWARDING_LOSS_NOTICE} {length} {errno}\n");
    preserving_errno(|| {
        let unreported = UNREPORTED_LOSSES.swap(0, Ordering::AcqRel);
        if unreported > 0 && send(notice(0, 0).as_bytes()).is_err() {
            UNREPORTED_LOSSES.fetch_add(unreported, Ordering::AcqRel);
        }
        let line = tagged_forwarded_message(
            current_tid(),
            &forwarded_line(target, record_suffix, message),
        );
        if let Err(error) = send(&line) {
            let errno = error.raw_os_error().unwrap_or(0);
            if send(notice(line.len(), errno).as_bytes()).is_err() {
                UNREPORTED_LOSSES.fetch_add(1, Ordering::AcqRel);
            }
        }
    });
}

/// The calling thread's kernel id, for [`tagged_forwarded_message`].
fn current_tid() -> i32 {
    // SAFETY: gettid has no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// Prefixes a forwarded record with the id of the guest thread that sent it,
/// `T<tid> `, so the coordinator can write each thread's records at that thread's
/// own scheduler turn ([`write_forwarded_for`]). The coordinator strips the tag
/// before the record reaches the log.
pub fn tagged_forwarded_message(tid: i32, line: &[u8]) -> Vec<u8> {
    let mut message = format!("T{tid} ").into_bytes();
    message.extend_from_slice(line);
    message
}

/// The sender's thread id and the record, for a message tagged by
/// [`tagged_forwarded_message`]; `None` for an untagged message (a
/// [`FORWARDING_LOSS_NOTICE`] or [`FORWARDING_RETIRED_NOTICE`] sent raw).
pub fn split_forwarded_message(message: &[u8]) -> Option<(i32, &[u8])> {
    let rest = message.strip_prefix(b"T")?;
    let space = rest.iter().position(|&byte| byte == b' ')?;
    let digits = std::str::from_utf8(&rest[..space]).ok()?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, &rest[space + 1..]))
}

/// Receives each record the coordinator writes from a forwarding socket: a
/// [`forwarded_line`] (its sender tag removed), or a [`FORWARDING_LOSS_NOTICE`].
pub type ForwardedRecordSink = Box<dyn FnMut(&[u8]) + Send>;

/// Where a thread's forwarded records may be written at its request
/// ([`ForwardedOrder::at_request`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Runner {
    /// No turn has been committed yet and no request has arrived.
    NoTurnYet,
    /// No turn has been committed yet; this thread sent the first request, so it
    /// is the root.
    Root(i32),
    /// The thread of the last ordinary COMMIT, whose request ends that turn.
    Exclusive(i32),
    /// The last COMMIT was a BACKGROUND one: no request is a fixed point.
    None,
}

/// The order rule's state ([`set_forwarded_source`]): the forwarded records taken
/// from the socket but not yet written, by sending thread, and which thread's
/// request is a point the schedule fixes. It decides what to write; the caller
/// writes it.
#[derive(Debug)]
pub(crate) struct ForwardedOrder {
    pending: std::collections::BTreeMap<i32, Vec<Vec<u8>>>,
    runner: Runner,
    /// A second root asked before any turn ([`ForwardedOrder::at_request`]).
    ambiguous: bool,
    /// An untagged ordinary record arrived ([`ForwardedOrder::untagged`]).
    legacy: bool,
}

/// The [`FORWARDING_LOSS_NOTICE`] the coordinator writes when a forwarded record
/// arrives without a sender tag (a guest runtime older than the tag), so its place
/// in the log would be the host's choice.
pub const FORWARDING_UNTAGGED_RECORD_NOTICE: &[u8] =
    b"HERMIT_DETLOG_RECORD_LOST 0 0 untagged forwarded record from an older runtime\n";

/// The [`FORWARDING_LOSS_NOTICE`] the coordinator writes when more than one guest
/// thread sends a request before the first turn, so the forwarded records' order
/// would depend on the host.
pub const FORWARDING_AMBIGUOUS_ORDER_NOTICE: &[u8] =
    b"HERMIT_DETLOG_RECORD_LOST 0 0 forwarded order ambiguous: more than one root\n";

impl ForwardedOrder {
    pub(crate) fn new() -> Self {
        Self {
            pending: Default::default(),
            runner: Runner::NoTurnYet,
            ambiguous: false,
            legacy: false,
        }
    }

    /// Queues a record thread `tid` sent.
    pub(crate) fn queue(&mut self, tid: i32, record: Vec<u8>) {
        self.pending.entry(tid).or_default().push(record);
    }

    /// The records to write when a request from `tid` arrives: `tid`'s queue if its
    /// request is a point the schedule fixes, otherwise none.
    pub(crate) fn at_request(&mut self, tid: i32) -> Vec<Vec<u8>> {
        match self.runner {
            Runner::NoTurnYet => {
                self.runner = Runner::Root(tid);
                self.pending.remove(&tid).unwrap_or_default()
            }
            Runner::Root(root) | Runner::Exclusive(root) if root == tid => {
                self.pending.remove(&tid).unwrap_or_default()
            }
            Runner::Root(_) if !self.ambiguous => {
                // A second thread asks before any turn: there is more than one root
                // (for example a preload constructor that forked before the runtime
                // was installed), so which root's records come first is decided by
                // the host. Say so instead of writing a host-timed order: the loss
                // notice makes verification refuse to compare the records.
                self.ambiguous = true;
                vec![FORWARDING_AMBIGUOUS_ORDER_NOTICE.to_vec()]
            }
            _ => Vec::new(),
        }
    }

    /// The records to write immediately before the scheduler commits a turn of
    /// `tid` (all of `tid`'s queue); `exclusive` is false for a BACKGROUND turn.
    pub(crate) fn at_commit(&mut self, tid: i32, exclusive: bool) -> Vec<Vec<u8>> {
        self.runner = if exclusive {
            Runner::Exclusive(tid)
        } else {
            Runner::None
        };
        self.pending.remove(&tid).unwrap_or_default()
    }

    /// What to write for a message without a sender tag, at once: a loss notice the
    /// runtime sent as is; for an ordinary record (from a runtime older than the
    /// tag), the record preceded, the first time, by
    /// [`FORWARDING_UNTAGGED_RECORD_NOTICE`], so verification refuses to compare
    /// records whose place the host chose.
    pub(crate) fn untagged(&mut self, message: &[u8]) -> Vec<Vec<u8>> {
        if message.starts_with(FORWARDING_LOSS_NOTICE.as_bytes()) {
            return vec![message.to_vec()];
        }
        let mut out = Vec::new();
        if !self.legacy {
            self.legacy = true;
            out.push(FORWARDING_UNTAGGED_RECORD_NOTICE.to_vec());
        }
        out.push(message.to_vec());
        out
    }

    /// Everything still queued, thread by thread in thread-id order.
    pub(crate) fn finish(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending)
            .into_values()
            .flatten()
            .collect()
    }
}

/// The receiving end of the socket an in-guest Tool sends its records on, where
/// the coordinator writes them, and the order rule's state ([`set_forwarded_source`]).
struct ForwardedSource {
    socket: OwnedFd,
    sink: ForwardedRecordSink,
    order: ForwardedOrder,
}

static FORWARDED_SOURCE: Mutex<Option<ForwardedSource>> = Mutex::new(None);

/// Registers the receiving end of the socket an in-guest Tool sends its records on
/// ([`send_forwarded_record`]) and the sink that writes them to the run's log.
///
/// THE ORDER RULE, one for every backend whose Tool runs in the guest. Each record
/// carries its sending thread ([`tagged_forwarded_message`]). Before it handles any
/// guest request the coordinator takes the records waiting on the socket into a
/// queue per sending thread ([`drain_forwarded_at_request`] at the top of the
/// request handler). A thread's queued records are written into the log:
/// - when that thread's own request arrives, if it is the thread the scheduler last
///   committed an ordinary turn to, or, before any turn, the thread whose request
///   arrives first (the root; every other thread is created during some turn). That
///   thread's request is the event that ends the turn the schedule gave it, so the
///   request arrives at a point the schedule fixes, and its records land where a
///   single-process (ptrace) run writes them, ahead of everything handling that
///   request writes. (A thread still in background IO from an earlier BACKGROUND
///   turn may run at the same time; its records wait for its own COMMIT, by the next
///   rule. Coordinator lines its continuation writes below INFO verbosity, such as
///   TRACE, are not ordered by this rule; the INFO DETLOG and COMMIT lines that
///   verification compares are.)
/// - otherwise immediately before the scheduler commits that thread's next turn
///   ([`write_forwarded_for`], at both kinds of COMMIT): a thread that runs without
///   holding the turn (a forked child doing its setup, a thread in background IO)
///   sends its records while other threads' turns are being handled, so only its
///   own next COMMIT is a point the schedule fixes;
/// - and, for any still queued when the run ends, then, thread by thread in
///   thread-id order.
///
/// If a second thread sends a request before the first turn, there is more than one
/// root (a preload constructor that forked before the runtime was installed, say),
/// and which root's records come first would be the host's choice. The coordinator
/// then writes [`FORWARDING_AMBIGUOUS_ORDER_NOTICE`], so verification refuses to
/// compare the records rather than compare a host-timed order.
///
/// A `send` on a message socket queues the record on the receiver before it returns,
/// so a guest thread's records sent before a request are queued by the time that
/// request is handled, and so before the turn it leads to is committed. Writing every
/// thread's records at any request's arrival instead put a record wherever the host
/// happened to deliver it: a forked child's records, sent while its parent's next
/// request was being handled, landed before or after the scheduler's records for that
/// request depending on timing.
pub fn set_forwarded_source(socket: OwnedFd, sink: ForwardedRecordSink) {
    *FORWARDED_SOURCE.lock().unwrap() = Some(ForwardedSource {
        socket,
        sink,
        order: ForwardedOrder::new(),
    });
}

/// Unregisters the source set with [`set_forwarded_source`], after taking the last
/// waiting records and writing every queued record, thread by thread in thread-id
/// order, and closes its socket.
pub fn clear_forwarded_source() {
    let mut source = FORWARDED_SOURCE.lock().unwrap();
    if let Some(source) = source.as_mut() {
        collect_forwarded(source);
        let records = source.order.finish();
        write_records(source, records);
    }
    source.take();
}

/// Takes every message waiting on the registered forwarding socket into the queue of
/// its sending thread, without blocking and without writing it. An untagged message is
/// written at once as `ForwardedOrder::untagged` says. Without a registered source it
/// does nothing.
pub fn drain_forwarded() {
    if let Some(source) = FORWARDED_SOURCE.lock().unwrap().as_mut() {
        collect_forwarded(source);
    }
}

/// Takes waiting messages as [`drain_forwarded`] does, then writes every queued record
/// thread `tid` sent, in the order it sent them. The scheduler calls this immediately
/// before it commits a turn of `tid`; `exclusive` is false for a BACKGROUND turn,
/// which runs alongside later turns, and true for an ordinary one.
pub fn write_forwarded_for(tid: i32, exclusive: bool) {
    if let Some(source) = FORWARDED_SOURCE.lock().unwrap().as_mut() {
        collect_forwarded(source);
        let records = source.order.at_commit(tid, exclusive);
        write_records(source, records);
    }
}

/// Takes waiting messages as [`drain_forwarded`] does at the top of the handler of
/// a request from thread `tid`, and writes `tid`'s queued records now when its
/// request is a point the schedule fixes (the order rule at [`set_forwarded_source`]).
pub fn drain_forwarded_at_request(tid: i32) {
    if let Some(source) = FORWARDED_SOURCE.lock().unwrap().as_mut() {
        collect_forwarded(source);
        let records = source.order.at_request(tid);
        write_records(source, records);
    }
}

fn write_records(source: &mut ForwardedSource, records: Vec<Vec<u8>>) {
    for record in records {
        (source.sink)(&record);
    }
}

fn collect_forwarded(source: &mut ForwardedSource) {
    let fd = source.socket.as_raw_fd();
    let mut buffer = Vec::new();
    loop {
        // The size of the next message, without taking it (a message socket reports
        // the whole message's length with MSG_TRUNC).
        // SAFETY: a zero-length peek writes nothing.
        let size = unsafe {
            libc::recv(
                fd,
                std::ptr::null_mut(),
                0,
                libc::MSG_PEEK | libc::MSG_TRUNC | libc::MSG_DONTWAIT,
            )
        };
        if size < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if size <= 0 {
            // Nothing waiting (EAGAIN), the senders are gone (0), or an error: the
            // next drain tries again, and the end of the run drains the rest.
            return;
        }
        buffer.resize(size as usize, 0);
        // SAFETY: `buffer` has room for `size` bytes.
        let received = unsafe {
            libc::recv(
                fd,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if received <= 0 {
            return;
        }
        let message = &buffer[..received as usize];
        match split_forwarded_message(message) {
            Some((tid, record)) => source.order.queue(tid, record.to_vec()),
            None => {
                for record in source.order.untagged(message) {
                    (source.sink)(&record);
                }
            }
        }
    }
}

/// Returns whether a record emitted by module `target` goes to the
/// process-local deterministic-record sink.
#[doc(hidden)]
pub fn forwards_target(target: &str) -> bool {
    FORWARDER
        .get()
        .is_some_and(|(_, policy)| policy.forwards(target))
}

/// Emits one deterministic record through tracing and the process-local sink. `target` is
/// the emitting module's path, so a forwarded record names the same module a record observed
/// through tracing does.
#[doc(hidden)]
pub fn emit_forwarded(target: &str, record_suffix: &str, message: fmt::Arguments<'_>) {
    tracing::info!("DETLOG {}{}", message, record_suffix);
    FORWARDER.get().expect("forwarder disappeared").0(target, record_suffix, message);
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This is currently at the INFO log level.
#[macro_export]
macro_rules! detlog {
    (event = $event:expr; $($arg:tt)+) => {{
        if $crate::detlog::forwards_target(::core::module_path!())
            || ::tracing::enabled!(::tracing::Level::INFO)
        {
            let record_suffix = $crate::detlog::record_suffix($event);
            if $crate::detlog::forwards_target(::core::module_path!()) {
                $crate::detlog::emit_forwarded(
                    ::core::module_path!(),
                    &record_suffix,
                    format_args!($($arg)+),
                );
            } else {
                ::tracing::info!("DETLOG {}{}", format_args!($($arg)+), record_suffix);
            }
        }
    }};
    ($($arg:tt)+) => {{
        $crate::detlog!(event = $crate::detlog::DetLogEvent::Other; $($arg)+);
    }};
}

/// Whether a [`detlog!`] record emitted at this point would reach anything.
///
/// WHY THIS IS NEEDED, and why it is a macro rather than a function.
///
/// `detlog!` routes to the process-local forwarder when one is installed and its
/// policy takes the calling module's records, and to `tracing::info!` otherwise. `tracing` does not evaluate a macro's value
/// expressions when the level is disabled, so work done *inside* a `detlog!`
/// argument is already free when nothing observes the record. Work done
/// *before* the macro is not, and callers that must prepare something expensive
/// to pass in have no way to know they can skip it.
///
/// `tracing`'s level check is per-callsite and keyed on the *calling module's*
/// target, so this has to expand at the caller rather than resolve inside
/// `detcore::detlog`; otherwise a target-scoped filter could enable one and
/// disable the other.
///
/// Use it only to skip preparatory work. It is not a substitute for `detlog!`'s
/// own gating, and a caller that guards a record with it must still emit that
/// record through `detlog!`.
#[macro_export]
macro_rules! detlog_observed {
    () => {
        $crate::detlog::forwards_target(::core::module_path!())
            || ::tracing::enabled!(::tracing::Level::INFO)
    };
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This variant is at a higher log level and requires that logging verbosity is
/// set to DEBUG.
#[macro_export]
macro_rules! detlog_debug {
    (event = $event:expr; $($arg:tt)+) => {{
        if ::tracing::enabled!(::tracing::Level::DEBUG) {
            let record_suffix = $crate::detlog::record_suffix($event);
            ::tracing::debug!("DETLOG {}{}", format_args!($($arg)+), record_suffix);
        }
    }};
    ($($arg:tt)+) => {{
        $crate::detlog_debug!(event = $crate::detlog::DetLogEvent::Other; $($arg)+);
    }};
}

#[cfg(test)]
mod tests {
    use tracing::Metadata;
    use tracing::span;
    use tracing::subscriber::Interest;

    use super::DetLogEvent;
    use super::DetLogRecord;
    use super::ForwardPolicy;
    use super::RECORD_SEPARATOR;
    use super::record_suffix;

    #[test]
    fn forwarded_line_names_the_emitting_module() {
        assert_eq!(
            super::forwarded_line(
                "detcore::tool_local",
                " DETLOG_RECORD={}",
                format_args!("USER RAND: seeding PRNG for root thread with seed {}", 0)
            ),
            b"INFO detcore::tool_local: DETLOG USER RAND: seeding PRNG for root thread with seed 0 DETLOG_RECORD={}\n"
                .to_vec()
        );
    }

    /// Forwarding failures are never silent, and never change the guest's errno.
    /// One sequential test, because the unreported-loss count is process-wide:
    /// - a record sent on an invalid descriptor fails, and so does its notice,
    ///   and errno is left as it was;
    /// - the next record that gets through is preceded by a loss notice for it;
    /// - a record too large for the socket arrives as a loss notice, and the next
    ///   small record intact.
    #[test]
    fn forwarding_failures_become_loss_notices() {
        let mut pair = [-1; 2];
        // SAFETY: socketpair writes two descriptors on success.
        let created = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        assert_eq!(created, 0);
        let [sender, receiver] = pair;
        let mut buffer = vec![0u8; 2 << 20];
        let mut receive = || {
            // SAFETY: `buffer` is a live, writable byte slice.
            let received = unsafe {
                libc::recv(
                    receiver,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            assert!(received > 0, "{}", std::io::Error::last_os_error());
            String::from_utf8_lossy(&buffer[..received as usize]).into_owned()
        };
        let is_notice =
            |text: &str| text.starts_with(&format!("{} ", super::FORWARDING_LOSS_NOTICE));

        // SAFETY: errno is this thread's own.
        unsafe {
            *libc::__errno_location() = libc::EAGAIN;
            super::send_forwarded_record(-1, "detcore", "", format_args!("lost"));
            assert_eq!(*libc::__errno_location(), libc::EAGAIN);
        }
        let tag = format!("T{} ", super::current_tid());
        super::send_forwarded_record(sender, "detcore", "", format_args!("after"));
        let notice = receive();
        assert!(is_notice(&notice), "{notice}");
        assert_eq!(receive(), format!("{tag}INFO detcore: DETLOG after\n"));

        let small: libc::c_int = 4096;
        // SAFETY: a valid socket and an int option value.
        unsafe {
            libc::setsockopt(
                sender,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const small).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        let huge = "x".repeat(1 << 20);
        super::send_forwarded_record(sender, "detcore", "", format_args!("{huge}"));
        super::send_forwarded_record(sender, "detcore", "", format_args!("small"));
        let notice = receive();
        assert!(is_notice(&notice), "{notice}");
        assert_eq!(receive(), format!("{tag}INFO detcore: DETLOG small\n"));
        // SAFETY: closing the two descriptors this test created.
        unsafe {
            libc::close(sender);
            libc::close(receiver);
        }
    }

    /// The order rule at `set_forwarded_source`, on its own state (no process-wide
    /// source, so no other test's scheduler commits can interfere): records are
    /// written at the sender's own request only before any turn or when it is the
    /// thread of the last ordinary COMMIT, otherwise at its own next COMMIT, and the
    /// rest at the end in thread-id order.
    #[test]
    fn forwarded_records_are_written_at_points_the_schedule_fixes() {
        let mut order = super::ForwardedOrder::new();
        let text = |records: Vec<Vec<u8>>| -> Vec<String> {
            records
                .into_iter()
                .map(|record| String::from_utf8(record).unwrap())
                .collect()
        };
        let none: Vec<String> = Vec::new();

        // Before any turn only the root thread exists: its records go out at its
        // request. Its turn 0 makes it the exclusive runner.
        order.queue(3, b"root-1".to_vec());
        assert_eq!(text(order.at_request(3)), ["root-1"]);
        assert_eq!(text(order.at_commit(3, true)), none);
        // A forked child that does not hold the turn: queued until its own COMMIT,
        // even when its request arrives first.
        order.queue(9, b"child-1".to_vec());
        assert_eq!(text(order.at_request(9)), none);
        assert_eq!(text(order.at_commit(9, true)), ["child-1"]);
        // Thread 9 now holds the turn: its records go out at its request; thread 3's
        // records sent meanwhile wait for 3's COMMIT.
        order.queue(3, b"root-2".to_vec());
        order.queue(9, b"child-2".to_vec());
        assert_eq!(text(order.at_request(9)), ["child-2"]);
        assert_eq!(text(order.at_request(3)), none);
        // A BACKGROUND commit writes its thread's records but leaves no exclusive
        // runner, so a later request writes nothing.
        assert_eq!(text(order.at_commit(3, false)), ["root-2"]);
        order.queue(3, b"root-3".to_vec());
        assert_eq!(text(order.at_request(3)), none);
        // At the end, every queued record, thread by thread in thread-id order.
        order.queue(12, b"late-12".to_vec());
        order.queue(5, b"late-5".to_vec());
        assert_eq!(text(order.finish()), ["root-3", "late-5", "late-12"]);
        assert_eq!(text(order.finish()), none);

        // Two roots before any turn: the second request yields one ambiguity notice
        // (and only one), and the second root's records wait for its COMMIT.
        let mut order = super::ForwardedOrder::new();
        order.queue(3, b"first-root".to_vec());
        order.queue(4, b"second-root".to_vec());
        assert_eq!(text(order.at_request(3)), ["first-root"]);
        let notice = String::from_utf8(super::FORWARDING_AMBIGUOUS_ORDER_NOTICE.to_vec()).unwrap();
        assert!(notice.starts_with(&format!("{} ", super::FORWARDING_LOSS_NOTICE)));
        assert_eq!(text(order.at_request(4)), [notice]);
        assert_eq!(text(order.at_request(4)), none);
        assert_eq!(text(order.at_commit(4, true)), ["second-root"]);

        // Untagged messages: a loss notice passes as is; an ordinary record from an
        // older runtime is written with one untagged-record notice before it.
        let mut order = super::ForwardedOrder::new();
        let retired = String::from_utf8(super::FORWARDING_RETIRED_NOTICE.to_vec()).unwrap();
        assert_eq!(
            text(order.untagged(super::FORWARDING_RETIRED_NOTICE)),
            [retired]
        );
        let untagged =
            String::from_utf8(super::FORWARDING_UNTAGGED_RECORD_NOTICE.to_vec()).unwrap();
        assert!(untagged.starts_with(&format!("{} ", super::FORWARDING_LOSS_NOTICE)));
        assert_eq!(
            text(order.untagged(b"INFO detcore: DETLOG legacy-1\n")),
            [untagged, "INFO detcore: DETLOG legacy-1\n".to_string()]
        );
        assert_eq!(
            text(order.untagged(b"INFO detcore: DETLOG legacy-2\n")),
            ["INFO detcore: DETLOG legacy-2\n"]
        );
    }

    #[test]
    fn split_forwarded_message_reads_the_sender_tag() {
        assert_eq!(
            super::split_forwarded_message(b"T42 INFO detcore: x\n"),
            Some((42, &b"INFO detcore: x\n"[..]))
        );
        assert_eq!(
            super::split_forwarded_message(super::FORWARDING_RETIRED_NOTICE),
            None
        );
        assert_eq!(super::split_forwarded_message(b"T INFO"), None);
        assert_eq!(super::split_forwarded_message(b"Tx1 INFO"), None);
        assert_eq!(super::split_forwarded_message(b"INFO detcore"), None);
    }

    #[test]
    fn test_detlog() {
        detlog!("Hello : {}. From {:?}", "World", 31337);
    }

    #[test]
    fn structured_record_round_trips_and_refuses_an_incomplete_current_shape() {
        let suffix = record_suffix(DetLogEvent::SyscallResult {
            finished_syscall_number: 37,
        });
        let line = format!("INFO detcore: DETLOG finish syscall #999{suffix}");
        let (human, record) = DetLogRecord::split(&line).unwrap();
        assert_eq!(human, "INFO detcore: DETLOG finish syscall #999");
        assert_eq!(
            record.unwrap().event,
            DetLogEvent::SyscallResult {
                finished_syscall_number: 37
            }
        );

        let missing_number = format!(
            "INFO detcore: DETLOG finish syscall #999{RECORD_SEPARATOR}{{\"schema\":1,\"event\":{{\"kind\":\"syscall_result\"}}}}"
        );
        assert!(
            DetLogRecord::split(&missing_number)
                .unwrap_err()
                .contains("finished_syscall_number"),
            "an incomplete current record must fail by field name"
        );
    }

    /// Minimal subscriber that reports every callsite as enabled.
    ///
    /// `register_callsite` deliberately answers `sometimes()` rather than
    /// letting the default derive `always()`: an `always`/`never` answer is
    /// cached per callsite for the life of the process, which would leak
    /// between tests.
    struct AlwaysEnabled;

    impl tracing::Subscriber for AlwaysEnabled {
        fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// `detlog_observed!` must answer true when a subscriber would take the
    /// record. Callers use it to decide whether to prepare data for a
    /// `detlog!`, so a false negative silently drops determinism evidence.
    #[test]
    fn detlog_observed_is_true_when_a_subscriber_is_listening() {
        tracing::subscriber::with_default(AlwaysEnabled, || {
            assert!(detlog_observed!());
        });
    }

    /// ...and false when nothing is listening, which is the whole point: it is
    /// what lets `detlog_memory_maps` skip enumerating `/proc/<pid>/maps` on
    /// every syscall of a run that writes no log. Measured before this gate
    /// existed, on a QEMU/Linux boot with `RUST_LOG` unset (123 bytes of log
    /// produced): `--detlog-stack` cost 4.36x and `--detlog-heap` 4.76x the
    /// no-flag baseline.
    ///
    /// Uses a distinct callsite from the enabled test above on purpose --
    /// `tracing` caches per-callsite interest, so sharing one callsite between
    /// the two cases would make them order-dependent.
    #[test]
    fn detlog_observed_is_false_when_nothing_is_listening() {
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            assert!(!detlog_observed!());
        });
    }

    /// Takes WARN from everywhere and INFO only from `detcore::random` and its
    /// submodules, as `RUST_LOG=warn,detcore::random=info` does.
    struct RandomAtInfo;

    impl tracing::Subscriber for RandomAtInfo {
        fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
                || (*metadata.level() <= tracing::Level::INFO
                    && metadata.target().starts_with("detcore::random"))
        }
        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::INFO)
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// A forwarded process must emit exactly the records the coordinator's
    /// subscriber would take in-process. A target-scoped filter used to yield
    /// no forwarding at all, because the coordinator asked only about the
    /// generic `detcore` target, so SaBRe silently dropped the scoped records.
    #[test]
    fn forward_policy_answers_per_target_as_the_subscriber_does() {
        let policy = tracing::subscriber::with_default(RandomAtInfo, || {
            ForwardPolicy::from_current_subscriber("warn,detcore::random=info")
        });
        assert!(policy.forwards_any());
        assert!(policy.forwards("detcore::random"));
        assert!(policy.forwards("detcore::random::inner"));
        assert!(!policy.forwards("detcore"));
        assert!(!policy.forwards("detcore::tool_local"));
        assert!(!policy.forwards("detcore::scheduler::runqueue"));
        assert_eq!(policy.encode(), "0,warn=0,detcore::random=1");

        let everything = tracing::subscriber::with_default(AlwaysEnabled, || {
            ForwardPolicy::from_current_subscriber("")
        });
        assert_eq!(everything, ForwardPolicy::all());

        let nothing =
            tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
                ForwardPolicy::from_current_subscriber("warn,detcore::random=info")
            });
        assert!(!nothing.forwards_any());
    }

    /// The longest named target that prefixes the record's target decides, by
    /// string prefix as `tracing-subscriber` matches directives.
    #[test]
    fn forward_policy_most_specific_target_decides() {
        let policy = ForwardPolicy::decode("1,detcore=0,detcore::random=1").unwrap();
        assert!(policy.forwards("hermit"));
        assert!(!policy.forwards("detcore"));
        assert!(!policy.forwards("detcore::tool_local"));
        assert!(policy.forwards("detcore::random"));
        assert!(policy.forwards("detcore::randomness"));
    }

    #[test]
    fn forward_policy_encoding_round_trips_and_refuses_malformed_values() {
        assert_eq!(ForwardPolicy::decode("1").unwrap(), ForwardPolicy::all());
        let policy = ForwardPolicy::decode("0,warn=0,detcore::random=1").unwrap();
        assert_eq!(ForwardPolicy::decode(&policy.encode()).unwrap(), policy);
        for malformed in ["", "2", "1,detcore", "1,=1", "1,detcore=2", "1,detcore=1=0"] {
            assert!(
                ForwardPolicy::decode(malformed).is_err(),
                "{malformed:?} must be refused"
            );
        }
    }
}
