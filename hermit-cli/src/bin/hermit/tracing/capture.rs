use std::io;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Error;
use hermit::liteinst::CaptureDestination;
use hermit::liteinst::CaptureLimits;
use hermit::liteinst::CaptureOptions;
use hermit::liteinst::CaptureTimeouts;
use hermit::liteinst::DestinationProgress;
use hermit::liteinst::Evidence;
use hermit::liteinst::LogInput;
use hermit::liteinst::PendingRun;
use hermit::liteinst::Session;
use hermit::liteinst_bootstrap::EffectiveFilter;
use tracing_subscriber::util::SubscriberInitExt;

use super::BoundedWriter;
use super::TRUNCATION_MARKER;
use super::liteinst::EVENT_BYTES;
use super::liteinst::FormatterLimits;
use super::liteinst::SPAN_BYTES;
use super::liteinst::record_subscriber_with_failure;

const FINAL_DRAIN: Duration = Duration::from_secs(2);

struct Counted<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct Destination<W: Write> {
    writer: BoundedWriter<Counted<W>>,
    limit: u64,
    discarded: u64,
    failed: bool,
}

impl<W: Write> Destination<W> {
    pub fn new(writer: W, limit: u64) -> Self {
        Self {
            writer: BoundedWriter::new(
                Counted {
                    inner: writer,
                    bytes: 0,
                },
                limit,
            ),
            limit,
            discarded: 0,
            failed: false,
        }
    }
}

impl<W: Write> Write for Destination<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let before = self.writer.remaining;
        match self.writer.write(bytes) {
            Ok(consumed) => {
                if self.limit != 0 {
                    self.discarded += consumed as u64 - (before - self.writer.remaining);
                }
                Ok(consumed)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.writer.flush();
        self.failed |= result.is_err();
        result
    }
}

impl<W: Write + Send + 'static> CaptureDestination for Destination<W> {
    fn progress(&self) -> DestinationProgress {
        let total = self.writer.inner.bytes;
        let data = if self.limit == 0 {
            total
        } else {
            total.min(self.limit)
        };
        let marker = total - data;
        DestinationProgress {
            acknowledged_data_bytes: data,
            discarded_bytes: self.discarded,
            marker_bytes: marker,
            marker_complete: self.writer.announced
                && marker == TRUNCATION_MARKER.len() as u64
                && !self.failed,
            marker_failed: self.writer.announced && self.failed,
            output_ceiling: self.writer.announced,
        }
    }
}

#[derive(Debug)]
pub struct CaptureError {
    pub primary: Arc<Error>,
    pub evidence: Box<Evidence>,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}; LiteInst retained evidence: {:?}",
            self.primary, self.evidence
        )
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.primary.as_ref().as_ref())
    }
}

pub struct Capture {
    session: Session,
    input: Option<LogInput>,
}

impl Capture {
    pub fn install<W: Write + Send + 'static>(
        writer: W,
        limit: u64,
        filter: EffectiveFilter,
    ) -> Result<Self, Error> {
        let options = CaptureOptions {
            limits: CaptureLimits {
                producers: 64,
                slots_per_producer: 16,
                max_record_bytes: EVENT_BYTES,
                host_pending_bytes: 8 * 1024 * 1024,
                guest_pending_bytes: 8 * 1024 * 1024,
                pending_records: 1024,
                diagnostic_bytes: 64 * 1024,
            },
            timeouts: CaptureTimeouts {
                startup: Duration::from_secs(10),
                blocked_publication: Duration::from_secs(10),
                final_drain: FINAL_DRAIN,
            },
        };
        let (mut session, input, host) =
            hermit::liteinst::prepare(options, Destination::new(writer, limit), &filter)?;
        let failed_host = host.clone();
        let (subscriber, status) = record_subscriber_with_failure(
            filter,
            false,
            FormatterLimits::new(options.limits.max_record_bytes, SPAN_BYTES)?,
            move |record| {
                host.write_record(record).map(|_| ()).map_err(|error| {
                    io::Error::other(format!("host complete-record publication: {error:?}"))
                })
            },
            move || failed_host.record_failed(),
        );
        session.retain_record_status(move || status.failure().map(|error| error as Arc<_>))?;
        if let Err(error) = subscriber.try_init() {
            drop(input);
            return Err(CaptureError {
                primary: Arc::new(error.into()),
                evidence: Box::new(session.finish_evidence(Instant::now() + FINAL_DRAIN)),
            }
            .into());
        }
        Ok(Self {
            session,
            input: Some(input),
        })
    }

    pub fn run<T>(
        mut self,
        run: impl FnOnce(LogInput) -> Result<PendingRun<T>, Error>,
    ) -> Result<T, Error> {
        let input = self.input.take().expect("single-use CLI log input");
        let (result, evidence) = match run(input) {
            Ok(pending) => {
                let finalized = self.session.finish(pending, Instant::now() + FINAL_DRAIN);
                (finalized.result, finalized.evidence)
            }
            Err(error) => (
                Err(Arc::new(error)),
                self.session.finish_evidence(Instant::now() + FINAL_DRAIN),
            ),
        };
        result.map_err(|primary| {
            let timeout = primary
                .downcast_ref::<hermit::GuestTimedOut>()
                .map(|error| error.limit);
            let error = Error::new(CaptureError {
                primary,
                evidence: Box::new(evidence),
            });
            match timeout {
                Some(limit) => error.context(hermit::GuestTimedOut { limit }),
                None => error,
            }
        })
    }
}

#[cfg(test)]
#[path = "capture/tests.rs"]
mod tests;
