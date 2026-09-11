use std::panic::AssertUnwindSafe;
use std::sync::Barrier;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use super::*;

type Records = Arc<Mutex<Vec<Vec<u8>>>>;

fn baseline_limits() -> FormatterLimits {
    FormatterLimits::new(4096, 1024).unwrap()
}

fn record_subscriber<Sink>(
    filter: EffectiveFilter,
    ansi: bool,
    sink: Sink,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
{
    super::record_subscriber(filter, ansi, baseline_limits(), sink)
}

fn subscriber_with_format<Sink, Format>(
    filter: EffectiveFilter,
    ansi: bool,
    sink: Sink,
    formatter: Format,
) -> (impl Subscriber + Send + Sync, RecordStatus)
where
    Sink: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    Format: FormatEvent<tracing_subscriber::Registry, CheckedFields> + Send + Sync + 'static,
{
    super::subscriber_with_format(filter, ansi, baseline_limits(), sink, formatter)
}

fn filter() -> EffectiveFilter {
    EffectiveFilter::from_directives_lossy("host_record=info", tracing::metadata::LevelFilter::OFF)
}

#[test]
fn failed_complete_record_notifies_transport_once_before_returning() {
    let records = Records::default();
    let notifications = Arc::new(AtomicUsize::new(0));
    let seen = notifications.clone();
    let (subscriber, status) = super::record_subscriber_with_failure(
        filter(),
        false,
        baseline_limits(),
        capture(&records),
        move || {
            seen.fetch_add(1, Ordering::SeqCst);
        },
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", broken = ?BrokenFormat);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        tracing::info!(target: "host_record", "refused-after-failure");
    });
    assert!(status.failure().is_some());
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    assert!(records.lock().unwrap().is_empty());
}

fn capture(records: &Records) -> impl Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static {
    let records = records.clone();
    move |bytes| {
        records.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct FixedTime;

impl tracing_subscriber::fmt::time::FormatTime for FixedTime {
    fn format_time(&self, writer: &mut Writer<'_>) -> fmt::Result {
        writer.write_str("unchanged-time")
    }
}

struct Chunks;

impl fmt::Debug for Chunks {
    fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
        writer.write_str("first")?;
        writer.write_str("\nsecond")?;
        writer.write_str("\x1b[31mthird")
    }
}

fn structured_events() {
    let span =
        tracing::info_span!(target: "host_record", "work", task = 7, later = tracing::field::Empty);
    let _entered = span.enter();
    tracing::info!(target: "host_record", chunks = ?Chunks, number = 1.0, "multi\nline");
    span.record("later", 9);
    tracing::info!(target: "host_record", "after span update");
    tracing::debug!(target: "host_record", "must remain filtered");
    tracing::warn!(target: "other_record", "must remain filtered too");
}

#[test]
fn real_formatter_bytes_fields_filter_and_ansi_are_unchanged() {
    #[derive(Clone)]
    struct Bytes(Arc<Mutex<Vec<u8>>>);
    impl Write for Bytes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    for ansi in [false, true] {
        let expected = Bytes(Arc::default());
        let writer = expected.clone();
        let original = tracing_subscriber::fmt()
            .with_timer(FixedTime)
            .with_env_filter(filter().into_filter())
            .with_ansi(ansi)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(original, structured_events);

        let records = Records::default();
        let (subscriber, status) = subscriber_with_format(
            filter(),
            ansi,
            capture(&records),
            format().with_timer(FixedTime),
        );
        tracing::subscriber::with_default(subscriber, structured_events);
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records.concat(), *expected.0.lock().unwrap());
        assert!(records[0].windows(6).any(|bytes| bytes == b"second"));
        let updated_field: &[u8] = if ansi {
            b"\x1b[3mlater\x1b[0m\x1b[2m=\x1b[0m9"
        } else {
            b"later=9"
        };
        assert!(
            records[1]
                .windows(updated_field.len())
                .any(|bytes| bytes == updated_field)
        );
        assert!(status.failure().is_none());
    }
}

#[test]
fn public_preparation_is_scoped_and_drop_does_not_publish() {
    let records = Records::default();
    let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
    assert!(records.lock().unwrap().is_empty());
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "public host record");
    });
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert!(
        String::from_utf8_lossy(&records[0]).ends_with("INFO host_record: public host record\n")
    );
    assert!(status.failure().is_none());
}

struct BrokenFormat;

impl fmt::Debug for BrokenFormat {
    fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
        writer.write_str("partial format must not commit")?;
        Err(fmt::Error)
    }
}

#[test]
fn formatting_error_rejects_partial_and_later_records() {
    let records = Records::default();
    let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", broken = ?BrokenFormat);
        tracing::info!(target: "host_record", "not silently recovered");
    });
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Formatting)
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn span_creation_and_update_errors_are_inspectable() {
    for update in [false, true] {
        let records = Records::default();
        let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
        tracing::subscriber::with_default(subscriber, || {
            if update {
                let span = tracing::info_span!(target: "host_record", "broken", value = tracing::field::Empty);
                span.record("value", tracing::field::debug(BrokenFormat));
                let _entered = span.enter();
                tracing::info!(target: "host_record", "in malformed span");
            } else {
                let span =
                    tracing::info_span!(target: "host_record", "broken", value = ?BrokenFormat);
                let _entered = span.enter();
                tracing::info!(target: "host_record", "in malformed span");
            }
        });
        assert!(matches!(
            status.failure().as_deref(),
            Some(RecordFailure::Formatting)
        ));
        assert!(records.lock().unwrap().is_empty());
    }
}

#[test]
fn partial_sink_error_is_sticky_and_never_retried() {
    struct Partial(Vec<u8>);
    impl Write for Partial {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                self.0.extend_from_slice(&bytes[..3]);
                Ok(3)
            } else {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "after prefix"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let partial = Arc::new(Mutex::new(Partial(Vec::new())));
    let attempted = Records::default();
    let attempts = Arc::new(AtomicUsize::new(0));
    let sink = partial.clone();
    let calls = attempts.clone();
    let attempted_record = attempted.clone();
    let (subscriber, status) = record_subscriber(filter(), false, move |bytes| {
        calls.fetch_add(1, Ordering::Relaxed);
        attempted_record.lock().unwrap().push(bytes.to_vec());
        sink.lock().unwrap().write_all(bytes)
    });
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "partially accepted");
        tracing::info!(target: "host_record", "no automatic replay");
    });
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert_eq!(partial.lock().unwrap().0.len(), 3);
    assert_eq!(attempted.lock().unwrap().len(), 1);
    assert_eq!(partial.lock().unwrap().0, attempted.lock().unwrap()[0][..3]);
    let failure = status.failure().unwrap();
    let RecordFailure::Sink(error) = failure.as_ref() else {
        panic!("missing sink cause")
    };
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(error.to_string(), "after prefix");
}

#[test]
fn interrupted_record_commit_is_not_retried_like_a_byte_write() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let (subscriber, status) = record_subscriber(filter(), false, move |_| {
        calls.fetch_add(1, Ordering::Relaxed);
        Err(io::Error::from(io::ErrorKind::Interrupted))
    });
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "uncertain commit");
    });
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert!(
        matches!(status.failure().as_deref(), Some(RecordFailure::Sink(error)) if error.kind() == io::ErrorKind::Interrupted)
    );
}

#[test]
fn unused_writer_flush_and_drop_do_not_commit_or_finalize() {
    let records = Records::default();
    let factory = RecordWriterFactory {
        sink: capture(&records),
        status: RecordStatus::default(),
    };
    {
        let mut writer = factory.make_writer();
        writer.flush().unwrap();
    }
    assert!(records.lock().unwrap().is_empty());
    {
        let mut writer = factory.make_writer();
        writer.write_all(b"complete\nmultiline\n").unwrap();
        writer.flush().unwrap();
    }
    assert_eq!(
        *records.lock().unwrap(),
        [b"complete\nmultiline\n".to_vec()]
    );
}

#[test]
fn concurrent_records_are_distinct_not_a_determinism_claim() {
    let records = Records::default();
    let (subscriber, status) =
        subscriber_with_format(filter(), false, capture(&records), format().without_time());
    let dispatch = tracing::Dispatch::new(subscriber);
    let barrier = Arc::new(Barrier::new(4));
    std::thread::scope(|scope| {
        for producer in 0..4 {
            let barrier = barrier.clone();
            let dispatch = dispatch.clone();
            scope.spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    barrier.wait();
                    for sequence in 0..16 {
                        tracing::info!(target: "host_record", producer, sequence, "distinct");
                    }
                });
            });
        }
    });
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 64);
    for producer in 0..4 {
        for sequence in 0..16 {
            let expected =
                format!(" INFO host_record: distinct producer={producer} sequence={sequence}\n");
            assert_eq!(
                records
                    .iter()
                    .filter(|record| **record == expected.as_bytes())
                    .count(),
                1
            );
        }
    }
    assert!(status.failure().is_none());
}

#[test]
fn formatting_unwind_is_preserved_and_cannot_publish_stale_buffer() {
    struct Panics;
    impl fmt::Debug for Panics {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            writer.write_str("unfinished")?;
            panic!("format panic")
        }
    }
    let records = Records::default();
    let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
    tracing::subscriber::with_default(subscriber, || {
        assert!(
            std::panic::catch_unwind(|| tracing::info!(target: "host_record", value = ?Panics))
                .is_err()
        );
        tracing::info!(target: "host_record", "must not inherit stale formatting");
    });
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Unwinding)
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn sink_unwind_is_preserved_and_status_outlives_subscriber() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let (subscriber, status) = record_subscriber(filter(), false, move |_| {
        calls.fetch_add(1, Ordering::Relaxed);
        panic!("sink panic")
    });
    assert!(
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            tracing::subscriber::with_default(
                subscriber,
                || tracing::info!(target: "host_record", "panics"),
            );
        }))
        .is_err()
    );
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Unwinding)
    ));
}

#[test]
fn reentry_is_rejected_without_holding_a_sink_lock() {
    let status = RecordStatus::default();
    let records = Records::default();
    let sink = capture(&records);
    let nested_status = status.clone();
    let factory = RecordWriterFactory {
        status: status.clone(),
        sink: move |_: &[u8]| {
            let nested = RecordWriterFactory {
                sink: &sink,
                status: nested_status.clone(),
            };
            assert!(nested.make_writer().write_all(b"recursive").is_err());
            Ok(())
        },
    };
    factory.make_writer().write_all(b"outer").unwrap();
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Reentrant)
    ));
    assert!(records.lock().unwrap().is_empty());
    assert!(Entered::new(&RecordStatus::default(), Stage::Event).is_ok());
}

#[test]
fn real_formatting_reentry_poison_prevents_outer_commit() {
    struct Reenters(RecordStatus);
    impl fmt::Debug for Reenters {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            let nested = RecordWriterFactory {
                status: self.0.clone(),
                sink: |_: &[u8]| panic!("reentrant sink must not be reached"),
            };
            assert!(nested.make_writer().write_all(b"recursive").is_err());
            writer.write_str("outer formatting continued")
        }
    }
    let records = Records::default();
    let (subscriber, status) = record_subscriber(filter(), false, capture(&records));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", value = ?Reenters(status.clone()));
    });
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Reentrant)
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn empty_complete_event_is_one_record_not_drop_or_flush() {
    struct Empty;
    impl FormatEvent<tracing_subscriber::Registry, CheckedFields> for Empty {
        fn format_event(
            &self,
            _: &FmtContext<'_, tracing_subscriber::Registry, CheckedFields>,
            _: Writer<'_>,
            _: &Event<'_>,
        ) -> fmt::Result {
            Ok(())
        }
    }
    let records = Records::default();
    let (subscriber, status) = subscriber_with_format(filter(), false, capture(&records), Empty);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "formatted empty");
    });
    assert_eq!(*records.lock().unwrap(), [Vec::<u8>::new()]);
    assert!(status.failure().is_none());
}

#[test]
fn later_sink_error_is_dropped_without_holding_the_status_lock() {
    struct ObserveDrop {
        status: RecordStatus,
        observed: Arc<AtomicUsize>,
    }
    impl fmt::Debug for ObserveDrop {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            writer.write_str("later error")
        }
    }
    impl fmt::Display for ObserveDrop {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Debug::fmt(self, writer)
        }
    }
    impl std::error::Error for ObserveDrop {}
    impl Drop for ObserveDrop {
        fn drop(&mut self) {
            assert!(self.status.failure().is_some());
            self.observed.fetch_add(1, Ordering::Relaxed);
        }
    }
    let status = RecordStatus::default();
    let observed = Arc::new(AtomicUsize::new(0));
    status.fail(RecordFailure::Formatting);
    status.fail(RecordFailure::Sink(io::Error::other(ObserveDrop {
        status: status.clone(),
        observed: observed.clone(),
    })));
    assert_eq!(observed.load(Ordering::Relaxed), 1);
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Formatting)
    ));
}

fn bounded_subscriber(
    records: &Records,
    event_bytes: usize,
    span_bytes: usize,
) -> (impl Subscriber + Send + Sync, RecordStatus) {
    super::subscriber_with_format(
        filter(),
        false,
        FormatterLimits::new(event_bytes, span_bytes).unwrap(),
        capture(records),
        format().without_time(),
    )
}

fn cached_span(span: &tracing::Span) -> (String, usize) {
    span.with_subscriber(|(identity, dispatch)| {
        let registry = dispatch
            .downcast_ref::<tracing_subscriber::Registry>()
            .unwrap();
        let span = registry.span(identity).unwrap();
        let extensions = span.extensions();
        let fields = extensions.get::<FormattedFields<CheckedFields>>().unwrap();
        (fields.fields.clone(), fields.fields.capacity())
    })
    .unwrap()
}

#[test]
fn formatter_limits_refuse_zero_and_overflow_without_allocating() {
    for (event, span) in [
        (0, 1),
        (1, 0),
        (usize::MAX, 1),
        (1, usize::MAX),
        (isize::MAX as usize, 1),
    ] {
        assert_eq!(
            FormatterLimits::new(event, span).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(FormatterLimits::new(1, 1).is_ok());
}

#[test]
fn fixed_storage_rejects_before_copy_and_stays_poisoned() {
    use std::fmt::Write as _;

    let status = RecordStatus::default();
    let mut buffer = BoundedBuffer::new(3, BufferKind::Event, &status).unwrap();
    assert_eq!(buffer.bytes.len(), 3);
    buffer.write_str("é").unwrap();
    assert_eq!(buffer.text(), "é");
    assert!(buffer.write_str("é").is_err());
    assert_eq!(buffer.text(), "é");
    assert_eq!(buffer.bytes.len(), 3);
    assert!(buffer.write_str("x").is_err());
    assert_eq!(buffer.bytes[2], 0);
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 3
        })
    ));
}

#[test]
fn fixed_storage_moves_to_string_without_capacity_growth() {
    use std::fmt::Write as _;

    for capacity in [1, 3, 8, 31, 1024] {
        let status = RecordStatus::default();
        let mut buffer = BoundedBuffer::new(capacity, BufferKind::Span, &status).unwrap();
        buffer.write_str("x").unwrap();
        let text = buffer.into_string();
        assert_eq!(text, "x");
        assert_eq!(text.capacity(), capacity);
        assert!(status.failure().is_none());
    }
}

#[test]
fn real_event_exact_boundary_and_one_byte_overflow() {
    let expected = b" INFO host_record: exact\n";
    for limit in [expected.len(), expected.len() - 1] {
        let records = Records::default();
        let (subscriber, status) = bounded_subscriber(&records, limit, 32);
        tracing::subscriber::with_default(
            subscriber,
            || tracing::info!(target: "host_record", "exact"),
        );
        if limit == expected.len() {
            assert_eq!(*records.lock().unwrap(), [expected.to_vec()]);
            assert!(status.failure().is_none());
        } else {
            assert!(records.lock().unwrap().is_empty());
            assert!(
                matches!(status.failure().as_deref(), Some(RecordFailure::Size { buffer: BufferKind::Event, limit: found }) if *found == limit)
            );
        }
    }
}

#[test]
fn oversized_single_field_refuses_without_partial_sink_record() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 32, 32);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "{}", "x".repeat(256));
        tracing::info!(target: "host_record", "later");
    });
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 32
        })
    ));
}

#[test]
fn ignored_chunk_errors_cannot_turn_oversize_into_success() {
    struct Ignores(Arc<AtomicUsize>);
    impl fmt::Debug for Ignores {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            for _ in 0..16 {
                self.0.fetch_add(1, Ordering::Relaxed);
                let _ = writer.write_str("xxxxxxxxxxxxxxxx");
            }
            Ok(())
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 64, 32);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", value = ?Ignores(calls.clone()));
    });
    assert_eq!(calls.load(Ordering::Relaxed), 16);
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 64
        })
    ));
}

#[test]
fn span_cache_exact_total_bound_and_failed_update_is_unchanged() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 7);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", v = 1);
        let initial = cached_span(&span);
        assert_eq!(initial.0, "v=1");
        assert!(initial.1 <= 8);
        span.record("v", 2);
        assert_eq!(cached_span(&span), ("v=1 v=2".to_owned(), 7));
        let _entered = span.enter();
        tracing::info!(target: "host_record", "at boundary");
        span.record("v", 3);
        assert_eq!(cached_span(&span), ("v=1 v=2".to_owned(), 7));
        tracing::info!(target: "host_record", "must not publish");
    });
    assert_eq!(
        *records.lock().unwrap(),
        [b" INFO small{v=1 v=2}: host_record: at boundary\n".to_vec()]
    );
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Span,
            limit: 7
        })
    ));
}

#[test]
fn span_update_counts_separator_before_appending() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 3);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", v = 1);
        let before = cached_span(&span);
        span.record("v", 2);
        assert_eq!(cached_span(&span), before);
    });
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Span,
            limit: 3
        })
    ));
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn oversized_initial_span_and_updates_never_keep_partial_bytes() {
    for update in [false, true] {
        let records = Records::default();
        let (subscriber, status) = bounded_subscriber(&records, 128, 8);
        tracing::subscriber::with_default(subscriber, || {
            if update {
                let span = tracing::info_span!(target: "host_record", "small", value = tracing::field::Empty);
                let before = cached_span(&span);
                span.record("value", "far too large for the span cache");
                assert_eq!(cached_span(&span), before);
            } else {
                let span = tracing::info_span!(target: "host_record", "small", value = "far too large for the span cache");
                assert_eq!(cached_span(&span), (String::new(), 0));
            }
            tracing::info!(target: "host_record", "refused");
        });
        assert!(matches!(
            status.failure().as_deref(),
            Some(RecordFailure::Size {
                buffer: BufferKind::Span,
                limit: 8
            })
        ));
        assert!(records.lock().unwrap().is_empty());
    }
}

#[test]
fn utf8_span_exact_boundary_preserves_whole_value() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 6);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", v = "é");
        assert_eq!(cached_span(&span).0, "v=\"é\"");
        let _entered = span.enter();
        tracing::info!(target: "host_record", "UTF-8");
    });
    assert!(status.failure().is_none());
    assert_eq!(
        *records.lock().unwrap(),
        [" INFO small{v=\"é\"}: host_record: UTF-8\n"
            .as_bytes()
            .to_vec()]
    );
}

#[test]
fn formatting_occurs_once_per_new_value_not_a_sizing_pass() {
    struct Once(Arc<AtomicUsize>);
    impl fmt::Debug for Once {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            let observed = self.0.fetch_add(1, Ordering::Relaxed);
            write!(writer, "value-{observed}")
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 256, 128);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "once", value = ?Once(calls.clone()));
        span.record("value", tracing::field::debug(Once(calls.clone())));
        let _entered = span.enter();
        tracing::info!(target: "host_record", value = ?Once(calls.clone()), "once");
    });
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert!(status.failure().is_none());
    assert_eq!(
        *records.lock().unwrap(),
        [b" INFO once{value=value-0 value=value-1}: host_record: once value=value-2\n".to_vec()]
    );
}

#[test]
fn commit_adapter_also_refuses_oversize_before_sink() {
    let records = Records::default();
    let factory = RecordWriterFactory {
        sink: capture(&records),
        status: RecordStatus::default(),
    };
    assert!(factory.write_record(b"too large", 3).is_err());
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        factory.status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 3
        })
    ));
}

#[test]
fn event_fields_are_not_subject_to_the_smaller_span_budget() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 1);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", value = 123456, "event only");
    });
    assert!(status.failure().is_none());
    assert_eq!(
        *records.lock().unwrap(),
        [b" INFO host_record: event only value=123456\n".to_vec()]
    );
}

#[test]
fn cached_span_prefixes_are_included_in_the_event_budget() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 32, 16);
    tracing::subscriber::with_default(subscriber, || {
        let outer = tracing::info_span!(target: "host_record", "outer", value = 1);
        let _outer = outer.enter();
        let inner = tracing::info_span!(target: "host_record", "inner", value = 2);
        let _inner = inner.enter();
        assert_eq!(cached_span(&outer).0, "value=1");
        assert_eq!(cached_span(&inner).0, "value=2");
        tracing::info!(target: "host_record", "too much combined output");
    });
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Size {
            buffer: BufferKind::Event,
            limit: 32
        })
    ));
}

#[test]
fn ignored_span_errors_cannot_commit_partial_cache() {
    struct Ignores;
    impl fmt::Debug for Ignores {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            for _ in 0..8 {
                let _ = writer.write_str("xxxxxxxx");
            }
            Ok(())
        }
    }
    for update in [false, true] {
        let records = Records::default();
        let (subscriber, status) = bounded_subscriber(&records, 128, 16);
        tracing::subscriber::with_default(subscriber, || {
            if update {
                let span = tracing::info_span!(target: "host_record", "small", value = 1);
                let before = cached_span(&span);
                span.record("value", tracing::field::debug(Ignores));
                assert_eq!(cached_span(&span), before);
            } else {
                let span = tracing::info_span!(target: "host_record", "small", value = ?Ignores);
                assert_eq!(cached_span(&span), (String::new(), 0));
            }
        });
        assert!(records.lock().unwrap().is_empty());
        assert!(matches!(
            status.failure().as_deref(),
            Some(RecordFailure::Size {
                buffer: BufferKind::Span,
                limit: 16
            })
        ));
    }
}

#[test]
fn panicking_span_update_matches_upstream_poisoning_and_keeps_failure() {
    let expected_poisoning = match std::env::var("HERMIT_FORMATTER_TEST_REGISTRY_LOCK") {
        Ok(lock) if lock == "std" => Some(true),
        Ok(lock) if lock == "parking_lot" => Some(false),
        Err(std::env::VarError::NotPresent) => None,
        other => panic!("invalid formatter test registry lock contract: {other:?}"),
    };
    assert_span_update_panic_contract(expected_poisoning);
}

fn assert_span_update_panic_contract(expected_poisoning: Option<bool>) {
    struct Panics;
    impl fmt::Debug for Panics {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            writer.write_str("partial")?;
            panic!("span update panic")
        }
    }
    fn exercise() -> (String, bool) {
        let span = tracing::info_span!(target: "host_record", "small", value = 1);
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            span.record("value", tracing::field::debug(Panics));
        }))
        .unwrap_err();
        let message = panic.downcast_ref::<&str>().unwrap().to_string();
        let poisoned = std::panic::catch_unwind(AssertUnwindSafe(|| {
            span.with_subscriber(|(identity, dispatch)| {
                let registry = dispatch
                    .downcast_ref::<tracing_subscriber::Registry>()
                    .unwrap();
                let span = registry.span(identity).unwrap();
                let _extensions = span.extensions();
            });
        }))
        .is_err();
        (message, poisoned)
    }

    let original = tracing_subscriber::fmt()
        .with_env_filter(filter().into_filter())
        .with_writer(io::sink)
        .finish();
    let baseline = tracing::subscriber::with_default(original, exercise);
    assert_eq!(baseline.0, "span update panic");
    if let Some(expected) = expected_poisoning {
        assert_eq!(baseline.1, expected);
    }
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    tracing::subscriber::with_default(subscriber, || {
        assert_eq!(exercise(), baseline);
        tracing::info!(target: "host_record", "not a recovered record");
    });
    assert!(records.lock().unwrap().is_empty());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Unwinding)
    ));
}

#[test]
fn bounded_fields_preserve_ansi_and_sanitization_modes() {
    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    for ansi in [false, true] {
        for sanitize in [false, true] {
            let expected = Output::default();
            let output = expected.clone();
            let original = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(ansi)
                .with_ansi_sanitization(sanitize)
                .with_env_filter(filter().into_filter())
                .with_writer(move || output.clone())
                .finish();
            tracing::subscriber::with_default(original, structured_events);
            let actual = Output::default();
            let output = actual.clone();
            let status = RecordStatus::default();
            let checked = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(ansi)
                .with_ansi_sanitization(sanitize)
                .fmt_fields(CheckedFields {
                    status: status.clone(),
                    limits: baseline_limits(),
                })
                .with_env_filter(filter().into_filter())
                .with_writer(move || output.clone())
                .finish();
            tracing::subscriber::with_default(checked, structured_events);
            assert!(!expected.0.lock().unwrap().is_empty());
            assert_eq!(*actual.0.lock().unwrap(), *expected.0.lock().unwrap());
            assert!(status.failure().is_none());
        }
    }
}

#[test]
fn upstream_output_guard_rejects_any_unexpected_tls_bytes() {
    let status = RecordStatus::default();
    let mut output = EmptyOutput(status.clone());
    output.write_all(b"").unwrap();
    output.flush().unwrap();
    assert!(status.failure().is_none());
    assert!(output.write_all(b"must not be discarded silently").is_err());
    assert!(matches!(
        status.failure().as_deref(),
        Some(RecordFailure::Formatting)
    ));
}

struct AllocationControl {
    attempts: usize,
    refuse: Option<usize>,
}

thread_local! {
    static PAYLOAD_ALLOCATIONS: RefCell<Option<AllocationControl>> = const { RefCell::new(None) };
}

struct PayloadAllocations;

impl PayloadAllocations {
    fn new(refuse: Option<usize>) -> Self {
        PAYLOAD_ALLOCATIONS.with(|control| {
            assert!(control.borrow().is_none());
            *control.borrow_mut() = Some(AllocationControl {
                attempts: 0,
                refuse,
            });
        });
        Self
    }

    fn attempts(&self) -> usize {
        PAYLOAD_ALLOCATIONS.with(|control| control.borrow().as_ref().unwrap().attempts)
    }
}

impl Drop for PayloadAllocations {
    fn drop(&mut self) {
        PAYLOAD_ALLOCATIONS.with(|control| *control.borrow_mut() = None);
    }
}

pub(super) fn before_payload_allocation() -> Result<(), std::collections::TryReserveError> {
    let refused = PAYLOAD_ALLOCATIONS.with(|control| {
        let mut control = control.borrow_mut();
        let Some(control) = control.as_mut() else {
            return false;
        };
        control.attempts += 1;
        control.refuse == Some(control.attempts)
    });
    if refused {
        Vec::<u8>::new().try_reserve_exact(usize::MAX)
    } else {
        Ok(())
    }
}

fn remove_span_cache<Fields: 'static>(span: &tracing::Span) {
    span.with_subscriber(|(identity, dispatch)| {
        let registry = dispatch
            .downcast_ref::<tracing_subscriber::Registry>()
            .unwrap();
        let span = registry.span(identity).unwrap();
        assert!(
            span.extensions_mut()
                .remove::<FormattedFields<Fields>>()
                .is_some()
        );
    })
    .unwrap();
}

fn allocation_failure(status: &RecordStatus) -> Arc<RecordFailure> {
    let failure = status.failure().unwrap();
    assert!(matches!(&*failure, RecordFailure::Allocation));
    failure
}

#[test]
fn event_scratch_allocation_refusal_is_sticky_and_never_publishes() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    let allocations = PayloadAllocations::new(Some(1));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "host_record", "refused before formatting");
        let first = allocation_failure(&status);
        tracing::info!(target: "host_record", "subsequent refusal");
        assert!(Arc::ptr_eq(&first, &allocation_failure(&status)));
    });
    assert_eq!(allocations.attempts(), 1);
    allocation_failure(&status);
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn initial_cache_backing_allocation_refusal_keeps_empty_fields_and_status() {
    struct Counted<'count>(&'count AtomicUsize);
    impl fmt::Debug for Counted<'_> {
        fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            writer.write_str("value")
        }
    }
    let calls = AtomicUsize::new(0);
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    let allocations = PayloadAllocations::new(Some(1));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", value = ?Counted(&calls));
        assert_eq!(cached_span(&span), (String::new(), 0));
        let first = allocation_failure(&status);
        span.record("value", tracing::field::debug(Counted(&calls)));
        assert_eq!(cached_span(&span), (String::new(), 0));
        let later = tracing::info_span!(target: "host_record", "later", value = ?Counted(&calls));
        assert_eq!(cached_span(&later), (String::new(), 0));
        tracing::info!(target: "host_record", "cannot publish");
        assert!(Arc::ptr_eq(&first, &allocation_failure(&status)));
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(allocations.attempts(), 1);
    allocation_failure(&status);
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn update_scratch_allocation_refusal_preserves_complete_previous_cache() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    let allocations = PayloadAllocations::new(Some(2));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", value = 1);
        let before = cached_span(&span);
        assert_eq!(before, ("value=1".to_owned(), 32));
        assert!(status.failure().is_none());
        span.record("value", 2);
        assert_eq!(cached_span(&span), before);
        let first = allocation_failure(&status);
        span.record("value", 3);
        assert_eq!(cached_span(&span), before);
        tracing::info!(target: "host_record", "cannot publish");
        assert!(Arc::ptr_eq(&first, &allocation_failure(&status)));
    });
    assert_eq!(allocations.attempts(), 2);
    allocation_failure(&status);
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn missing_cache_on_record_uses_the_same_fallible_initial_hook() {
    let records = Records::default();
    let (subscriber, status) = bounded_subscriber(&records, 128, 32);
    let allocations = PayloadAllocations::new(Some(2));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "host_record", "small", value = 1);
        remove_span_cache::<CheckedFields>(&span);
        span.record("value", 2);
        assert_eq!(cached_span(&span), (String::new(), 0));
        let first = allocation_failure(&status);
        span.record("value", 3);
        assert_eq!(cached_span(&span), (String::new(), 0));
        tracing::info!(target: "host_record", "cannot publish");
        assert!(Arc::ptr_eq(&first, &allocation_failure(&status)));
    });
    assert_eq!(allocations.attempts(), 2);
    allocation_failure(&status);
    assert!(records.lock().unwrap().is_empty());
}

#[test]
fn initial_cache_has_exact_capacity_and_no_second_payload_allocation() {
    for limit in [3, 7, 8, 31, 1024] {
        let records = Records::default();
        let (subscriber, status) = bounded_subscriber(&records, 128, limit);
        let allocations = PayloadAllocations::new(Some(2));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(target: "host_record", "small", v = 1);
            assert_eq!(cached_span(&span), ("v=1".to_owned(), limit));
        });
        assert_eq!(allocations.attempts(), 1);
        assert!(status.failure().is_none());
        assert!(records.lock().unwrap().is_empty());
    }
}

#[test]
fn owned_scratch_transfer_preserves_allocation_identity_and_utf8() {
    use std::fmt::Write as _;

    let status = RecordStatus::default();
    let allocations = PayloadAllocations::new(Some(2));
    let mut buffer = BoundedBuffer::new(7, BufferKind::Span, &status).unwrap();
    buffer.write_str("é").unwrap();
    let pointer = buffer.bytes.as_ptr();
    let text = buffer.into_string();
    assert_eq!(text.as_ptr(), pointer);
    assert_eq!(text, "é");
    assert_eq!(text.capacity(), 7);
    assert_eq!(allocations.attempts(), 1);
    assert!(status.failure().is_none());
}

#[test]
fn default_initial_hook_preserves_legacy_formatter_and_both_metadata_bits() {
    struct LegacyFields(Arc<Mutex<Vec<(bool, bool)>>>);
    impl<'writer> FormatFields<'writer> for LegacyFields {
        fn format_fields<Fields: RecordFields>(
            &self,
            writer: Writer<'writer>,
            fields: Fields,
        ) -> fmt::Result {
            self.0
                .lock()
                .unwrap()
                .push((writer.has_ansi_escapes(), writer.sanitizes_ansi_escapes()));
            DefaultFields::new().format_fields(writer, fields)
        }
    }
    for ansi in [false, true] {
        for sanitize in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(ansi)
                .with_ansi_sanitization(sanitize)
                .with_env_filter(filter().into_filter())
                .fmt_fields(LegacyFields(calls.clone()))
                .with_writer(io::sink)
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                let span = tracing::info_span!(target: "host_record", "small", v = 1);
                span.record("v", 2);
                remove_span_cache::<LegacyFields>(&span);
                span.record("v", 3);
            });
            assert_eq!(*calls.lock().unwrap(), [(ansi, sanitize); 3]);
        }
    }
}
