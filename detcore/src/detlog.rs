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

static FORWARDER: OnceLock<DetlogForwarder> = OnceLock::new();

/// Installs a process-local sink for deterministic INFO records.
///
/// Backends whose tool runs in another process can use this to transport the
/// same records that are normally observed through the coordinator's tracing
/// subscriber. Only the first sink installed in a process is retained.
pub fn set_forwarder(forwarder: DetlogForwarder) -> Result<(), DetlogForwarder> {
    FORWARDER.set(forwarder)
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
        let line = forwarded_line(target, record_suffix, message);
        if let Err(error) = send(&line) {
            let errno = error.raw_os_error().unwrap_or(0);
            if send(notice(line.len(), errno).as_bytes()).is_err() {
                UNREPORTED_LOSSES.fetch_add(1, Ordering::AcqRel);
            }
        }
    });
}

/// Receives each message the coordinator drains from a forwarding socket, in arrival
/// order: a [`forwarded_line`], or a [`FORWARDING_LOSS_NOTICE`].
pub type ForwardedRecordSink = Box<dyn FnMut(&[u8]) + Send>;

/// The receiving end of the socket an in-guest Tool sends its records on, and where
/// the coordinator writes them ([`set_forwarded_source`]).
static FORWARDED_SOURCE: Mutex<Option<(OwnedFd, ForwardedRecordSink)>> = Mutex::new(None);

/// Registers the receiving end of the socket an in-guest Tool sends its records on
/// ([`send_forwarded_record`]) and the sink that writes them to the run's log.
///
/// THE ORDER RULE, one for every backend whose Tool runs in the guest: the coordinator
/// writes the records already waiting on the socket immediately before it handles any
/// guest request ([`drain_forwarded`] at the top of the request handler), and once more
/// when the run ends. A `send` on a message socket queues the record on the receiver
/// before it returns, so a guest thread's records for an event are waiting when its
/// next request arrives, and every coordinator record for that event comes from
/// handling that request. The log then holds the guest's and the coordinator's
/// records in the order a single-process (ptrace) run writes them, and both
/// verification runs of a deterministic guest write the same log.
pub fn set_forwarded_source(socket: OwnedFd, sink: ForwardedRecordSink) {
    *FORWARDED_SOURCE.lock().unwrap() = Some((socket, sink));
}

/// Unregisters the source set with [`set_forwarded_source`], after a last
/// [`drain_forwarded`], and closes its socket.
pub fn clear_forwarded_source() {
    drain_forwarded();
    FORWARDED_SOURCE.lock().unwrap().take();
}

/// Passes every message waiting on the registered forwarding socket to its sink, in
/// arrival order, without blocking. Without a registered source it does nothing.
pub fn drain_forwarded() {
    let mut source = FORWARDED_SOURCE.lock().unwrap();
    let Some((socket, sink)) = source.as_mut() else {
        return;
    };
    let fd = socket.as_raw_fd();
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
        sink(&buffer[..received as usize]);
    }
}

/// Returns whether a process-local deterministic-record sink is installed.
#[doc(hidden)]
pub fn forwarding_enabled() -> bool {
    FORWARDER.get().is_some()
}

/// Emits one deterministic record through tracing and the process-local sink. `target` is
/// the emitting module's path, so a forwarded record names the same module a record observed
/// through tracing does.
#[doc(hidden)]
pub fn emit_forwarded(target: &str, record_suffix: &str, message: fmt::Arguments<'_>) {
    tracing::info!("DETLOG {}{}", message, record_suffix);
    FORWARDER.get().expect("forwarder disappeared")(target, record_suffix, message);
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This is currently at the INFO log level.
#[macro_export]
macro_rules! detlog {
    (event = $event:expr; $($arg:tt)+) => {{
        if $crate::detlog::forwarding_enabled() || ::tracing::enabled!(::tracing::Level::INFO) {
            let record_suffix = $crate::detlog::record_suffix($event);
            if $crate::detlog::forwarding_enabled() {
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
/// `detlog!` routes to the process-local forwarder when one is installed and to
/// `tracing::info!` otherwise. `tracing` does not evaluate a macro's value
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
        $crate::detlog::forwarding_enabled() || ::tracing::enabled!(::tracing::Level::INFO)
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
        super::send_forwarded_record(sender, "detcore", "", format_args!("after"));
        let notice = receive();
        assert!(is_notice(&notice), "{notice}");
        assert_eq!(receive(), "INFO detcore: DETLOG after\n");

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
        assert_eq!(receive(), "INFO detcore: DETLOG small\n");
        // SAFETY: closing the two descriptors this test created.
        unsafe {
            libc::close(sender);
            libc::close(receiver);
        }
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
}
