//! Complete host event publication, without capture/session ownership.
//!
//! The pinned formatter renders once into fixed-capacity event storage; its
//! upstream TLS output buffer is not used. Span updates replace their cache only
//! after bounded formatting succeeds. Neither flush nor Drop commits a record.
//! The supplied sink must synchronously accept a complete record or return an
//! error; an error may have side effects and is never retried here. Ordering,
//! quotas, collector lifetime and finalization belong to the capture owner.
//! A local failure poisons future publication; already in-flight sink calls may
//! finish. The retained failure cannot be reset or treated as successful logs.

use std::cell::RefCell;
use std::fmt;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::FormattedFields;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format;
use tracing_subscriber::fmt::format::DefaultFields;
use tracing_subscriber::fmt::format::FormatEvent;
use tracing_subscriber::fmt::format::FormatFields;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::registry::LookupSpan;

use crate::liteinst_bootstrap::EffectiveFilter;

pub const EVENT_BYTES: usize = 1024 * 1024;
pub const SPAN_BYTES: usize = 64 * 1024;

/// Formatter payload budgets, not a bound on callback allocations, registry or
/// filter bookkeeping, allocator overhead, live spans or concurrent emitters.
#[derive(Clone, Copy, Debug)]
pub struct FormatterLimits {
    event_bytes: usize,
    span_bytes: usize,
}

impl FormatterLimits {
    pub fn new(event_bytes: usize, span_bytes: usize) -> io::Result<Self> {
        let envelope = span_bytes
            .checked_mul(2)
            .and_then(|spans| spans.checked_add(event_bytes))
            .and_then(|bytes| bytes.checked_add(8));
        if event_bytes == 0
            || span_bytes == 0
            || envelope.is_none_or(|bytes| bytes > isize::MAX as usize)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid formatter limits",
            ));
        }
        Ok(Self {
            event_bytes,
            span_bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferKind {
    Event,
    Span,
}

/// A sticky failure, not a capture-completion or deterministic-order verdict.
#[derive(Debug)]
pub enum RecordFailure {
    Formatting,
    Sink(io::Error),
    Reentrant,
    Unwinding,
    Size { buffer: BufferKind, limit: usize },
    Allocation,
}

impl fmt::Display for RecordFailure {
    fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Formatting => writer.write_str("host record formatting failed"),
            Self::Sink(error) => write!(writer, "host record sink failed: {error}"),
            Self::Reentrant => writer.write_str("reentrant host record publication"),
            Self::Unwinding => writer.write_str("host record formatting or publication unwound"),
            Self::Size { buffer, limit } => {
                write!(writer, "{buffer:?} formatter exceeded {limit} bytes")
            }
            Self::Allocation => writer.write_str("formatter storage allocation failed"),
        }
    }
}

impl std::error::Error for RecordFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sink(error) => Some(error),
            _ => None,
        }
    }
}

/// Retain this independently of the subscriber, including through its Drop.
#[derive(Clone, Default)]
pub struct RecordStatus(
    Arc<Mutex<Option<Arc<RecordFailure>>>>,
    Arc<OnceLock<Box<dyn Fn() + Send + Sync>>>,
);

impl RecordStatus {
    pub fn failure(&self) -> Option<Arc<RecordFailure>> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn fail(&self, failure: RecordFailure) {
        let mut stored = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if stored.is_none() {
            *stored = Some(Arc::new(failure));
            drop(stored);
            if let Some(notify) = self.1.get() {
                notify();
            }
        } else {
            drop(stored);
            drop(failure);
        }
    }

    fn formatting(&self, action: impl FnOnce() -> fmt::Result) -> fmt::Result {
        let attempt = Attempt(self);
        let result = action();
        if result.is_err() {
            self.fail(RecordFailure::Formatting);
        }
        drop(attempt);
        result
    }
}

struct Attempt<'status>(&'status RecordStatus);

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.fail(RecordFailure::Unwinding);
        }
    }
}

struct BoundedBuffer<'status> {
    bytes: Box<[u8]>,
    used: usize,
    failed: bool,
    kind: BufferKind,
    status: &'status RecordStatus,
}

fn reserve_payload(
    bytes: &mut Vec<u8>,
    limit: usize,
) -> Result<(), std::collections::TryReserveError> {
    #[cfg(test)]
    tests::before_payload_allocation()?;
    bytes.try_reserve_exact(limit)
}

impl<'status> BoundedBuffer<'status> {
    fn new(
        limit: usize,
        kind: BufferKind,
        status: &'status RecordStatus,
    ) -> Result<Self, fmt::Error> {
        let mut bytes = Vec::new();
        if reserve_payload(&mut bytes, limit).is_err() {
            status.fail(RecordFailure::Allocation);
            return Err(fmt::Error);
        }
        bytes.resize(limit, 0);
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            used: 0,
            failed: false,
            kind,
            status,
        })
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.used])
            .expect("only complete UTF-8 strings were copied")
    }

    fn into_string(self) -> String {
        let mut bytes = self.bytes.into_vec();
        bytes.truncate(self.used);
        String::from_utf8(bytes).expect("only complete UTF-8 strings were copied")
    }
}

impl fmt::Write for BoundedBuffer<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.failed {
            return Err(fmt::Error);
        }
        let end = self.used.checked_add(text.len());
        let Some(end) = end.filter(|&end| end <= self.bytes.len()) else {
            self.failed = true;
            self.status.fail(RecordFailure::Size {
                buffer: self.kind,
                limit: self.bytes.len(),
            });
            return Err(fmt::Error);
        };
        self.bytes[self.used..end].copy_from_slice(text.as_bytes());
        self.used = end;
        Ok(())
    }
}

thread_local! {
    static ACTIVE: RefCell<Vec<(usize, Stage)>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Event,
    Fields,
    Sink,
}

struct Entered;

impl Entered {
    fn new(status: &RecordStatus, stage: Stage) -> Result<Self, fmt::Error> {
        if status.failure().is_some() {
            return Err(fmt::Error);
        }
        let identity = Arc::as_ptr(&status.0) as usize;
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active.iter().any(|&(owner, active_stage)| {
                owner == identity && !(stage == Stage::Fields && active_stage == Stage::Event)
            }) {
                status.fail(RecordFailure::Reentrant);
                return Err(fmt::Error);
            }
            active.push((identity, stage));
            Ok(Self)
        })
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.borrow_mut().pop();
        });
    }
}

struct CheckedFormat<Format, Sink> {
    inner: Format,
    status: RecordStatus,
    limits: FormatterLimits,
    writer: RecordWriterFactory<Sink>,
}

impl<Registry, Fields, Format, Sink> FormatEvent<Registry, Fields> for CheckedFormat<Format, Sink>
where
    Registry: Subscriber + for<'lookup> LookupSpan<'lookup>,
    Fields: for<'writer> FormatFields<'writer> + 'static,
    Format: FormatEvent<Registry, Fields>,
    Sink: Fn(&[u8]) -> io::Result<()>,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, Registry, Fields>,
        _writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let entered = Entered::new(&self.status, Stage::Event)?;
        let mut buffer =
            BoundedBuffer::new(self.limits.event_bytes, BufferKind::Event, &self.status)?;
        let bounded = Writer::new(&mut buffer);
        self.status
            .formatting(|| self.inner.format_event(context, bounded, event))?;
        if self.status.failure().is_some() {
            return Err(fmt::Error);
        }
        drop(entered);
        self.writer
            .write_record(buffer.text().as_bytes(), self.limits.event_bytes)
            .map_err(|_| fmt::Error)
    }
}

/// Poison publication on a field error rather than invoking fmt's span-error
/// fallback, which re-formats the broken value directly to stderr. An internal
/// failed new span has an empty cache; a failed update preserves the old cache.
struct CheckedFields {
    status: RecordStatus,
    limits: FormatterLimits,
}

impl<'writer> FormatFields<'writer> for CheckedFields {
    fn format_fields<Fields: RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        fields: Fields,
    ) -> fmt::Result {
        let Ok(_entered) = Entered::new(&self.status, Stage::Fields) else {
            return Ok(());
        };
        let Ok(mut buffer) =
            BoundedBuffer::new(self.limits.span_bytes, BufferKind::Span, &self.status)
        else {
            return Ok(());
        };
        let bounded = Writer::new(&mut buffer);
        if self
            .status
            .formatting(|| DefaultFields::new().format_fields(bounded, fields))
            .is_ok()
            && self.status.failure().is_none()
        {
            fmt::Write::write_str(&mut writer, buffer.text())?;
        }
        Ok(())
    }

    fn add_fields(
        &self,
        current: &'writer mut FormattedFields<Self>,
        fields: &tracing::span::Record<'_>,
    ) -> fmt::Result {
        use std::fmt::Write as _;

        let Ok(_entered) = Entered::new(&self.status, Stage::Fields) else {
            return Ok(());
        };
        let Ok(mut buffer) =
            BoundedBuffer::new(self.limits.span_bytes, BufferKind::Span, &self.status)
        else {
            return Ok(());
        };
        if buffer.write_str(&current.fields).is_err()
            || (!current.fields.is_empty() && buffer.write_str(" ").is_err())
        {
            return Ok(());
        }
        let bounded = Writer::new(&mut buffer);
        if self
            .status
            .formatting(|| DefaultFields::new().format_fields(bounded, fields))
            .is_ok()
            && self.status.failure().is_none()
        {
            current.fields = buffer.into_string();
        }
        Ok(())
    }
}

struct RecordWriter<'sink, Sink> {
    sink: &'sink Sink,
    status: &'sink RecordStatus,
}

impl<Sink: Fn(&[u8]) -> io::Result<()>> Write for RecordWriter<'_, Sink> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes).map(|()| bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let _entered = Entered::new(self.status, Stage::Sink)
            .map_err(|_| io::Error::other("reentrant host record publication"))?;
        let _attempt = Attempt(self.status);
        match (self.sink)(bytes) {
            Ok(()) => Ok(()),
            Err(error) => {
                let kind = error.kind();
                self.status.fail(RecordFailure::Sink(error));
                Err(io::Error::new(kind, "host record sink failed"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RecordWriterFactory<Sink> {
    sink: Sink,
    status: RecordStatus,
}

struct EmptyOutput(RecordStatus);

impl Write for EmptyOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.0.fail(RecordFailure::Formatting);
        Err(io::Error::other("unexpected upstream formatter output"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<Sink: Fn(&[u8]) -> io::Result<()>> RecordWriterFactory<Sink> {
    fn write_record(&self, bytes: &[u8], limit: usize) -> io::Result<()> {
        if bytes.len() > limit {
            self.status.fail(RecordFailure::Size {
                buffer: BufferKind::Event,
                limit,
            });
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized formatted record",
            ));
        }
        self.make_writer().write_all(bytes)
    }
}

impl<'sink, Sink: Fn(&[u8]) -> io::Result<()> + 'sink> MakeWriter<'sink>
    for RecordWriterFactory<Sink>
{
    type Writer = RecordWriter<'sink, Sink>;

    fn make_writer(&'sink self) -> Self::Writer {
        RecordWriter {
            sink: &self.sink,
            status: &self.status,
        }
    }
}

/// Prepare but do not install a subscriber. Captured records are intentionally
/// ANSI-free, so the public `Writer::new` settings used for bounded formatting
/// exactly match the subscriber writer settings. No destination or global state
/// is opened. The caller must inspect the retained status after emitter
/// quiescence; `None` is only absence of a local failure so far, not a qualified
/// log receipt.
#[cfg(test)]
pub fn record_subscriber<Sink>(
    filter: EffectiveFilter,
    limits: FormatterLimits,
    sink: Sink,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    subscriber_with_format(filter, limits, sink, format())
}

pub fn record_subscriber_with_failure<Sink>(
    filter: EffectiveFilter,
    limits: FormatterLimits,
    sink: Sink,
    failure: impl Fn() + Send + Sync + 'static,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    let status = RecordStatus::default();
    let _ = status.1.set(Box::new(failure));
    subscriber_with_status(filter, limits, sink, format(), status)
}

#[cfg(test)]
fn subscriber_with_format<Sink, Format>(
    filter: EffectiveFilter,
    limits: FormatterLimits,
    sink: Sink,
    formatter: Format,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    Format: FormatEvent<tracing_subscriber::Registry, CheckedFields> + Send + Sync + 'static,
{
    let status = RecordStatus::default();
    subscriber_with_status(filter, limits, sink, formatter, status)
}

fn subscriber_with_status<Sink, Format>(
    filter: EffectiveFilter,
    limits: FormatterLimits,
    sink: Sink,
    formatter: Format,
    status: RecordStatus,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    Format: FormatEvent<tracing_subscriber::Registry, CheckedFields> + Send + Sync + 'static,
{
    let output_status = status.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .log_internal_errors(false)
        .fmt_fields(CheckedFields {
            status: status.clone(),
            limits,
        })
        .event_format(CheckedFormat {
            inner: formatter,
            status: status.clone(),
            limits,
            writer: RecordWriterFactory {
                sink,
                status: status.clone(),
            },
        })
        .with_env_filter(filter.into_filter())
        .with_writer(move || EmptyOutput(output_status.clone()))
        .finish();
    (subscriber, status)
}

#[cfg(test)]
#[path = "liteinst_record/tests.rs"]
mod tests;
