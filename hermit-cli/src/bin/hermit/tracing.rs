/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io;
use std::io::IsTerminal;
use std::io::Write;
use std::io::stderr;

use tracing::Subscriber;
use tracing::metadata::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;

const DEFAULT_TRACE_LEVEL: LevelFilter = LevelFilter::WARN;

/// Environment override for [`DEFAULT_LOG_MAX_BYTES`]; `0` disables the bound.
pub const LOG_MAX_BYTES_ENV: &str = "HERMIT_LOG_MAX_BYTES";

/// Ceiling on one run's log file.
///
/// A run that makes no progress still logs. Measured 2026-08-11 over the 4111
/// retained `--verify` comparison logs on one host: p50 0.4 MiB, p90 43.5 MiB,
/// p99 145.5 MiB -- and then a tail to 928.8 GiB, 4357.4 GiB in total. The
/// largest single file was a KVM guest livelocked on `sched_yield`, logging the
/// same fifteen line shapes for 11.7 hours; its registers at the 50 GiB and
/// 900 GiB offsets were identical.
///
/// 1 GiB is about 7x p99, so it never truncates a log anyone reads, and it
/// removes 98.6% of those bytes while touching 0.56% of the files. The
/// distribution is bimodal enough that any bound from 0.25 to 16 GiB reclaims
/// over 94%, so this is a safety limit rather than a tuning parameter.
///
/// Compression is NOT the control. `/tmp` happens to be zstd-compressed here,
/// which is why a terabyte of repetitive DETLOG has been survivable, but a
/// compression ratio scales with how repetitive the runaway happens to be and
/// silently converts a hard failure into an invisible one.
pub const DEFAULT_LOG_MAX_BYTES: u64 = 1 << 30;

/// Written once, in-band, when the bound is reached.
///
/// A truncated log MUST say so. Output that simply stops reads as a run that
/// ENDED, which would be a worse evidence defect than the disk hazard this
/// bound removes: a reader would draw conclusions from an absence we created.
const TRUNCATION_MARKER: &[u8] =
    b"\n=== HERMIT LOG TRUNCATED: reached the configured size bound (HERMIT_LOG_MAX_BYTES). \
Output beyond this point was DISCARDED. The run itself continued and was NOT affected. ===\n";

/// Resolve the configured ceiling; `0` (or an unparsable value) means unlimited.
pub fn log_max_bytes() -> u64 {
    match std::env::var(LOG_MAX_BYTES_ENV) {
        Ok(raw) => raw.trim().parse().unwrap_or(DEFAULT_LOG_MAX_BYTES),
        Err(_) => DEFAULT_LOG_MAX_BYTES,
    }
}

/// A writer that stops after `limit` bytes and says so in-band.
///
/// Beyond the bound this reports the bytes as consumed rather than returning a
/// short count: `tracing_appender` treats a short write as an I/O error, so a
/// truthful-but-short return would turn a size limit into a logging failure.
pub struct BoundedWriter<W: Write> {
    inner: W,
    remaining: u64,
    bounded: bool,
    announced: bool,
}

impl<W: Write> BoundedWriter<W> {
    /// `limit == 0` disables the bound entirely.
    pub fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            bounded: limit != 0,
            announced: false,
        }
    }
}

impl<W: Write> BoundedWriter<W> {
    /// Announce truncation the FIRST time bytes are actually discarded.
    ///
    /// Deferring this to the next `write` leaves a log that was truncated on
    /// its final write silent, which is the exact failure the marker exists to
    /// prevent -- caught end-to-end: a 100-byte bound produced exactly 100
    /// bytes and no marker, because no further write ever arrived.
    fn announce_truncation(&mut self) -> io::Result<()> {
        if !self.announced {
            self.announced = true;
            self.inner.write_all(TRUNCATION_MARKER)?;
            self.inner.flush()?;
        }
        Ok(())
    }
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.bounded {
            return self.inner.write(buf);
        }
        if self.remaining == 0 {
            self.announce_truncation()?;
            return Ok(buf.len());
        }
        let take = buf.len().min(self.remaining as usize);
        let written = self.inner.write(&buf[..take])?;
        self.remaining -= written as u64;
        if written < take {
            // A genuine short write by the inner writer: report it truthfully
            // and let the caller retry the remainder.
            return Ok(written);
        }
        if take < buf.len() {
            // This write is the one that crossed the bound; its tail is being
            // dropped, so say so now rather than hoping for another write.
            self.announce_truncation()?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Returns a non-blocking subscriber for logging to a file.
///
/// NOTE: Writes to `f` are unbuffered, so this may be slow.
fn file_subscriber<W: Write + Send + 'static>(
    level: LevelFilter,
    f: W,
) -> (impl Subscriber, impl Drop) {
    let filter = EnvFilter::from_default_env()
        .add_directive("tokio=debug".parse().expect("correct directive"))
        .add_directive(level.into());

    let (writer, guard) = tracing_appender::non_blocking(f);

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .finish();

    (subscriber, guard)
}

/// Initializes tracing to the given file `f`.
///
/// NOTE: Writes to `f` are unbuffered, so this may be slow.
#[must_use = "This function returns a guard that should not be immediately dropped"]
pub fn init_file_tracing<W: Write + Send + 'static>(level: Option<LevelFilter>, f: W) -> impl Drop {
    let level = level.unwrap_or(DEFAULT_TRACE_LEVEL);

    let (subscriber, guard) = file_subscriber(level, f);

    subscriber
        .try_init()
        .expect("global tracing subscriber to install");

    guard
}

/// Returns a tracing subscriber that logs to `stderr`.
///
/// NOTE: Writes to stderr are unbuffered, so this may be slow.
pub fn stderr_subscriber(level: Option<LevelFilter>) -> impl Subscriber {
    let level = level.unwrap_or(DEFAULT_TRACE_LEVEL);

    let filter = EnvFilter::from_default_env()
        .add_directive("tokio=debug".parse().expect("correct directive"))
        .add_directive(level.into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(stderr().is_terminal())
        .finish()
}

/// Initializes tracing to `stderr`.
///
/// NOTE: Writes to stderr are unbuffered, so this may be slow.
pub fn init_stderr_tracing(level: Option<LevelFilter>) {
    // Create an extra, pointless thread just so that our thread number starts at the same DetTid
    // "3" that the `init_file_tracing` option does.
    std::thread::spawn(|| {}).join().unwrap();

    stderr_subscriber(level)
        .try_init()
        .expect("global tracing subscriber to install")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bracketed both ways on purpose: a bound that always fires would silently
    /// truncate ordinary diagnostic logs, which is the failure mode opposite to
    /// the one being fixed and just as damaging to an investigation.
    #[test]
    fn bound_truncates_and_announces_exactly_once() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 100);
            for _ in 0..50 {
                writer.write_all(&[b'x'; 10]).unwrap();
            }
            writer.flush().unwrap();
        }
        let body = sink.iter().filter(|byte| **byte == b'x').count();
        let text = String::from_utf8_lossy(&sink);
        assert_eq!(body, 100, "wrote past the bound");
        assert_eq!(
            text.matches("HERMIT LOG TRUNCATED").count(),
            1,
            "truncation must be announced exactly once, not per write"
        );
    }

    #[test]
    fn under_the_bound_nothing_is_truncated_or_announced() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 1000);
            for _ in 0..50 {
                writer.write_all(&[b'x'; 10]).unwrap();
            }
        }
        assert_eq!(sink.len(), 500);
        assert!(!String::from_utf8_lossy(&sink).contains("HERMIT LOG TRUNCATED"));
    }

    #[test]
    fn a_zero_limit_disables_the_bound() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 0);
            writer.write_all(&[b'z'; 4096]).unwrap();
        }
        assert_eq!(sink.len(), 4096);
    }

    /// The regression the unit tests originally missed and an end-to-end run
    /// caught: a log whose FINAL write crosses the bound must still announce
    /// itself. Deferring the marker to the next write left a 100-byte-bounded
    /// log at exactly 100 bytes with no marker at all.
    #[test]
    fn a_single_straddling_write_announces_without_a_following_write() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = BoundedWriter::new(&mut sink, 10);
            writer.write_all(&[b'q'; 40]).unwrap();
            // deliberately no further write
        }
        let text = String::from_utf8_lossy(&sink);
        assert_eq!(
            text.matches("HERMIT LOG TRUNCATED").count(),
            1,
            "a truncated final write must announce itself: {text}"
        );
        assert_eq!(sink.iter().filter(|b| **b == b'q').count(), 10);
    }

    /// The writer's marker and the comparator's marker must be the same text.
    ///
    /// `detcore::logdiff` refuses to return a comparison verdict for a log
    /// containing [`detcore::logdiff::TRUNCATION_MARKER`]. If the text written
    /// here ever drifts from the text matched there, that refusal stops firing
    /// and a truncated pair silently returns to comparing only its prefix --
    /// with nothing failing to say so. This test is the binding between them.
    #[test]
    fn the_written_marker_is_the_text_the_comparator_matches() {
        let written = String::from_utf8(TRUNCATION_MARKER.to_vec()).unwrap();
        assert!(
            written.contains(detcore::logdiff::TRUNCATION_MARKER),
            "the bounded writer emits {written:?}, which does not contain the marker the \
             comparator matches ({:?}); detcore::logdiff would no longer detect truncation",
            detcore::logdiff::TRUNCATION_MARKER
        );
    }

    /// The non-obvious correctness point: `tracing_appender` treats a short
    /// write as an I/O error, so a discarding writer must still report the
    /// caller's whole buffer as consumed.
    #[test]
    fn a_write_straddling_the_bound_reports_full_consumption() {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = BoundedWriter::new(&mut sink, 5);
        assert_eq!(writer.write(&[b'y'; 40]).unwrap(), 40);
    }
}
