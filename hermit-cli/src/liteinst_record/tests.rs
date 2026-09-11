use std::cell::Cell;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use tracing::Event;
use tracing::Subscriber;
use tracing::metadata::LevelFilter;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::FormatEvent;
use tracing_subscriber::fmt::format::FormatFields;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::registry::LookupSpan;

use super::*;

thread_local! {
    static FAIL_NEXT_PAYLOAD_ALLOCATION: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn before_payload_allocation() -> Result<(), std::collections::TryReserveError> {
    if !FAIL_NEXT_PAYLOAD_ALLOCATION.replace(false) {
        return Ok(());
    }
    Vec::<u8>::new().try_reserve(usize::MAX)
}

fn filter() -> EffectiveFilter {
    EffectiveFilter::from_directives_lossy("info", LevelFilter::INFO)
}

fn limits(event_bytes: usize, span_bytes: usize) -> FormatterLimits {
    FormatterLimits::new(event_bytes, span_bytes).unwrap()
}

struct ExactRecord;

impl<Registry, Fields> FormatEvent<Registry, Fields> for ExactRecord
where
    Registry: Subscriber + for<'lookup> LookupSpan<'lookup>,
    Fields: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, Registry, Fields>,
        mut writer: Writer<'_>,
        _event: &Event<'_>,
    ) -> fmt::Result {
        writer.write_str("exact record\n")
    }
}

#[test]
fn publishes_one_complete_exact_record() {
    let calls = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let received = calls.clone();
    let (subscriber, status) = subscriber_with_format(
        filter(),
        limits(64, 64),
        move |bytes: &[u8]| {
            received.lock().unwrap().push(bytes.to_vec());
            Ok(())
        },
        ExactRecord,
    );

    tracing::subscriber::with_default(subscriber, || tracing::info!("ignored by test formatter"));

    assert!(status.failure().is_none());
    assert_eq!(&*calls.lock().unwrap(), &[b"exact record\n".to_vec()]);
}

#[test]
fn oversized_initial_span_fields_fail_before_entering_the_cache() {
    let (subscriber, status) = record_subscriber(filter(), limits(1024, 8), |_bytes: &[u8]| Ok(()));

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("bounded", value = "long span value");
        let _entered = span.enter();
    });

    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Span,
            limit: 8
        })
    ));
}

#[test]
fn payload_allocation_failure_is_retained() {
    let (subscriber, status) = record_subscriber(
        filter(),
        limits(64, 64),
        |_bytes: &[u8]| -> io::Result<()> { Ok(()) },
    );
    FAIL_NEXT_PAYLOAD_ALLOCATION.set(true);

    tracing::subscriber::with_default(subscriber, || tracing::info!("allocation failure"));

    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Allocation)
    ));
}

#[test]
fn zero_formatter_limit_is_rejected() {
    assert!(FormatterLimits::new(0, 1).is_err());
    assert!(FormatterLimits::new(1, 0).is_err());
}
