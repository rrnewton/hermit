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
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// A process-local sink for deterministic INFO records.
///
/// NOTE the shape: a sink receives ONLY the message arguments — no level, no
/// target, no timestamp. That is why every implementer has had to re-create the
/// record framing by hand, and why two of them independently got it wrong. Use
/// [`canonical_record`] rather than formatting a line locally; see its docs for
/// the contract being satisfied.
pub type DetlogForwarder = for<'a> fn(fmt::Arguments<'a>);

/// Monotonic source for the record-separator stamp. See [`next_record_stamp`].
static NEXT_RECORD_STAMP: AtomicU64 = AtomicU64::new(0);

/// The canonical target every DETLOG record is attributed to when a backend
/// emits outside the supervisor's tracing subscriber.
pub const DETLOG_TARGET: &str = "detcore";

/// Render one record in the CANONICAL framing that `logdiff` requires.
///
/// THE CONTRACT. [`crate::logdiff`]'s `extract_log_messages` is the only thing
/// that turns a log stream into comparable records, and it splits on a leading
/// RFC3339 stamp, then requires a level tag. So the stamp is a load-bearing
/// SEPARATOR, not decoration: a backend that omits it does not error, it
/// silently collapses its ENTIRE run into one record, and any cross-backend diff
/// then compares one message against thousands.
///
/// WHY THIS LIVES HERE rather than in each backend. Backends whose Detcore tool
/// runs in the supervisor (ptrace, KVM, and the ptrace-hosted e9patch and
/// LiteInst paths) get the framing free from `tracing_subscriber::fmt()`. The
/// ones whose tool runs elsewhere — DBI as a DynamoRIO client, SaBRe as an
/// in-guest plugin — cannot, so they formatted lines by hand and each produced a
/// different, non-conforming shape. One shared renderer is the fix; a per-backend
/// one is the bug.
///
/// The layout matches `tracing_subscriber::fmt()`'s default exactly: stamp,
/// space, level right-aligned in five columns, space, target, `": "`, fields,
/// newline. The five-column alignment is part of the contract — it is why
/// supervisor-emitted lines read `…Z  INFO detcore:` with two spaces.
pub fn canonical_record(
    stamp: u64,
    level: &str,
    target: &str,
    fields: fmt::Arguments<'_>,
) -> String {
    // The stamp is a RECORD SEPARATOR, not a clock reading — see
    // `next_record_stamp` for why it must not be a real wall-clock time.
    let micros = stamp % 1_000_000;
    let secs = (stamp / 1_000_000) % 60;
    let mins = (stamp / 60_000_000) % 60;
    let hours = (stamp / 3_600_000_000) % 24;
    format!(
        "1970-01-01T{hours:02}:{mins:02}:{secs:02}.{micros:06}Z {level:>5} {target}: {fields}\n"
    )
}

/// A synthetic, strictly increasing stamp used ONLY to delimit records.
///
/// Deliberately NOT a wall-clock reading, for two reasons:
///
/// 1. **It must not perturb the guest.** These sinks run inside the traced
///    process. Calling the clock would inject a `clock_gettime` that Detcore
///    intercepts and that advances virtualized time, so merely enabling logging
///    would change the schedule it is supposed to observe.
/// 2. **The value is discarded anyway.** `extract_log_messages` splits *on* the
///    stamp and keeps only what follows, so nothing downstream reads it. A
///    counter satisfies the contract as well as a clock and, unlike a clock, is
///    deterministic.
///
/// Microsecond-shaped purely so the rendered text is a well-formed RFC3339
/// instant; it is not a duration and must not be read as elapsed time.
pub fn next_record_stamp() -> u64 {
    NEXT_RECORD_STAMP.fetch_add(1, Ordering::Relaxed)
}

/// Render a DETLOG record for an out-of-supervisor sink, at INFO with the
/// canonical target. This is what a [`DetlogForwarder`] should write.
pub fn canonical_detlog_line(message: fmt::Arguments<'_>) -> String {
    canonical_record(
        next_record_stamp(),
        "INFO",
        DETLOG_TARGET,
        format_args!("DETLOG {message}"),
    )
}

static FORWARDER: OnceLock<DetlogForwarder> = OnceLock::new();

/// Installs a process-local sink for deterministic INFO records.
///
/// Backends whose tool runs in another process can use this to transport the
/// same records that are normally observed through the coordinator's tracing
/// subscriber. Only the first sink installed in a process is retained.
pub fn set_forwarder(forwarder: DetlogForwarder) -> Result<(), DetlogForwarder> {
    FORWARDER.set(forwarder)
}

/// Returns whether a process-local deterministic-record sink is installed.
#[doc(hidden)]
pub fn forwarding_enabled() -> bool {
    FORWARDER.get().is_some()
}

/// Returns whether deterministic INFO records have an observable sink.
///
/// Out-of-process tools such as SaBRe install a forwarder without a tracing
/// subscriber, so tracing's INFO gate alone is not the DETLOG condition.
#[doc(hidden)]
pub fn info_observable() -> bool {
    info_observable_from(
        tracing::enabled!(tracing::Level::INFO),
        forwarding_enabled(),
    )
}

fn info_observable_from(tracing_info_enabled: bool, forwarder_installed: bool) -> bool {
    tracing_info_enabled || forwarder_installed
}

/// Emits one deterministic record through tracing and the process-local sink.
#[doc(hidden)]
pub fn emit_forwarded(message: fmt::Arguments<'_>) {
    tracing::info!("DETLOG {}", message);
    FORWARDER.get().expect("forwarder disappeared")(message);
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This is currently at the INFO log level.
#[macro_export]
macro_rules! detlog {
    ($($arg:tt)+) => {{
        if $crate::detlog::forwarding_enabled() {
            $crate::detlog::emit_forwarded(format_args!($($arg)+));
        } else {
            tracing::info!("DETLOG {}", format_args!($($arg)+));
        }
    }};
}

/// Macro used to encapsulate tracing should-be-deterministic information.
/// This variant is at a higher log level and requires that logging verbosity is
/// set to DEBUG.
#[macro_export]
macro_rules! detlog_debug {
    ($($arg:tt)+) => {{
        tracing::debug!("DETLOG {}", format!($($arg)+));
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_observability_brackets_both_detlog_sinks() {
        assert!(!info_observable_from(false, false));
        assert!(info_observable_from(true, false));
        assert!(info_observable_from(false, true));
        assert!(info_observable_from(true, true));
    }

    #[test]
    fn test_detlog() {
        detlog!("Hello : {}. From {:?}", "World", 31337);
    }

    #[test]
    fn canonical_line_matches_the_supervisor_framing() {
        // Byte-for-byte the shape ptrace emits through tracing_subscriber::fmt():
        //   2026-08-06T12:04:57.836579Z  INFO detcore: DETLOG ...
        // Note the TWO spaces before INFO — the level is right-aligned in five
        // columns. A sink that emitted one space would shift every record.
        let line = canonical_record(0, "INFO", "detcore", format_args!("DETLOG x"));
        assert_eq!(
            line,
            "1970-01-01T00:00:00.000000Z  INFO detcore: DETLOG x\n"
        );
    }

    #[test]
    fn five_column_alignment_holds_for_every_level() {
        for (level, expected) in [
            ("ERROR", "Z ERROR t: m\n"),
            ("WARN", "Z  WARN t: m\n"),
            ("INFO", "Z  INFO t: m\n"),
            ("DEBUG", "Z DEBUG t: m\n"),
            ("TRACE", "Z TRACE t: m\n"),
        ] {
            let line = canonical_record(0, level, "t", format_args!("m"));
            assert!(line.ends_with(expected), "{level}: {line:?}");
        }
    }

    #[test]
    fn stamp_stays_a_well_formed_rfc3339_instant_across_rollovers() {
        for stamp in [0, 999_999, 1_000_000, 59_999_999, 60_000_000, 3_599_999_999] {
            let line = canonical_record(stamp, "INFO", "t", format_args!("m"));
            let text = line.split(' ').next().unwrap();
            assert_eq!(text.len(), "1970-01-01T00:00:00.000000Z".len(), "{line:?}");
            assert!(text.ends_with('Z'), "{line:?}");
            assert!(text[11..13].parse::<u32>().unwrap() < 24, "{line:?}");
            assert!(text[14..16].parse::<u32>().unwrap() < 60, "{line:?}");
            assert!(text[17..19].parse::<u32>().unwrap() < 60, "{line:?}");
        }
    }

    #[test]
    fn stamps_strictly_increase_and_rendering_is_pure() {
        let a = next_record_stamp();
        let b = next_record_stamp();
        assert!(a < b, "{a} {b}");
        // Pure in the stamp: this is what makes the sink deterministic, and it is
        // the property a SystemTime::now() implementation would destroy.
        assert_eq!(
            canonical_record(7, "INFO", "t", format_args!("m")),
            canonical_record(7, "INFO", "t", format_args!("m"))
        );
    }

    #[test]
    fn forwarded_detlog_line_is_split_by_the_real_consumer() {
        // The decisive check: feed three sink-rendered lines to the SAME splitter
        // the comparator uses, and require three records. Before the shared
        // renderer, hand-rolled sinks emitted no stamp and this collapsed to one.
        let stream: String = (0..3)
            .map(|_| canonical_detlog_line(format_args!("event")))
            .collect();
        assert_eq!(
            crate::logdiff::record_count_for_test(&stream),
            3,
            "{stream:?}"
        );
    }
}
