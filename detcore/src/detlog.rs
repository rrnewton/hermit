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
use std::sync::OnceLock;

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

/// Severity of a [`ForwardedRecord`], mirroring [`tracing::Level`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForwardedLevel {
    /// [`tracing::Level::ERROR`]
    Error,
    /// [`tracing::Level::WARN`]
    Warn,
    /// [`tracing::Level::INFO`]
    Info,
    /// [`tracing::Level::DEBUG`]
    Debug,
    /// [`tracing::Level::TRACE`]
    Trace,
}

impl From<tracing::Level> for ForwardedLevel {
    fn from(level: tracing::Level) -> Self {
        match level {
            tracing::Level::ERROR => Self::Error,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::INFO => Self::Info,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::TRACE => Self::Trace,
        }
    }
}

impl From<ForwardedLevel> for tracing::Level {
    fn from(level: ForwardedLevel) -> Self {
        match level {
            ForwardedLevel::Error => Self::ERROR,
            ForwardedLevel::Warn => Self::WARN,
            ForwardedLevel::Info => Self::INFO,
            ForwardedLevel::Debug => Self::DEBUG,
            ForwardedLevel::Trace => Self::TRACE,
        }
    }
}

impl ForwardedLevel {
    /// Every level, most verbose first.
    pub const ALL: [Self; 5] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];

    /// The lowercase name [`str::parse`] accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl std::str::FromStr for ForwardedLevel {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str() == name)
            .ok_or_else(|| format!("{name:?} is not a Detcore record level"))
    }
}

/// One Detcore tracing event captured in a process that cannot reach the
/// run's log, for the coordinator to log at the point the guest reaches it.
///
/// A backend whose local tool runs inside the guest (SaBRe) sends these ahead
/// of each global request. The coordinator then logs them before handling that
/// request, which is where an in-coordinator local tool (ptrace) would already
/// have logged them. Target and level are kept so the logged record is the one
/// the local tool emitted, not a relabelled copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardedRecord {
    /// Severity of the original event.
    pub level: ForwardedLevel,
    /// Target of the original event, e.g. `detcore::random`.
    pub target: String,
    /// The event's fields, rendered as tracing-subscriber's default formatter
    /// renders them. ANSI sanitization is left to the logging subscriber.
    pub fields: String,
}

/// Whether `target` belongs to Detcore's log envelope.
pub fn is_detcore_target(target: &str) -> bool {
    target == "detcore" || target.starts_with("detcore::")
}

/// Renders event fields exactly as tracing-subscriber 0.3's `DefaultFields`
/// does with ANSI styling off, except for its message sanitization. The
/// subscriber that logs the forwarded record sanitizes the whole rendered
/// text, which reproduces the stock output unless a non-message field's
/// `Debug` output contains a raw terminal control character.
///
/// Events bridged from the `log` crate, the only ones carrying `log.` fields,
/// have the target `log` and are never captured, so no such field arrives here.
struct FieldRenderer<'a> {
    out: &'a mut String,
}

impl FieldRenderer<'_> {
    fn pad(&mut self) {
        if !self.out.is_empty() {
            self.out.push(' ');
        }
    }
}

impl tracing::field::Visit for FieldRenderer<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.record_debug(field, &format_args!("{value}"))
        } else {
            self.record_debug(field, &value)
        }
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        if let Some(source) = value.source() {
            struct Sources<'a>(&'a (dyn std::error::Error + 'static));
            impl fmt::Display for Sources<'_> {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    let mut list = f.debug_list();
                    let mut current = Some(self.0);
                    while let Some(error) = current {
                        list.entry(&format_args!("{error}"));
                        current = error.source();
                    }
                    list.finish()
                }
            }
            self.record_debug(
                field,
                &format_args!("{} {}.sources={}", value, field.name(), Sources(source)),
            )
        } else {
            self.record_debug(field, &format_args!("{value}"))
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        use std::fmt::Write as _;
        let name = field.name();
        self.pad();
        let _ = match name {
            "message" => write!(self.out, "{value:?}"),
            name => write!(
                self.out,
                "{}={value:?}",
                name.strip_prefix("r#").unwrap_or(name)
            ),
        };
    }
}

/// Render one event's fields for a [`ForwardedRecord`].
pub fn render_fields(event: &tracing::Event<'_>) -> String {
    let mut out = String::new();
    event.record(&mut FieldRenderer { out: &mut out });
    out
}

/// A global subscriber that captures Detcore events instead of logging them.
///
/// It keeps no thread-local state and takes no lock, so a record emitted from
/// a signal handler or after Rust TLS destruction has begun is still captured,
/// provided it is installed as the global default and no scoped dispatcher is
/// ever set in the process. Spans are not tracked.
pub struct RecordCapture {
    max_level: tracing::Level,
    push: fn(ForwardedRecord),
}

impl RecordCapture {
    /// Capture Detcore events at `max_level` and more severe into `push`.
    pub fn new(max_level: tracing::Level, push: fn(ForwardedRecord)) -> Self {
        Self { max_level, push }
    }

    fn captures(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= self.max_level && is_detcore_target(metadata.target())
    }
}

impl tracing::Subscriber for RecordCapture {
    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if self.captures(metadata) {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        self.captures(metadata)
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::from_level(
            self.max_level,
        ))
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let metadata = event.metadata();
        (self.push)(ForwardedRecord {
            level: (*metadata.level()).into(),
            target: metadata.target().to_owned(),
            fields: render_fields(event),
        });
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Drains the records this process captured, oldest first.
pub type RecordSource = fn() -> Vec<ForwardedRecord>;

/// Logs one record received from another process.
pub type RecordSink = fn(&ForwardedRecord);

static SOURCE: OnceLock<RecordSource> = OnceLock::new();
static SINK: OnceLock<RecordSink> = OnceLock::new();

/// Capture this process's Detcore events for the coordinator's log.
///
/// Installs [`RecordCapture`] as the global subscriber and registers `take` as
/// the source that global requests drain before they are sent. Fails if either
/// is already installed; a capture that silently lost records would make the
/// log look complete when it is not.
pub fn install_capture(
    max_level: ForwardedLevel,
    push: fn(ForwardedRecord),
    take: RecordSource,
) -> Result<(), String> {
    SOURCE
        .set(take)
        .map_err(|_| "a Detcore record source is already installed".to_owned())?;
    tracing::subscriber::set_global_default(RecordCapture::new(max_level.into(), push))
        .map_err(|error| format!("cannot install the Detcore record capture: {error}"))
}

/// Install `take` as this test binary's record source. Every caller must pass
/// the same function, since the source cannot be replaced.
#[cfg(test)]
pub(crate) fn install_test_source(take: RecordSource) {
    assert!(std::ptr::fn_addr_eq(*SOURCE.get_or_init(|| take), take));
}

/// Install `sink` as this test binary's record sink, as above.
#[cfg(test)]
pub(crate) fn install_test_sink(sink: RecordSink) {
    assert!(std::ptr::fn_addr_eq(*SINK.get_or_init(|| sink), sink));
}

/// Records captured since the previous call, oldest first; empty unless
/// [`install_capture`] ran in this process.
pub(crate) fn take_records() -> Vec<ForwardedRecord> {
    SOURCE.get().map_or_else(Vec::new, |take| take())
}

/// Registers how the coordinator logs records received from guest processes.
/// Only the first sink installed in a process is retained.
pub fn set_record_sink(sink: RecordSink) -> Result<(), RecordSink> {
    SINK.set(sink)
}

/// Log a record received from another process through the registered sink.
///
/// Without a sink the record is still logged, under the `detcore::forwarded`
/// target with its original target as a field, rather than dropped.
pub fn emit_record(record: &ForwardedRecord) {
    if let Some(sink) = SINK.get() {
        return sink(record);
    }
    let (origin, fields) = (&record.target, &record.fields);
    match tracing::Level::from(record.level) {
        tracing::Level::ERROR => {
            tracing::error!(target: "detcore::forwarded", origin = %origin, "{fields}")
        }
        tracing::Level::WARN => {
            tracing::warn!(target: "detcore::forwarded", origin = %origin, "{fields}")
        }
        tracing::Level::INFO => {
            tracing::info!(target: "detcore::forwarded", origin = %origin, "{fields}")
        }
        tracing::Level::DEBUG => {
            tracing::debug!(target: "detcore::forwarded", origin = %origin, "{fields}")
        }
        tracing::Level::TRACE => {
            tracing::trace!(target: "detcore::forwarded", origin = %origin, "{fields}")
        }
    }
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This is currently at the INFO log level.
#[macro_export]
macro_rules! detlog {
    (event = $event:expr; $($arg:tt)+) => {{
        if ::tracing::enabled!(::tracing::Level::INFO) {
            let record_suffix = $crate::detlog::record_suffix($event);
            ::tracing::info!("DETLOG {}{}", format_args!($($arg)+), record_suffix);
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
/// `detlog!` emits through `tracing::info!`. `tracing` does not evaluate a macro's value
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
        ::tracing::enabled!(::tracing::Level::INFO)
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
