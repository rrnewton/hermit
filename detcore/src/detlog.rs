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

/// A process-local sink for deterministic INFO records.
pub type DetlogForwarder = for<'a> fn(fmt::Arguments<'a>);

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
    ($($arg:tt)+) => {{
        tracing::debug!("DETLOG {}", format!($($arg)+));
    }};
}

#[cfg(test)]
mod tests {
    use tracing::Metadata;
    use tracing::span;
    use tracing::subscriber::Interest;

    #[test]
    fn test_detlog() {
        detlog!("Hello : {}. From {:?}", "World", 31337);
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
