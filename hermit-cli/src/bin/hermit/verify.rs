/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use colored::Colorize;
use detcore::logdiff;
use detcore::logdiff::ComparisonSideLabels;
use detcore::logdiff::LogComparisonMode;
use hermit::Context;
use hermit::Error;
use pretty_assertions::Comparison;
use reverie::process::ExitStatus;
use reverie::process::Output;
use serde::Serialize;
use tempfile::NamedTempFile;
use tempfile::TempPath;
use tracing::metadata::LevelFilter;

use super::global_opts::GlobalOpts;
use super::record_envelope::RecordEnvelope;
use super::record_envelope::RecordEnvelopePolicy;

pub(crate) struct ComparedRun<'a> {
    pub output: &'a Output,
    pub log: TempPath,
    /// Reader-facing name for this side of the comparison. Run verification
    /// compares two fresh executions; record verification compares one
    /// recording with its replay.
    pub label: &'a str,
}

pub(crate) struct ComparisonOptions {
    /// Controls only how much diff *output* is printed (a larger syscall-history
    /// window), NOT the comparison semantics. Comparison strictness is carried
    /// separately in [`Self::strictness`] so a quiet run can still be
    /// bitwise-strict — the two knobs were historically conflated behind a single
    /// `verbose` flag, which made the only bitwise comparison also the loudest.
    pub verbose: bool,
    /// How strictly the internal event stream is compared. This is the
    /// condition the verdict rests on, and is recorded verbatim in the resulting
    /// [`VerificationOutcome`] so a consumer can tell a stripped match from a
    /// bitwise one.
    pub strictness: LogCompareStrictness,
    pub compare_logs: bool,
    /// Compare DEBUG/TRACE diagnostics in addition to the canonical INFO
    /// envelope. This is reserved for the explicit `--verify-verbose` diagnostic
    /// mode; an ordinary `--verify-strict` verdict must not depend on diagnostic
    /// events merely because the caller requested that they be captured.
    pub diagnostic_full_trace: bool,
    /// Whether the compared records contain the CONTENT of syscall output
    /// buffers, i.e. whether the run hashed them. That is ON BY DEFAULT; it is
    /// false only when the caller passed `--no-detlog-io-buffers`.
    ///
    /// This is not a comparator setting; it describes what the records being
    /// compared can possibly show. See [`ComparisonSpec::compare_io_buffers`].
    pub compare_io_buffers: bool,
    /// Keep both captured logs at their selected paths after comparison,
    /// whether the runs match or diverge.
    pub keep_logs: bool,
    /// Typed, versioned record envelope applied before selecting messages.
    /// Its policy identity is serialized beside the verdict.
    pub record_envelope: RecordEnvelope,
}

/// How strictly two runs' internal logs are compared — the condition a
/// [`Verdict`] rests on.
///
/// A bare "matched" verdict is meaningless without this. The two modes sit at
/// opposite ends of the available log comparisons:
///
/// - [`Self::Stripped`] normalizes away numeric values, addresses, tmp paths,
///   and — most importantly — the virtual-time timestamps and syscall
///   argument/result values that parity exists to check, so a `Matched` verdict
///   under `Stripped` asserts only "matched after normalizing known-
///   nondeterministic data", NOT parity. STRIPPING DESTROYS THE ABILITY TO
///   DETECT A DIFFERENCE.
/// - [`Self::Canonical`] is the parity mode. It strips the real wall-clock
///   timestamp prefix only (genuinely irreproducible; done by
///   `extract_log_messages`), canonicalizes host
///   memory addresses to an ordinal by first appearance (1, 2, 3…), preserving
///   identity, ordering, and aliasing while discarding only the host-specific raw
///   pointer, and compares exactly everything else — virtual-time timestamps,
///   syscall inputs/results, counts, sizes, flags. CANONICALIZING PRESERVES the
///   ability to detect a difference (allocation-order and aliasing changes still
///   diverge), which is the whole point.
///
/// Carrying the strictness beside the verdict is the same discipline as
/// recording the `-j` a byte count was measured at: the value is uninterpretable
/// without the condition that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogCompareStrictness {
    /// `strip_lines = true`, comparing the deterministic Detcore/scheduler
    /// message subset. Tolerant of limited nondeterminism (numbers, addresses,
    /// tmp paths, and timestamps are normalized before diffing). NOT a parity
    /// claim.
    Stripped,
    /// The parity mode (`BitwiseInfoV1`): `strip_lines = false` and
    /// `canonicalize_addresses = true`, comparing every INFO record admitted by
    /// the named record envelope.
    /// Only the real wall-clock timestamp prefix is stripped and host addresses
    /// are canonicalized to first-appearance ordinals; every other byte —
    /// virtual-time timestamps, raw syscall argument/result values, counts,
    /// sizes, flags — must match exactly.
    Canonical,
}

/// Which captured messages actually participated in the log comparison.
///
/// This travels in the typed report so an INFO-parity consumer never has to
/// infer the observation envelope from the requested logging verbosity. In
/// particular, explicitly capturing DEBUG does not silently promote those
/// diagnostics into the `BitwiseInfoV1` verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparedLogScope {
    /// The legacy selected DETLOG/scheduler subset used by stripped verification.
    Deterministic,
    /// Every INFO message, exactly; DEBUG/TRACE captures remain diagnostic.
    Info,
    /// Every captured message, selected only by explicit diagnostic verification.
    FullTrace,
}

/// Versioned policy token: the only strippable datum is the real wall-clock
/// timestamp PREFIX. Recorded in [`ComparisonSpec::stripped_prefixes`] so a
/// consumer sees exactly which prefixes were removed, not a bare boolean.
pub const STRIP_WALL_CLOCK_PREFIX_V1: &str = "real-wall-clock-prefix/v1";

/// Versioned policy token: host memory addresses are canonicalized to an ordinal
/// by first appearance (identity/order/aliasing preserved). Recorded in
/// [`ComparisonSpec::canonicalizations`].
pub const CANON_ADDRESS_ORDINAL_V1: &str = "host-address-to-first-appearance-ordinal/v1";

/// Versioned policy token marking the lossy wholesale normalization the
/// [`LogCompareStrictness::Stripped`] mode applies (numbers, addresses, tmp
/// paths, and timestamps erased). Its presence in a spec is disqualifying for
/// parity: it is recorded so a consumer can see WHY a stripped spec is not
/// parity rather than having to infer it.
pub const STRIP_UNSAFE_NORMALIZATION_V1: &str = "unsafe-numeric-address-and-path-normalization/v1";

/// The exact set of stripped-prefix tokens the parity ([`Canonical`]) policy
/// permits: the wall-clock prefix, and nothing else.
///
/// [`Canonical`]: LogCompareStrictness::Canonical
const PARITY_STRIPPED_PREFIXES: &[&str] = &[STRIP_WALL_CLOCK_PREFIX_V1];

/// The exact set of canonicalization tokens the parity ([`Canonical`]) policy
/// requires: address-to-ordinal, and nothing else.
///
/// [`Canonical`]: LogCompareStrictness::Canonical
const PARITY_CANONICALIZATIONS: &[&str] = &[CANON_ADDRESS_ORDINAL_V1];

/// The exact comparison that produced a [`Verdict`], carried beside it so a bare
/// "verified" can always say *which* comparison certified it.
///
/// The high-level [`Self::strictness`] and the concrete flags it expands to are
/// both recorded: a JSON consumer keying a bitwise-parity ratchet on the verdict
/// can require `strip_lines == false`, `full_trace == true`, and an INFO-or-
/// stronger [`Self::log_scope`] directly, rather than having to know how a
/// strictness label maps onto the diff engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ComparisonSpec {
    /// The strictness label the comparison ran under.
    pub strictness: LogCompareStrictness,
    /// Whether the internal event stream was compared at all. When
    /// `false` (e.g. KVM concurrent mode) only stdout/stderr/exit status were
    /// compared and the strictness fields describe a log comparison that did not
    /// run — a consumer must not read such a verdict as bitwise log parity.
    pub compare_logs: bool,
    /// Whether the compared records contain the CONTENT of syscall output
    /// buffers, not merely the syscalls' return values.
    ///
    /// ⚠️ WHEN THIS IS `false`, A MATCH IS NOT A CONTENT-PARITY RESULT, and a
    /// consumer must not read it as one. Reverie types many output buffers as
    /// bare pointers, so the record prints the ADDRESS and not the bytes --
    /// `reverie-syscalls/src/syscalls.rs` carries standing TODOs saying exactly
    /// that for `Read` and `Write`. Two runs whose buffers differ while their
    /// return values agree therefore produce character-identical records and
    /// compare equal.
    ///
    /// That is not hypothetical. A `recvmsg` of a netlink RTM_GETLINK dump
    /// returns a stable `Ok(1468)` on every run while four bytes of its payload
    /// vary (host kernel jiffies and a kernel-randomised timer). Measured: the
    /// same guest reports `verdict: matched, bitwise_parity: true` with this
    /// `false`, and `verdict: diverged` with it `true`. On one QEMU/Linux boot,
    /// 44.1% of syscalls (278,824 of 632,228) move bytes through a buffer whose
    /// content the record does not show.
    ///
    /// True by default, and set false only by `--no-detlog-io-buffers`. It is
    /// recorded here rather than inferred so a consumer can require it instead
    /// of assuming it.
    pub compare_io_buffers: bool,
    /// The message envelope selected from the captured log. The compared-message
    /// counts refer exactly to this scope.
    pub log_scope: ComparedLogScope,
    /// Versioned policy selecting which parsed records entered the message
    /// envelope. A caller-defined predicate is disclosed but cannot qualify for
    /// parity.
    pub record_envelope: RecordEnvelopePolicy,
    /// Concrete: were numeric values, addresses, tmp paths, and timestamps
    /// normalized away wholesale before diffing (the lossy [`Stripped`] path)?
    ///
    /// [`Stripped`]: LogCompareStrictness::Stripped
    pub strip_lines: bool,
    /// Concrete: were host memory addresses canonicalized to first-appearance
    /// ordinals before diffing? Unlike
    /// [`Self::strip_lines`] this is lossless for parity: it discards only the
    /// raw pointer value, keeping identity, order, and aliasing.
    pub canonicalize_addresses: bool,
    /// Concrete: was the complete parity observation envelope compared (vs. the
    /// legacy deterministic subset)? For `BitwiseInfoV1`, that complete envelope
    /// is every INFO record admitted by [`Self::record_envelope`], and
    /// [`Self::log_scope`] records whether the explicit diagnostic full-trace
    /// superset was requested.
    pub full_trace: bool,
    /// Concrete: was everything OTHER than the stripped prefix and the
    /// canonicalized addresses compared exactly (virtual-time timestamps,
    /// syscall inputs/results, counts, sizes, flags)? True for the parity policy;
    /// false whenever a lossy normalization (e.g. [`Self::strip_lines`]) ran.
    pub exact_remainder: bool,
    /// Versioned tokens for every prefix STRIPPED before comparison. The parity
    /// policy permits exactly `["real-wall-clock-prefix/v1"]`; a lossy stripped
    /// comparison additionally lists the wholesale-normalization token. Recorded
    /// (not inferred) so a consumer can see precisely what was discarded.
    pub stripped_prefixes: &'static [&'static str],
    /// Versioned tokens for every CANONICALIZATION applied before comparison. The
    /// parity policy requires exactly
    /// `["host-address-to-first-appearance-ordinal/v1"]`.
    pub canonicalizations: &'static [&'static str],
    /// Concrete: were any `--ignore-lines` substring filters applied, dropping
    /// matching log lines before the comparison? Bitwise parity requires none.
    pub ignore_lines: bool,
    /// Concrete: were `COMMIT` messages excluded from the comparison? Bitwise
    /// parity requires them included.
    pub skip_commit: bool,
    /// Concrete: were `DETLOG` messages (or any DETLOG class) excluded from the
    /// comparison? Bitwise parity requires the full event stream.
    pub skip_detlog: bool,
}

impl ComparisonSpec {
    /// Build the spec (and, implicitly, the concrete diff flags) from the
    /// requested strictness and whether logs are compared at all. This is the
    /// single place the strictness label maps onto `strip_lines`/`full_trace`,
    /// so the flags the diff engine sees and the flags the verdict reports can
    /// never drift apart.
    pub fn new(
        strictness: LogCompareStrictness,
        compare_logs: bool,
        diagnostic_full_trace: bool,
        compare_io_buffers: bool,
        record_envelope: RecordEnvelopePolicy,
    ) -> Self {
        // Map the strictness label onto the concrete diff flags AND the versioned
        // policy tokens in one place, so the flags the engine sees, the tokens
        // the verdict reports, and the strictness label can never drift apart.
        let (strip_lines, canonicalize_addresses, full_trace, exact_remainder, log_scope) =
            match strictness {
                // Lossy wholesale normalization: numbers/addresses/paths/timestamps
                // erased; the remainder is NOT compared exactly.
                LogCompareStrictness::Stripped => {
                    debug_assert!(!diagnostic_full_trace);
                    (true, false, false, false, ComparedLogScope::Deterministic)
                }
                // Parity (BitwiseInfoV1): strip only the wall-clock prefix,
                // canonicalize addresses, and compare every INFO message admitted
                // by the selected named record envelope exactly.
                // The explicit verbose diagnostic mode compares the all-level
                // superset without changing the canonicalization policy.
                LogCompareStrictness::Canonical => (
                    false,
                    true,
                    true,
                    true,
                    if diagnostic_full_trace {
                        ComparedLogScope::FullTrace
                    } else {
                        ComparedLogScope::Info
                    },
                ),
            };
        let (stripped_prefixes, canonicalizations): (&[&str], &[&str]) = match strictness {
            LogCompareStrictness::Stripped => (
                &[STRIP_WALL_CLOCK_PREFIX_V1, STRIP_UNSAFE_NORMALIZATION_V1],
                // Under stripping, addresses are ERASED (to a single `<ADDR>`
                // token), not canonicalized; there is no ordinal preserved.
                &[],
            ),
            LogCompareStrictness::Canonical => (PARITY_STRIPPED_PREFIXES, PARITY_CANONICALIZATIONS),
        };
        ComparisonSpec {
            strictness,
            compare_logs,
            compare_io_buffers,
            log_scope,
            record_envelope,
            strip_lines,
            canonicalize_addresses,
            full_trace,
            exact_remainder,
            stripped_prefixes,
            canonicalizations,
            // The `--verify` code paths never expose the diff engine's line
            // filters, so the comparison they produce applies none. These are
            // recorded (not merely assumed) so a parity consumer can *require*
            // their absence rather than trust that no CLI surface enables them;
            // `default_log_diff_opts_apply_no_line_filters` binds these values to
            // the actual `LogDiffOpts` the engine sees.
            ignore_lines: false,
            skip_commit: false,
            skip_detlog: false,
        }
    }

    /// The `LogComparisonMode` this spec selects for the diff engine.
    fn log_comparison_mode(&self) -> LogComparisonMode {
        match self.log_scope {
            ComparedLogScope::Deterministic => LogComparisonMode::Deterministic,
            ComparedLogScope::Info => LogComparisonMode::Info,
            ComparedLogScope::FullTrace => LogComparisonMode::FullTrace,
        }
    }

    /// Does this comparison satisfy the `BitwiseInfoV1` parity contract a
    /// determinism / record-replay ratchet must require before it may read a
    /// `Matched` verdict as *true parity*? A bare `verified` is not enough:
    /// `verified` can rest on a stripped compare, an opaque filtered subset, or
    /// an output-only fallback, all of which normalize or omit exactly the data
    /// (virtual-time timestamps, raw syscall argument/result values, whole event
    /// classes) that parity exists to check.
    ///
    /// This requires the EXACT `BitwiseInfoV1` policy shape, not merely
    /// "not stripped": a generic `strip_lines = false` is inadmissible on its
    /// own. All clauses must hold:
    /// - the full INFO event stream within the named record envelope (or the
    ///   explicit all-level diagnostic superset) was compared
    ///   ([`Self::full_trace`] and [`Self::log_scope`]), which carries exact
    ///   virtual timestamps and syscall argument/result values;
    /// - no lossy wholesale normalization ran (`!strip_lines`) and the remainder
    ///   was compared exactly ([`Self::exact_remainder`]);
    /// - addresses were CANONICALIZED, not erased ([`Self::canonicalize_addresses`]),
    ///   so allocation-order and aliasing differences are still detectable;
    /// - the versioned policy tokens are exactly the parity set — only the
    ///   wall-clock prefix stripped, only address-ordinal canonicalization
    ///   applied — so a future extra strip/canonicalization cannot silently pass;
    /// - the record predicate is one of the explicitly named, versioned
    ///   canonical envelopes ([`Self::record_envelope`]); an opaque
    ///   caller-defined predicate cannot qualify;
    /// - no ad hoc ignore/skip filter dropped any line or event class
    ///   (`!ignore_lines && !skip_commit && !skip_detlog`);
    /// - the internal log stream was actually compared, not skipped for an
    ///   output-only fallback ([`Self::compare_logs`]).
    ///
    /// A consumer asking for parity must reject `Matched` under every weaker
    /// comparison; this predicate is that single acceptance rule.
    /// ⚠️ EVERY CLAUSE BELOW IS ABOUT THE COMPARATOR, NOT ABOUT WHAT THE
    /// RECORDS CONTAIN. This answers "was the comparison maximally strict over
    /// the records it was given", which is not the same question as "are the
    /// two runs bitwise identical". A divergence in a syscall's output buffer is
    /// absent from the records entirely unless [`Self::compare_io_buffers`] is
    /// set, so this can return `true` for a pair of runs that differ in guest
    /// memory. Check `compare_io_buffers` alongside this when the distinction
    /// matters.
    pub fn is_bitwise_parity(&self) -> bool {
        self.compare_logs
            && self.full_trace
            && matches!(
                self.log_scope,
                ComparedLogScope::Info | ComparedLogScope::FullTrace
            )
            && !self.strip_lines
            && self.canonicalize_addresses
            && self.exact_remainder
            && self.stripped_prefixes == PARITY_STRIPPED_PREFIXES
            && self.canonicalizations == PARITY_CANONICALIZATIONS
            && self.record_envelope.is_canonical()
            && !self.ignore_lines
            && !self.skip_commit
            && !self.skip_detlog
    }
}

/// The verification verdict: did the two runs match?
///
/// This is deliberately distinct from the guest's exit status. The process exit
/// code of a `--verify` run historically encodes *the guest's* exit status (so
/// `record start --verify -- prog` behaves like `prog` for the common exit-0
/// case), which conflates two independent facts: "did the two runs match" and
/// "what did the guest exit with". A guest that deterministically exits nonzero
/// (e.g. `/bin/false`) makes a *passing* verification exit nonzero; symmetrically
/// a guest that exits zero while its runs diverge could only be told apart from a
/// match by scraping the human-readable banner. Carrying the verdict as its own
/// typed value removes that inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The two runs matched on every compared dimension (stdout, stderr, exit
    /// status, and — unless disabled — the internal DETLOG event stream) and
    /// every completed backend-specific invariant such as the DBT branch clock.
    Matched,
    /// The two runs diverged; verification failed.
    Diverged,
    /// Verification did not reach a verdict: the invocation aborted before the
    /// two runs could be compared (a run failed to start, the first run's exit
    /// status was rejected, SaBRe captured no DETLOG, recording failed, ...).
    ///
    /// This is NOT a synonym for `Diverged`. It exists so the `--verify-json`
    /// artifact always describes *this* invocation: without an explicit
    /// no-result state, an early abort would leave whatever the file previously
    /// contained -- including an older `{verified: true}` -- readable as though
    /// it described the run that just failed.
    NoResult,
}

/// How much log evidence the comparison actually consumed.
///
/// A configured-strict comparison proves nothing if it had no data: two empty
/// selections "match" trivially. Carrying the counts with the verdict is what
/// lets a parity consumer require nonzero executed work, exactly as a test
/// result must carry its executed count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ComparedLogCounts {
    /// Messages selected for comparison from the first run.
    pub left: usize,
    /// Messages selected for comparison from the second run.
    pub right: usize,
}

impl ComparedLogCounts {
    /// True when both sides actually contributed messages to the comparison.
    pub fn is_nonzero(&self) -> bool {
        self.left > 0 && self.right > 0
    }
}

/// The typed whole-process DBT counted-branch clocks compared for one verdict.
///
/// This is absent for non-DBT verification and for `no_result`: neither case
/// performed a complete two-run DBT clock comparison. Equal values document a
/// checked invariant on a match; unequal values name the backend-specific
/// dimension that made the verdict diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DbtCountedBranchComparison {
    /// Whole-process counted branches from run 1.
    pub left: u64,
    /// Whole-process counted branches from run 2.
    pub right: u64,
}

impl DbtCountedBranchComparison {
    /// Whether both completed runs reported the same counted-branch clock.
    pub fn matched(self) -> bool {
        self.left == self.right
    }
}

/// The full outcome of comparing two runs: the verification [`Verdict`] plus the
/// guest exit status, so a caller never has to infer either one from the other.
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    pub verdict: Verdict,
    /// Exit status of the second (replay / repeat) run, propagated verbatim.
    pub guest_status: ExitStatus,
    /// The exact common output/log comparison used for [`Self::verdict`],
    /// carried so a consumer never has to assume which comparison a "matched"
    /// rests on. Backend-specific dimensions are carried separately below.
    pub comparison: ComparisonSpec,
    /// How many log messages the comparison actually compared, or `None` when
    /// the log comparison was not run at all (output-only fallback). `None` and
    /// `Some(0/0)` are both "no log evidence" and neither can support parity.
    pub compared_log_messages: Option<ComparedLogCounts>,
    /// Typed whole-process DBT branch clocks, when that backend completed both
    /// runs and produced readable statistics. `None` for non-DBT runs and when
    /// verification did not reach a verdict.
    pub dbt_counted_branches: Option<DbtCountedBranchComparison>,
    /// Reader-facing names of the two compared sides, retained so terminal
    /// diagnostics do not assume that every comparison was two fresh runs.
    pub compared_labels: ComparisonSideLabels,
    /// Scheduler turn at the first log divergence, when a preceding COMMIT
    /// identified the turn.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Virtual nanoseconds at that same COMMIT, when the log recorded them.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// 1-based index of the first differing compared record.
    ///
    /// The two fields above are the position of the preceding scheduler COMMIT,
    /// so when no COMMIT precedes the differing record they both collapse to the
    /// origin and say nothing about how far the run got. This one is the
    /// LOCATION of the divergence rather than a bound on it, and it shares a
    /// unit with `compared_log_messages`, so `record / compared` is the fraction
    /// of the log that was deterministic.
    pub first_divergent_record: Option<usize>,
    /// How many syscalls the guest had COMPLETED when the divergence appeared,
    /// read from detcore's own `finish syscall #N` counter.
    ///
    /// The fourth unit, and the one closest to what a person debugging actually
    /// pictures: "the guest got 37 syscalls in" is more legible than a record
    /// index. It is NOT interchangeable with `first_divergent_record` -- one
    /// counts guest work, the other counts compared log records, and the two
    /// move at completely different rates.
    ///
    /// `null` when the logs matched, when no comparison ran, or when no syscall
    /// had completed before the divergence -- which is a real state, not a
    /// missing value: a run can diverge during startup.
    pub first_divergent_syscall: Option<u64>,
}

impl VerificationOutcome {
    /// Did verification pass, independent of the guest exit code?
    pub fn verified(&self) -> bool {
        self.verdict == Verdict::Matched
    }

    /// Collapse the outcome to the historical process-exit convention: a match
    /// propagates the guest exit status; a divergence is an error (nonzero
    /// exit). Callers that need to separate the verdict from the guest exit
    /// code must read [`Self::verdict`] / [`Self::verified`] (or the
    /// `--verify-json` report) *before* calling this.
    pub fn into_exit_status(self) -> Result<ExitStatus, Error> {
        match self.verdict {
            Verdict::Matched => Ok(self.guest_status),
            Verdict::Diverged => match self.dbt_counted_branches {
                Some(branches) if !branches.matched() => Err(Error::msg(format!(
                    "DBT verification failed: counted-branch clocks differed between runs ({} != {}); logs retained",
                    branches.left, branches.right
                ))),
                _ => Err(Error::msg(format!(
                    "Verification found a mismatch between {} and {} (logs retained).",
                    self.compared_labels.left, self.compared_labels.right,
                ))),
            },
            // Reached when the comparator refused (a truncated log) and nothing
            // else was observed to differ. Still an error, so the historical
            // nonzero process exit is unchanged -- but it must not be reported
            // as a mismatch, because no comparison established one.
            Verdict::NoResult => Err(Error::msg(
                "Verification did not reach a verdict (no comparison was performed).",
            )),
        }
    }
}

/// Machine-readable verification report written by `--verify-json`.
///
/// Every field carries the condition it describes: `verified`/`verdict` is the
/// verification result, `comparison` is the comparison that produced it, and
/// `guest_exit_code`/`guest_signal` describe the guest's own termination. A
/// consumer keys its decision on `verified` — but a *parity* consumer must not:
/// `verified` under a stripped comparison, an opaque filtered subset, or an
/// output-only fallback is not a bitwise-parity claim. Such a consumer reads
/// [`Self::bitwise_parity`] (or checks the `comparison` fields directly), which
/// is `true` only when the verdict rests on a full-INFO, named canonical record
/// envelope and unstripped log comparison.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    /// True iff the two runs matched (the verdict as a boolean).
    pub verified: bool,
    /// True iff the runs matched *and* the comparison that certified the match
    /// satisfies the bitwise INFO-parity contract (see
    /// [`ComparisonSpec::is_bitwise_parity`]). A determinism / record-replay
    /// ratchet keys on this single boolean; it can never be silently weakened to
    /// a stripped or filtered compare because a stripped/filtered match sets it
    /// `false`.
    pub bitwise_parity: bool,
    /// The verdict as a stable string ("matched" / "diverged").
    pub verdict: Verdict,
    /// The common output/log comparison used for the verdict. Without this a
    /// bitwise-parity consumer cannot distinguish a stripped match from a bitwise one.
    /// Backend-specific dimensions are carried separately below. `null`
    /// when no verdict was reached (see [`Verdict::NoResult`]).
    pub comparison: Option<ComparisonSpec>,
    /// How many messages in [`ComparisonSpec::log_scope`] were actually compared.
    /// `null` means the log comparison did not run. A strict *configuration* is
    /// not proof that the configured comparison had data, so this count is what makes
    /// [`Self::bitwise_parity`] falsifiable.
    pub compared_log_messages: Option<ComparedLogCounts>,
    /// Typed whole-process DBT counted-branch clocks. Omitted for non-DBT
    /// verification and for `no_result`; unequal values identify a branch-clock
    /// divergence that can exist even when the canonical log comparison matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbt_counted_branches: Option<DbtCountedBranchComparison>,
    /// The guest's exit code, if it exited normally.
    pub guest_exit_code: Option<i32>,
    /// The guest's terminating signal number, if it was killed by a signal.
    pub guest_signal: Option<i32>,
    /// Scheduler turn at the first log divergence, or `null` when the logs
    /// matched or no preceding COMMIT metadata was available.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Virtual nanoseconds at that same point, with the same nullability rules.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// 1-based index of the first differing compared record, or `null` when the
    /// logs matched or no comparison ran. `null` and `0` are NOT the same claim:
    /// null is "no divergence located", while a hypothetical 0 would mean the
    /// very first record differed. Nothing writes 0 -- the index is 1-based.
    pub first_divergent_record: Option<usize>,
    /// How many syscalls the guest had COMPLETED when the divergence appeared,
    /// from detcore's own `finish syscall #N` counter. See the identically
    /// named field on [`VerificationOutcome`].
    pub first_divergent_syscall: Option<u64>,
}

impl VerificationReport {
    /// The record stamped before verification runs: no verdict has been reached
    /// yet, so nothing may read as verified or as parity.
    pub fn no_result() -> Self {
        VerificationReport {
            verified: false,
            bitwise_parity: false,
            verdict: Verdict::NoResult,
            comparison: None,
            compared_log_messages: None,
            dbt_counted_branches: None,
            guest_exit_code: None,
            guest_signal: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
        }
    }
}

impl From<&VerificationOutcome> for VerificationReport {
    fn from(outcome: &VerificationOutcome) -> Self {
        VerificationReport {
            verified: outcome.verified(),
            // Bitwise parity is a conjunction: the runs matched AND the
            // comparison was strict enough for the match to *mean* bitwise
            // identity. A `Diverged` verdict is never bitwise parity.
            // Three-way conjunction: the runs matched, the comparison was
            // strict enough for the match to *mean* bitwise identity, AND that
            // comparison actually consumed log evidence. The third conjunct is
            // not redundant: an empty-vs-empty log comparison reports "no
            // difference" under the strictest possible spec, so without a
            // nonzero count a run that produced no DETLOG at all would certify
            // as bitwise parity.
            bitwise_parity: outcome.verified()
                && outcome.comparison.is_bitwise_parity()
                && outcome
                    .compared_log_messages
                    .is_some_and(|counts| counts.is_nonzero()),
            verdict: outcome.verdict,
            comparison: Some(outcome.comparison),
            compared_log_messages: outcome.compared_log_messages,
            dbt_counted_branches: if outcome.verdict == Verdict::NoResult {
                None
            } else {
                outcome.dbt_counted_branches
            },
            guest_exit_code: outcome.guest_status.code(),
            guest_signal: outcome.guest_status.signal(),
            first_divergent_scheduler_turn: outcome.first_divergent_scheduler_turn,
            first_divergent_virtual_nanoseconds: outcome.first_divergent_virtual_nanoseconds,
            first_divergent_record: outcome.first_divergent_record,
            first_divergent_syscall: outcome.first_divergent_syscall,
        }
    }
}

/// Write the verification report as a single JSON line to `path`.
///
/// This is the exit-code-independent verdict channel: the record it writes is
/// true or false based on whether verification matched, regardless of what the
/// guest exited with.
pub fn write_verification_json(path: &Path, outcome: &VerificationOutcome) -> Result<(), Error> {
    write_report_json(path, &VerificationReport::from(outcome))
}

/// Publish an explicit NO-RESULT record to `path` *before* verification starts.
///
/// This is what makes the artifact invocation-bound. `write_verification_json`
/// can only run once a verdict exists, but a `--verify-json` run has several
/// earlier exits (a run that fails to start, a rejected first-run status, a
/// SaBRe capture with zero DETLOG, a failed recording). If the caller reuses a
/// path, every one of those exits would otherwise leave the PREVIOUS
/// invocation's record -- possibly `{"verified":true,"bitwise_parity":true}` --
/// sitting there, readable as if it described the invocation that just failed.
/// Stamping a no-result first means the file always describes *this* run: it is
/// either the terminal verdict or an honest "no verdict reached".
pub fn write_pending_verification_json(path: &Path) -> Result<(), Error> {
    write_report_json(path, &VerificationReport::no_result())
}

/// Write `report` to `path` atomically: a reader concurrent with the write sees
/// either the old contents or the complete new record, never a truncated one.
/// The directory to stage the temporary record in: always the one the target
/// lives in, so `persist` is a same-filesystem rename.
///
/// `Path::parent` returns an EMPTY path for a bare filename, not `"."`. Falling
/// back to the system temp directory for that case (as this did) puts the
/// staged file on a different filesystem than the target whenever `TMPDIR` and
/// the working directory differ -- the common case, e.g. tmpfs `/tmp` beside a
/// btrfs checkout. `persist` then fails with `EXDEV` and the caller returns an
/// error, leaving whatever the file previously held: a stale
/// `{verified:true}` survives precisely the invocation that was supposed to
/// overwrite it. A bare filename means the working directory, so say so.
fn staging_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn write_report_json(path: &Path, report: &VerificationReport) -> Result<(), Error> {
    use std::io::Write as _;

    let json = serde_json::to_string(report)?;
    // Same directory as the target so the rename below stays within one
    // filesystem and is therefore atomic.
    let mut temp = NamedTempFile::new_in(staging_directory(path))
        .with_context(|| format!("creating a temporary file beside {}", path.display()))?;
    writeln!(temp, "{json}")
        .with_context(|| format!("writing verification verdict for {}", path.display()))?;
    temp.flush()
        .with_context(|| format!("flushing verification verdict for {}", path.display()))?;
    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("publishing verification verdict to {}", path.display()))?;
    Ok(())
}

/// Reject an explicit log level that would suppress the events verification compares.
pub(crate) fn validate_log_level(global: &GlobalOpts) -> Result<(), Error> {
    if let Some(level) = global.log
        && level < LevelFilter::INFO
    {
        anyhow::bail!(
            "--verify requires --log=info or a more verbose level; received --log={}",
            level.to_string().to_ascii_lowercase()
        );
    }
    Ok(())
}

/// Resolve the capture verbosity independently from the comparison scope.
///
/// Canonical verification defaults to INFO because INFO is the declared
/// `BitwiseInfoV1` observation envelope. An explicit DEBUG/TRACE request is
/// preserved in the capture for diagnostics, but ordinary canonical comparison
/// still selects INFO. Legacy stripped verification keeps its DEBUG default.
/// The explicit full-trace diagnostic mode requires TRACE regardless of a lower
/// requested level.
pub(crate) fn verification_log_level(
    requested: Option<LevelFilter>,
    strictness: LogCompareStrictness,
    diagnostic_full_trace: bool,
) -> LevelFilter {
    if diagnostic_full_trace {
        requested
            .unwrap_or(LevelFilter::TRACE)
            .max(LevelFilter::TRACE)
    } else {
        requested.unwrap_or(match strictness {
            LogCompareStrictness::Stripped => LevelFilter::DEBUG,
            LogCompareStrictness::Canonical => LevelFilter::INFO,
        })
    }
}

pub fn temp_log_files(name1: &str, name2: &str) -> io::Result<(NamedTempFile, NamedTempFile)> {
    temp_log_files_in(name1, name2, None)
}

pub fn temp_log_files_in(
    name1: &str,
    name2: &str,
    directory: Option<&Path>,
) -> io::Result<(NamedTempFile, NamedTempFile)> {
    let create = |name: &str| {
        let prefix = format!("{}_log_", name);
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix).rand_bytes(5);
        match directory {
            Some(directory) => builder.tempfile_in(directory),
            None => builder.tempfile(),
        }
    };
    let file1 = create(name1)?;
    let file2 = create(name2)?;

    Ok((file1, file2))
}

pub fn setup_double_run(
    global: &GlobalOpts,
    name1: &str,
    name2: &str,
    strictness: LogCompareStrictness,
) -> ((GlobalOpts, NamedTempFile), (GlobalOpts, NamedTempFile)) {
    let (file1, file2) = temp_log_files(name1, name2).unwrap();

    let path1 = PathBuf::from(file1.path());
    let path2 = PathBuf::from(file2.path());

    // Override global settings.  Unfortunately we lose the log output to the
    // screen.
    let mut global = global.clone();
    global.log_file = Some(path1);
    global.log = Some(verification_log_level(global.log, strictness, false));

    let mut global2 = global.clone();
    global2.log_file = Some(path2);
    ((global, file1), (global2, file2))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review re-emitting aggregate warnings captured by verification.
fn unsupported_syscalls_from_log(path: &Path) -> io::Result<BTreeSet<String>> {
    let mut syscalls = BTreeSet::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let Some((_, remainder)) = line.split_once("syscalls ") else {
            continue;
        };
        let Some((names, _)) = remainder.split_once(" used but not yet supported") else {
            continue;
        };
        for name in names.split(',') {
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                syscalls.insert(name.to_owned());
            }
        }
    }
    Ok(syscalls)
}

pub(crate) fn retain_verification_logs<const N: usize>(
    logs: [(&str, TempPath); N],
) -> Result<Vec<PathBuf>, Error> {
    let mut retained = Vec::with_capacity(N);
    eprintln!(":: Verification logs retained:");
    for (label, log) in logs {
        let path = log.keep()?;
        eprintln!("::   {label}: {}", path.display());
        retained.push(path);
    }
    Ok(retained)
}

pub fn compare_two_runs(
    first: ComparedRun<'_>,
    second: ComparedRun<'_>,
    options: ComparisonOptions,
) -> Result<VerificationOutcome, Error> {
    compare_two_runs_with_unsupported_scan(first, second, options, unsupported_syscalls_from_log)
}

fn compare_two_runs_with_unsupported_scan(
    first: ComparedRun<'_>,
    second: ComparedRun<'_>,
    options: ComparisonOptions,
    scan_unsupported_syscalls: impl Fn(&Path) -> io::Result<BTreeSet<String>>,
) -> Result<VerificationOutcome, Error> {
    let ComparedRun {
        output: out1,
        log: log1,
        label: label1,
    } = first;
    let ComparedRun {
        output: out2,
        log: log2,
        label: label2,
    } = second;
    let compared_labels = ComparisonSideLabels::new(label1, label2);
    let mut failed = false;
    // A difference that was actually OBSERVED, as opposed to a comparison that
    // was refused. Only an observed difference can justify a `Diverged`
    // verdict; see the verdict selection at the end of this function.
    let mut observed_divergence = false;
    // The log comparator declined to produce a verdict (a truncated input).
    let mut comparison_refused = false;
    // None until the log comparison actually runs; stays None on the
    // output-only (KVM concurrent) fallback so the report can distinguish
    // "compared nothing" from "compared and matched".
    let mut compared_log_messages: Option<ComparedLogCounts> = None;
    let mut first_divergent_scheduler_turn = None;
    let mut first_divergent_virtual_nanoseconds = None;
    let mut first_divergent_record = None;
    let mut first_divergent_syscall = None;

    // Resolve the strictness label to concrete diff flags once, and carry the
    // resulting spec through to the verdict so the returned outcome records
    // exactly which comparison certified it.
    let spec = ComparisonSpec::new(
        options.strictness,
        options.compare_logs,
        options.diagnostic_full_trace,
        options.compare_io_buffers,
        options.record_envelope.policy(),
    );

    if out1.stdout != out2.stdout {
        failed = true;
        observed_divergence = true;
        eprintln!("Mismatch in stdout between {label1} and {label2}:");
        let str1 = String::from_utf8_lossy(&out1.stdout);
        let str2 = String::from_utf8_lossy(&out2.stdout);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    if out1.stderr != out2.stderr {
        failed = true;
        observed_divergence = true;
        eprintln!("Mismatch in stderr between {label1} and {label2}:");
        let str1 = String::from_utf8_lossy(&out1.stderr);
        let str2 = String::from_utf8_lossy(&out2.stderr);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    let log_processing_result = (|| -> Result<(), Error> {
        if options.compare_logs {
            eprintln!(
                ":: {}",
                "Comparing captured verification logs...".yellow().bold()
            );
            // The comparison semantics come from `spec` (strip_lines + mode); only
            // the printed syscall-history depth still tracks `verbose`. Historically
            // both were flipped together, so the sole bitwise comparison was also the
            // loudest — decoupling them lets a quiet run be bitwise-strict.
            let diff_options = logdiff::LogDiffOpts {
                strip_lines: spec.strip_lines,
                // Thread canonical address normalization from the spec so the parity
                // (`Canonical`) policy actually rewrites host addresses to ordinals
                // in the engine; without this the verdict would REPORT
                // `canonicalize_addresses = true` while the diff ran with the raw
                // addresses — the exact proxy/binding drift the spec exists to close.
                canonicalize_addresses: spec.canonicalize_addresses,
                comparison: spec.log_comparison_mode(),
                side_labels: compared_labels.clone(),
                syscall_history: if options.verbose { 10 } else { 5 },
                // Thread the filter facts from the spec so what the verdict *reports*
                // (`spec.skip_commit`/`spec.skip_detlog`) is exactly what the diff
                // engine *does*; the remaining filters stay at their no-op defaults.
                skip_commit: spec.skip_commit,
                skip_detlog: spec.skip_detlog,
                ..Default::default()
            };
            // Bind the spec's recorded filter-absence to the engine's real defaults:
            // if `LogDiffOpts::default()` ever grew a filtering default, the spec
            // would silently misreport "no filters", so refuse to run in that case.
            debug_assert!(
                diff_options.ignore_lines.is_empty() != spec.ignore_lines,
                "ComparisonSpec.ignore_lines must match the diff engine's ignore_lines"
            );

            let summary = logdiff::try_log_diff_detailed_with_filter(
                log1.as_ref(),
                log2.as_ref(),
                &diff_options,
                options.record_envelope.predicate(),
            )?;
            compared_log_messages = Some(ComparedLogCounts {
                left: summary.compared_left,
                right: summary.compared_right,
            });
            if summary.diff_found {
                failed = true;
                if summary.refused {
                    comparison_refused = true;
                } else {
                    observed_divergence = true;
                }
                first_divergent_scheduler_turn = summary.first_divergent_scheduler_turn;
                first_divergent_virtual_nanoseconds = summary.first_divergent_virtual_nanoseconds;
                // Set inside this `diff_found` arm, matching its two siblings
                // above.
                //
                // The arm is DEFENCE, not the source of the guarantee, and the
                // difference matters to whoever edits this next: a matching pair
                // already yields `None` from the comparator, because
                // `first_divergent_record` is computed from `first_different`,
                // which is `None` when nothing differed. Deleting this arm would
                // therefore NOT produce a `0` today, and no test would catch its
                // removal -- verified by deleting the equivalent gate in
                // `logdiff.rs` and watching
                // `identical_logs_have_no_first_divergent_record` still pass.
                //
                // It is kept because it costs nothing and it is what holds the
                // invariant if the comparator ever starts reporting a position
                // on a match.
                first_divergent_record = summary.first_divergent_record;
                first_divergent_syscall = summary.first_divergent_syscall;
                if !summary.refused {
                    eprintln!(
                        ":: {}",
                        format!("Log differences found between {label1} and {label2}.")
                            .red()
                            .bold()
                    );
                }
            }
        } else {
            eprintln!(
                ":: KVM concurrent mode: comparing guest output and exit status; internal syscall trace order is not deterministic"
            );
        }

        if out1.status != out2.status {
            failed = true;
            observed_divergence = true;
            eprintln!(
                "Mismatch in exit status between {label1} and {label2}: {}",
                Comparison::new(&out1.status, &out2.status)
            );
        }

        if !failed {
            let mut unsupported = scan_unsupported_syscalls(log1.as_ref())?;
            unsupported.extend(scan_unsupported_syscalls(log2.as_ref())?);
            if let Some(message) = detcore::format_unsupported_syscall_warning(&unsupported) {
                eprintln!("WARNING: {message}");
            }
        }
        Ok(())
    })();

    if let Err(error) = log_processing_result {
        if options.keep_logs || failed {
            retain_verification_logs([(label1, log1), (label2, log2)])?;
        }
        return Err(error);
    }

    // Divergence historically retained both diagnostics. `--keep-logs` extends
    // that behavior to successful comparisons instead of changing the failure
    // path.
    if options.keep_logs || failed {
        retain_verification_logs([(label1, log1), (label2, log2)])?;
    }

    if failed {
        // A refused comparison is a NO-RESULT, not a divergence. A real
        // stdout/stderr/exit-status mismatch still outranks the refusal.
        let verdict = if comparison_refused && !observed_divergence {
            Verdict::NoResult
        } else {
            Verdict::Diverged
        };
        // Divergence is a verification *verdict*, not an I/O error: return it as
        // a value carrying the guest exit status. Callers that want the
        // historical "divergence -> nonzero process exit" behavior use
        // `VerificationOutcome::into_exit_status`.
        Ok(VerificationOutcome {
            verdict,
            guest_status: out2.status,
            comparison: spec,
            compared_log_messages,
            dbt_counted_branches: None,
            compared_labels: compared_labels.clone(),
            first_divergent_scheduler_turn,
            first_divergent_virtual_nanoseconds,
            first_divergent_record,
            first_divergent_syscall,
        })
    } else {
        Ok(VerificationOutcome {
            verdict: Verdict::Matched,
            guest_status: out2.status,
            comparison: spec,
            compared_log_messages,
            dbt_counted_branches: None,
            compared_labels: compared_labels.clone(),
            first_divergent_scheduler_turn,
            first_divergent_virtual_nanoseconds,
            first_divergent_record,
            first_divergent_syscall,
        })
    }
}

/// Announce a verification verdict only after its machine-readable report has
/// been published.
///
/// Keeping this outside [`compare_two_runs`] closes the interval in which the
/// CLI previously printed `Success` and an outer bucket timeout could kill it
/// before `write_verification_json` replaced the pending `no_result` report.
pub fn announce_verification_outcome(
    outcome: &VerificationOutcome,
    success_message: &str,
    failure_message: &str,
) {
    match outcome.verdict {
        Verdict::Matched => eprintln!(":: {}", success_message.green().bold()),
        Verdict::Diverged => eprintln!(":: {}", failure_message.red().bold()),
        Verdict::NoResult => eprintln!(
            ":: {}",
            "No result: the comparison was refused, so this is neither a match nor a difference."
                .red()
                .bold()
        ),
    }
}

fn display_diff(left: &str, right: &str) {
    for result in diff::lines(left, right) {
        match result {
            diff::Result::Left(s) => {
                eprintln!("- {}", s.red());
            }
            diff::Result::Right(s) => {
                eprintln!("+ {}", s.green());
            }
            diff::Result::Both(s, _) => {
                eprintln!("  {}", s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::Exited(status),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn empty_logs() -> (TempPath, TempPath) {
        let (left, right) = temp_log_files("verify_left", "verify_right").unwrap();
        (left.into_temp_path(), right.into_temp_path())
    }

    /// Two logs carrying identical, NONEMPTY comparable content. Distinct from
    /// [`empty_logs`]: an empty-vs-empty comparison is a no-result, so any test
    /// asserting a *parity* green must use this.
    fn logs_with_identical_detlog() -> (TempPath, TempPath) {
        let (left, right) = temp_log_files("verify_left", "verify_right").unwrap();
        let left_path = left.into_temp_path();
        let right_path = right.into_temp_path();
        let body = format!("{}{}", detlog_with_value(1), detlog_with_value(2));
        fs::write(&left_path, body.as_bytes()).unwrap();
        fs::write(&right_path, body.as_bytes()).unwrap();
        (left_path, right_path)
    }

    fn global_with_log(log: Option<LevelFilter>) -> GlobalOpts {
        GlobalOpts {
            log,
            log_file: None,
            log_file_handle: None,
            backend: None,
        }
    }

    #[test]
    fn verify_rejects_explicit_log_levels_below_info() {
        for level in [LevelFilter::OFF, LevelFilter::ERROR, LevelFilter::WARN] {
            let error = validate_log_level(&global_with_log(Some(level))).unwrap_err();
            assert!(
                error.to_string().contains("requires --log=info"),
                "unexpected error for {level}: {error}"
            );
        }
    }

    #[test]
    fn verify_accepts_default_and_info_or_more_verbose_logs() {
        for level in [
            None,
            Some(LevelFilter::INFO),
            Some(LevelFilter::DEBUG),
            Some(LevelFilter::TRACE),
        ] {
            validate_log_level(&global_with_log(level)).unwrap();
        }
    }

    #[test]
    fn verification_capture_level_honors_info_and_preserves_explicit_debug() {
        assert_eq!(
            verification_log_level(None, LogCompareStrictness::Canonical, false),
            LevelFilter::INFO
        );
        assert_eq!(
            verification_log_level(
                Some(LevelFilter::INFO),
                LogCompareStrictness::Canonical,
                false,
            ),
            LevelFilter::INFO
        );
        assert_eq!(
            verification_log_level(
                Some(LevelFilter::DEBUG),
                LogCompareStrictness::Canonical,
                false,
            ),
            LevelFilter::DEBUG,
            "explicit DEBUG remains captured for diagnostics"
        );
        assert_eq!(
            verification_log_level(None, LogCompareStrictness::Stripped, false),
            LevelFilter::DEBUG,
            "legacy stripped verification keeps its default"
        );
        assert_eq!(
            verification_log_level(
                Some(LevelFilter::INFO),
                LogCompareStrictness::Stripped,
                false,
            ),
            LevelFilter::INFO,
            "an explicit INFO request must not be promoted"
        );
        assert_eq!(
            verification_log_level(
                Some(LevelFilter::INFO),
                LogCompareStrictness::Canonical,
                true,
            ),
            LevelFilter::TRACE,
            "explicit full-trace diagnostics require TRACE capture"
        );
    }

    fn compare_with(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
        strictness: LogCompareStrictness,
    ) -> Result<VerificationOutcome, Error> {
        compare_with_envelope(
            left,
            left_log,
            right,
            right_log,
            strictness,
            RecordEnvelope::all_records_v1(),
        )
    }

    fn compare_with_envelope(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
        strictness: LogCompareStrictness,
        record_envelope: RecordEnvelope,
    ) -> Result<VerificationOutcome, Error> {
        compare_two_runs(
            ComparedRun {
                output: left,
                log: left_log,
                label: "run 1",
            },
            ComparedRun {
                output: right,
                log: right_log,
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs: false,
                record_envelope,
            },
        )
    }

    // The default (stripped) comparison, matching what a bare `--verify` runs.
    fn compare(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
    ) -> Result<VerificationOutcome, Error> {
        compare_with(
            left,
            left_log,
            right,
            right_log,
            LogCompareStrictness::Stripped,
        )
    }

    /// A DETLOG log message whose only variable is a numeric syscall value. The
    /// leading tag lets `extract_log_messages` accept it; " DETLOG " + "detcore:"
    /// let it survive the deterministic-message filter.
    fn detlog_with_value(value: u64) -> String {
        format!(
            "2026-08-06T01:00:00.000000Z INFO detcore: [dtid 2] DETLOG [syscall] write(fd=1, count={value})\n"
        )
    }

    #[test]
    fn extracts_unsupported_syscall_warning_union_from_logs() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            b"2026 WARN syscalls vmsplice,getppid used but not yet supported\ninvalid\n",
        )
        .unwrap();

        assert_eq!(
            unsupported_syscalls_from_log(file.path()).unwrap(),
            BTreeSet::from(["getppid".to_owned(), "vmsplice".to_owned()])
        );
    }

    /// Two logs carrying identical comparable content that were both cut at the
    /// bounded writer's size bound, i.e. each ends with the truncation marker
    /// on a line of its own.
    fn logs_truncated_at_the_bound() -> (TempPath, TempPath) {
        let (left, right) = temp_log_files("verify_left", "verify_right").unwrap();
        let left_path = left.into_temp_path();
        let right_path = right.into_temp_path();
        let body = format!(
            "{}{}{}\n",
            detlog_with_value(1),
            detlog_with_value(2),
            detcore::logdiff::TRUNCATION_MARKER
        );
        fs::write(&left_path, body.as_bytes()).unwrap();
        fs::write(&right_path, body.as_bytes()).unwrap();
        (left_path, right_path)
    }

    /// A refused comparison must publish `NoResult`, not `Diverged`.
    ///
    /// The banner already said "This is a NO-RESULT, not a difference and not a
    /// match" while the typed field in the same invocation said `diverged`.
    /// Those are contradictory claims about the same run, and the typed one is
    /// what a machine reads. `Diverged` asserts the two executions were
    /// observed to differ; nothing here observed anything.
    #[test]
    fn a_refused_comparison_is_a_no_result_not_a_divergence() {
        let left = output(0, b"hello\n", b"");
        let right = left.clone();
        let (log1, log2) = logs_truncated_at_the_bound();

        let outcome = compare(&left, log1, &right, log2).unwrap();
        assert_eq!(
            outcome.verdict,
            Verdict::NoResult,
            "a refused comparison must not be reported as a divergence"
        );
        assert!(!outcome.verified(), "a no-result is never verified");
        // The failure direction is unchanged: still not a match, still nonzero
        // compared counts of zero, still a nonzero process exit.
        assert_eq!(
            outcome.compared_log_messages,
            Some(ComparedLogCounts { left: 0, right: 0 })
        );
        let report = VerificationReport::from(&outcome);
        assert!(!report.verified);
        assert!(!report.bitwise_parity);
        assert!(
            outcome.into_exit_status().is_err(),
            "a no-result must still exit nonzero; it is not a pass"
        );
    }

    /// The direction that matters most: truncation must never DOWNGRADE a real
    /// divergence into "we didn't look".
    ///
    /// `NoResult` is a weaker claim than `Diverged`, so emitting it when the
    /// runs actually differed would lose a finding. Each case below pairs the
    /// same refused log comparison with a genuine observed difference, and each
    /// must still report `Diverged`.
    #[test]
    fn an_observed_difference_outranks_a_refused_comparison() {
        for (label, left, right) in [
            (
                "stdout",
                output(0, b"hello\n", b""),
                output(0, b"goodbye\n", b""),
            ),
            (
                "stderr",
                output(0, b"hello\n", b""),
                output(0, b"hello\n", b"oops\n"),
            ),
            (
                "exit status",
                output(0, b"hello\n", b""),
                output(3, b"hello\n", b""),
            ),
        ] {
            let (log1, log2) = logs_truncated_at_the_bound();
            let outcome = compare(&left, log1, &right, log2).unwrap();
            assert_eq!(
                outcome.verdict,
                Verdict::Diverged,
                "{label} differed, so the verdict must be Diverged even though the log \
                 comparison was refused"
            );
            assert!(!outcome.verified());
        }
    }

    #[test]
    fn identical_outputs_verify_successfully() {
        let left = output(0, b"hello\n", b"");
        let right = left.clone();
        let (log1, log2) = empty_logs();

        let outcome = compare(&left, log1, &right, log2).unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert!(outcome.verified());
        assert_eq!(outcome.guest_status, ExitStatus::Exited(0));
        // The default `--verify` path is a stripped comparison; the verdict says so.
        assert_eq!(
            outcome.comparison.strictness,
            LogCompareStrictness::Stripped
        );
        assert!(outcome.comparison.strip_lines);
        assert!(!outcome.comparison.full_trace);
    }

    // Direction 1 of the exit-code/verdict decoupling: a guest that exits
    // NONZERO but whose two runs match must report VERIFIED. Before the verdict
    // was separated from the exit code, the propagated `Exited(3)` was the only
    // signal a caller had, so a passing verification of `/bin/false`-like
    // programs was indistinguishable from a failure.
    #[test]
    fn nonzero_exit_with_matching_outputs_reports_verified() {
        let left = output(3, b"hello\n", b"oops\n");
        let right = left.clone();
        let (log1, log2) = empty_logs();

        let outcome = compare(&left, log1, &right, log2).unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert!(outcome.verified());
        // The guest status is preserved verbatim, carried *beside* the verdict.
        assert_eq!(outcome.guest_status, ExitStatus::Exited(3));
        // The structured report a `--verify-json` consumer would read:
        let report = VerificationReport::from(&outcome);
        assert!(report.verified);
        assert_eq!(report.guest_exit_code, Some(3));
        assert_eq!(report.guest_signal, None);
        // The report also carries the comparison that produced the verdict.
        assert_eq!(
            report.comparison.unwrap().strictness,
            LogCompareStrictness::Stripped
        );
        // Collapsing to the legacy exit convention still propagates the guest
        // code; the verdict channel above is what a caller keys on.
        assert_eq!(outcome.into_exit_status().unwrap(), ExitStatus::Exited(3));
    }

    fn diverged_message(label1: &'static str, label2: &'static str) -> String {
        let left = output(0, b"left-output", b"");
        let right = output(0, b"right-output", b"");
        let (left_log, right_log) = empty_logs();
        let left_path = left_log.to_path_buf();
        let right_path = right_log.to_path_buf();

        let outcome = compare_two_runs(
            ComparedRun {
                output: &left,
                log: left_log,
                label: label1,
            },
            ComparedRun {
                output: &right,
                log: right_log,
                label: label2,
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Stripped,
                compare_logs: false,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs: false,
                record_envelope: RecordEnvelope::all_records_v1(),
            },
        )
        .unwrap();

        assert_eq!(
            outcome.compared_labels,
            ComparisonSideLabels::new(label1, label2)
        );
        assert!(left_path.exists(), "a divergence must retain the left log");
        assert!(
            right_path.exists(),
            "a divergence must retain the right log"
        );
        fs::remove_file(left_path).unwrap();
        fs::remove_file(right_path).unwrap();
        outcome.into_exit_status().unwrap_err().to_string()
    }

    #[test]
    fn divergence_terminal_message_names_the_sides_it_compared() {
        let record = diverged_message("the recording", "the replay");
        assert!(
            record.contains("between the recording and the replay"),
            "{record}"
        );
        assert!(!record.contains("run 1") && !record.contains("run 2"));

        let run = diverged_message("run 1", "run 2");
        assert!(run.contains("between run 1 and run 2"), "{run}");
        assert!(!run.contains("recording") && !run.contains("replay"));
    }

    #[test]
    fn production_comparison_callers_bind_truthful_side_labels() {
        for (name, source, left, right) in [
            ("generic run", include_str!("run.rs"), "run 1", "run 2"),
            ("DBT run", include_str!("backends.rs"), "run 1", "run 2"),
            (
                "record/replay",
                include_str!("record_start.rs"),
                "the recording",
                "the replay",
            ),
        ] {
            let production = source
                .rsplit_once("#[cfg(test)]")
                .expect("production/test boundary")
                .0;
            for label in [left, right] {
                let binding = format!("label: \"{label}\"");
                assert_eq!(
                    production.matches(&binding).count(),
                    1,
                    "{name} must bind {label:?} exactly once"
                );
            }
        }

        let production = include_str!("verify.rs")
            .rsplit_once("#[cfg(test)]")
            .expect("production/test boundary")
            .0;
        assert!(production.contains("side_labels: compared_labels.clone()"));
        assert_eq!(
            production
                .matches("retain_verification_logs([(label1, log1), (label2, log2)])")
                .count(),
            2,
            "both retained-log paths must use the caller-bound labels"
        );
    }

    #[test]
    fn output_only_mode_ignores_internal_log_order() {
        let left = output(0, b"console", b"warning");
        let right = output(0, b"console", b"warning");
        let (left_log, right_log) = empty_logs();
        fs::write(&left_log, "DETLOG root event A\n").unwrap();
        fs::write(&right_log, "DETLOG root event B\n").unwrap();

        let outcome = compare_two_runs(
            ComparedRun {
                output: &left,
                log: left_log,
                label: "run 1",
            },
            ComparedRun {
                output: &right,
                log: right_log,
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Stripped,
                compare_logs: false,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs: false,
                record_envelope: RecordEnvelope::all_records_v1(),
            },
        )
        .unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(outcome.guest_status, ExitStatus::Exited(0));
        // The verdict records that the log stream was NOT compared, so no
        // consumer can mistake this for a bitwise log-parity result.
        assert!(!outcome.comparison.compare_logs);
    }

    #[test]
    fn stdout_stderr_and_status_mismatches_fail_verification() {
        let baseline = output(0, b"hello\n", b"");
        let mismatches = [
            output(0, b"different\n", b""),
            output(0, b"hello\n", b"different\n"),
            output(1, b"hello\n", b""),
        ];

        for mismatch in mismatches {
            let (log1, log2) = empty_logs();
            let path1 = log1.to_path_buf();
            let path2 = log2.to_path_buf();

            let outcome = compare(&baseline, log1, &mismatch, log2).unwrap();
            assert_eq!(outcome.verdict, Verdict::Diverged);
            assert!(!outcome.verified());
            // Collapsing a divergence to the legacy exit convention is an error
            // (nonzero process exit), preserving the historical behavior.
            assert!(outcome.into_exit_status().is_err());

            let _ = fs::remove_file(path1);
            let _ = fs::remove_file(path2);
        }
    }

    #[test]
    fn comparison_spec_maps_strictness_to_concrete_flags() {
        let stripped = ComparisonSpec::new(
            LogCompareStrictness::Stripped,
            true,
            false,
            false,
            RecordEnvelopePolicy::AllRecordsV1,
        );
        assert!(stripped.strip_lines);
        assert!(!stripped.full_trace);
        assert_eq!(
            stripped.log_comparison_mode(),
            LogComparisonMode::Deterministic
        );

        let canonical = ComparisonSpec::new(
            LogCompareStrictness::Canonical,
            true,
            false,
            true,
            RecordEnvelopePolicy::AllRecordsV1,
        );
        assert!(!canonical.strip_lines);
        assert!(canonical.canonicalize_addresses);
        assert!(canonical.exact_remainder);
        assert!(canonical.full_trace);
        assert_eq!(canonical.log_scope, ComparedLogScope::Info);
        assert_eq!(canonical.log_comparison_mode(), LogComparisonMode::Info);

        let diagnostic = ComparisonSpec::new(
            LogCompareStrictness::Canonical,
            true,
            true,
            true,
            RecordEnvelopePolicy::AllRecordsV1,
        );
        assert_eq!(diagnostic.log_scope, ComparedLogScope::FullTrace);
        assert_eq!(
            diagnostic.log_comparison_mode(),
            LogComparisonMode::FullTrace
        );
    }

    #[test]
    fn bitwise_info_ignores_debug_diagnostics_but_rejects_real_info_divergence() {
        let out = output(0, b"hello\n", b"");
        let make_logs = |right_info: u64| {
            let (left, right) = empty_logs();
            fs::write(
                &left,
                format!(
                    "{}2026-08-06T01:00:00.000001Z DEBUG detcore: diagnostic host timing=100\n",
                    detlog_with_value(7)
                ),
            )
            .unwrap();
            fs::write(
                &right,
                format!(
                    "{}2026-08-06T01:00:00.000002Z DEBUG detcore: diagnostic host timing=200\n",
                    detlog_with_value(right_info)
                ),
            )
            .unwrap();
            (left, right)
        };

        // Positive INFO bracket: the captured DEBUG diagnostics differ, while
        // the one INFO event on each side matches exactly.
        let (left, right) = make_logs(7);
        let matched =
            compare_with(&out, left, &out, right, LogCompareStrictness::Canonical).unwrap();
        assert_eq!(matched.verdict, Verdict::Matched);
        assert_eq!(matched.comparison.log_scope, ComparedLogScope::Info);
        assert_eq!(
            matched.compared_log_messages,
            Some(ComparedLogCounts { left: 1, right: 1 })
        );
        assert!(VerificationReport::from(&matched).bitwise_parity);

        // Negative INFO bracket: changing the actual INFO payload must fail even
        // though DEBUG remains outside the parity envelope.
        let (left, right) = make_logs(8);
        let left_path = left.to_path_buf();
        let right_path = right.to_path_buf();
        let info_diverged =
            compare_with(&out, left, &out, right, LogCompareStrictness::Canonical).unwrap();
        assert_eq!(info_diverged.verdict, Verdict::Diverged);
        let _ = fs::remove_file(left_path);
        let _ = fs::remove_file(right_path);

        // DEBUG is still available as an explicit diagnostic comparison. The
        // same matching INFO / differing DEBUG captures fail only when that
        // full-trace scope is requested.
        let (left, right) = make_logs(7);
        let left_path = left.to_path_buf();
        let right_path = right.to_path_buf();
        let debug_diverged = compare_two_runs(
            ComparedRun {
                output: &out,
                log: left,
                label: "run 1",
            },
            ComparedRun {
                output: &out,
                log: right,
                label: "run 2",
            },
            ComparisonOptions {
                verbose: true,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: true,
                compare_io_buffers: false,
                keep_logs: false,
                record_envelope: RecordEnvelope::all_records_v1(),
            },
        )
        .unwrap();
        assert_eq!(debug_diverged.verdict, Verdict::Diverged);
        assert_eq!(
            debug_diverged.comparison.log_scope,
            ComparedLogScope::FullTrace
        );
        let _ = fs::remove_file(left_path);
        let _ = fs::remove_file(right_path);
    }

    // The core of the strip-lines/verdict decoupling: two runs whose logs differ
    // ONLY in a numeric syscall value (a stand-in for a virtual-time timestamp or
    // a raw syscall argument) are reported MATCHED under the default stripped
    // comparison — because `strip_lines` normalizes the number away — but DIVERGED
    // under a bitwise comparison. The identical guest outputs are held constant so
    // the log comparison alone drives each verdict. A bare "verified" therefore
    // cannot say which comparison certified it; the carried `ComparisonSpec` can.
    #[test]
    fn stripped_matches_but_bitwise_diverges_on_numeric_only_log_difference() {
        let out = output(0, b"hello\n", b"");

        // Stripped: the numeric difference is normalized away -> Matched.
        let (log1, log2) = empty_logs();
        fs::write(&log1, detlog_with_value(100)).unwrap();
        fs::write(&log2, detlog_with_value(200)).unwrap();
        let stripped =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Stripped).unwrap();
        assert_eq!(stripped.verdict, Verdict::Matched);
        assert!(stripped.verified());
        assert!(stripped.comparison.strip_lines);
        assert!(!stripped.comparison.full_trace);

        // Canonical: the same inputs, but every byte compared (a decimal value,
        // untouched by address canonicalization) -> Diverged. The verdict flips
        // on the comparison mode alone, and the outcome records it.
        let (log1, log2) = empty_logs();
        let path1 = log1.to_path_buf();
        let path2 = log2.to_path_buf();
        fs::write(&path1, detlog_with_value(100)).unwrap();
        fs::write(&path2, detlog_with_value(200)).unwrap();
        let canonical =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();
        assert_eq!(canonical.verdict, Verdict::Diverged);
        assert!(!canonical.verified());
        assert_eq!(
            canonical.comparison.strictness,
            LogCompareStrictness::Canonical
        );
        assert!(!canonical.comparison.strip_lines);
        assert!(canonical.comparison.full_trace);
        // A `--verify-json` consumer reads the strictness from the report and so
        // can refuse to treat a stripped match as parity.
        let report = VerificationReport::from(&canonical);
        assert!(!report.verified);
        assert!(!report.comparison.unwrap().strip_lines);

        assert!(path1.exists(), "divergent run-1 log must be retained");
        assert!(path2.exists(), "divergent run-2 log must be retained");
        fs::remove_file(path1).unwrap();
        fs::remove_file(path2).unwrap();
    }

    #[test]
    fn verification_log_retention_matches_the_cli_contract() {
        let directory = tempfile::tempdir().unwrap();
        let output = output(0, b"hello\n", b"");

        for (left_value, right_value, keep_logs, expected, retained) in [
            (7, 7, false, Verdict::Matched, false),
            (7, 8, false, Verdict::Diverged, true),
            (7, 7, true, Verdict::Matched, true),
            (7, 8, true, Verdict::Diverged, true),
        ] {
            let (left, right) = temp_log_files_in("left", "right", Some(directory.path())).unwrap();
            fs::write(left.path(), detlog_with_value(left_value)).unwrap();
            fs::write(right.path(), detlog_with_value(right_value)).unwrap();
            let left_path = left.path().to_path_buf();
            let right_path = right.path().to_path_buf();
            let outcome = compare_two_runs(
                ComparedRun {
                    output: &output,
                    log: left.into_temp_path(),
                    label: "run 1",
                },
                ComparedRun {
                    output: &output,
                    log: right.into_temp_path(),
                    label: "run 2",
                },
                ComparisonOptions {
                    verbose: false,
                    strictness: LogCompareStrictness::Canonical,
                    compare_logs: true,
                    diagnostic_full_trace: false,
                    compare_io_buffers: false,
                    keep_logs,
                    record_envelope: RecordEnvelope::all_records_v1(),
                },
            )
            .unwrap();
            assert_eq!(outcome.verdict, expected);
            assert_eq!(left_path.exists(), retained, "run-1 retention mismatch");
            assert_eq!(right_path.exists(), retained, "run-2 retention mismatch");
            if retained {
                fs::remove_file(left_path).unwrap();
                fs::remove_file(right_path).unwrap();
            }
        }
    }

    #[test]
    fn requested_logs_survive_unsupported_syscall_scan_io_error() {
        let directory = tempfile::tempdir().unwrap();
        let output = output(0, b"hello\n", b"");
        let (left, right) = temp_log_files_in("left", "right", Some(directory.path())).unwrap();
        fs::write(left.path(), detlog_with_value(7)).unwrap();
        fs::write(right.path(), detlog_with_value(7)).unwrap();
        let left_path = left.path().to_path_buf();
        let right_path = right.path().to_path_buf();

        let error = compare_two_runs_with_unsupported_scan(
            ComparedRun {
                output: &output,
                log: left.into_temp_path(),
                label: "run 1",
            },
            ComparedRun {
                output: &output,
                log: right.into_temp_path(),
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs: true,
                record_envelope: RecordEnvelope::all_records_v1(),
            },
            |_| Err(io::Error::other("injected unsupported-syscall scan error")),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("injected unsupported-syscall scan error")
        );
        assert!(left_path.exists(), "run-1 log must survive the scan error");
        assert!(right_path.exists(), "run-2 log must survive the scan error");
        assert_eq!(
            fs::read_to_string(&left_path).unwrap(),
            detlog_with_value(7)
        );
        assert_eq!(
            fs::read_to_string(&right_path).unwrap(),
            detlog_with_value(7)
        );
        fs::remove_file(left_path).unwrap();
        fs::remove_file(right_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn requested_logs_survive_log_comparison_io_error() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let output = output(0, b"hello\n", b"");
        let (left, right) = temp_log_files_in("left", "right", Some(directory.path())).unwrap();
        fs::write(left.path(), detlog_with_value(7)).unwrap();
        fs::write(right.path(), detlog_with_value(7)).unwrap();
        let left_path = left.path().to_path_buf();
        let right_path = right.path().to_path_buf();
        fs::set_permissions(&left_path, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            fs::read(&left_path).is_err(),
            "the test must make the run-1 log unreadable"
        );

        let result = compare_two_runs(
            ComparedRun {
                output: &output,
                log: left.into_temp_path(),
                label: "run 1",
            },
            ComparedRun {
                output: &output,
                log: right.into_temp_path(),
                label: "run 2",
            },
            ComparisonOptions {
                verbose: false,
                strictness: LogCompareStrictness::Canonical,
                compare_logs: true,
                diagnostic_full_trace: false,
                compare_io_buffers: false,
                keep_logs: true,
                record_envelope: RecordEnvelope::all_records_v1(),
            },
        );

        assert!(result.is_err(), "unreadable input must fail comparison");
        assert!(
            left_path.exists(),
            "run-1 log must survive the comparison error"
        );
        assert!(
            right_path.exists(),
            "run-2 log must survive the comparison error"
        );
        fs::set_permissions(&left_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(left_path).unwrap();
        fs::remove_file(right_path).unwrap();
    }

    #[test]
    fn verification_report_carries_first_log_divergence_position() {
        let output = output(0, b"hello\n", b"");
        let (left, right) = empty_logs();
        let left_path = left.to_path_buf();
        let right_path = right.to_path_buf();
        fs::write(
            &left_path,
            "2026-08-13T01:02:03.000000Z INFO detcore::scheduler: COMMIT turn 23, dettid 2, on previously committed 4.567_890_123s\n\
             2026-08-13T01:02:03.000001Z INFO detcore: DETLOG value=1\n",
        )
        .unwrap();
        fs::write(
            &right_path,
            "2026-08-13T01:02:04.000000Z INFO detcore::scheduler: COMMIT turn 23, dettid 2, on previously committed 4.567_890_123s\n\
             2026-08-13T01:02:04.000001Z INFO detcore: DETLOG value=2\n",
        )
        .unwrap();

        let outcome = compare_with(
            &output,
            left,
            &output,
            right,
            LogCompareStrictness::Canonical,
        )
        .unwrap();
        let report = VerificationReport::from(&outcome);
        assert_eq!(report.verdict, Verdict::Diverged);
        assert_eq!(report.first_divergent_scheduler_turn, Some(23));
        assert_eq!(
            report.first_divergent_virtual_nanoseconds,
            Some(4_567_890_123)
        );

        let _ = fs::remove_file(left_path);
        let _ = fs::remove_file(right_path);
    }

    #[test]
    fn output_divergence_does_not_discard_the_log_divergence_position() {
        let left_output = output(0, b"left\n", b"");
        let right_output = output(0, b"right\n", b"");
        let (left, right) = empty_logs();
        let left_path = left.to_path_buf();
        let right_path = right.to_path_buf();
        fs::write(
            &left_path,
            "INFO detcore::scheduler: COMMIT turn 23 at time 4567890123\n\
             INFO detcore: DETLOG value=1\n",
        )
        .unwrap();
        fs::write(
            &right_path,
            "INFO detcore::scheduler: COMMIT turn 23 at time 4567890123\n\
             INFO detcore: DETLOG value=2\n",
        )
        .unwrap();

        let outcome = compare_with(
            &left_output,
            left,
            &right_output,
            right,
            LogCompareStrictness::Canonical,
        )
        .unwrap();
        let report = VerificationReport::from(&outcome);
        assert_eq!(report.verdict, Verdict::Diverged);
        assert_eq!(report.first_divergent_scheduler_turn, Some(23));
        assert_eq!(
            report.first_divergent_virtual_nanoseconds,
            Some(4_567_890_123)
        );

        let _ = fs::remove_file(left_path);
        let _ = fs::remove_file(right_path);
    }

    // The `--verify-json` payload names the comparison in the JSON itself, so a
    // downstream ratchet can gate on bitwise parity without out-of-band knowledge.
    /// FINDING 2, NEGATIVE BRACKET. A comparison that consumed ZERO log
    /// messages must never certify bitwise parity, even though every
    /// configuration field qualifies and the runs "matched": `diff_vecs`
    /// returns "no difference" for two empty selections, so configuration
    /// strictness alone would hand back a green over no work at all.
    #[test]
    fn empty_log_comparison_matches_but_is_never_parity() {
        let out = output(0, b"hello\n", b"");
        let (log1, log2) = empty_logs();
        let outcome =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();

        // The verdict itself is legitimately Matched: stdout, stderr and exit
        // status all agree. Only the PARITY claim is refused.
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(
            outcome.compared_log_messages,
            Some(ComparedLogCounts { left: 0, right: 0 })
        );
        // The spec still reports a fully-qualifying policy...
        assert!(outcome.comparison.is_bitwise_parity());
        // ...and that is exactly why the count is load-bearing.
        let report = VerificationReport::from(&outcome);
        assert!(report.verified);
        assert!(
            !report.bitwise_parity,
            "zero compared log messages must never certify bitwise parity"
        );
    }

    /// FINDING 1. Every early exit must leave an invocation-bound record: the
    /// pending stamp overwrites a previous invocation's green, so a stale
    /// `{verified:true}` can never be read as this run's result.
    /// Plant a previous invocation's GREEN verdict, the way a caller reusing one
    /// `--verify-json` path across runs leaves it.
    fn plant_previous_green(path: &Path) {
        fs::write(
            path,
            "{\"verified\":true,\"bitwise_parity\":true,\"verdict\":\"matched\"}\n",
        )
        .unwrap();
    }

    fn read_verdict(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The staging directory must always be the TARGET's directory, never the
    /// system temp directory.
    ///
    /// `Path::parent` yields an EMPTY path for a bare filename, and the earlier
    /// code treated that as "no directory" and staged in `TMPDIR`. `persist`
    /// then renames across filesystems whenever `TMPDIR` and the working
    /// directory differ -- tmpfs `/tmp` beside a btrfs checkout is the ordinary
    /// case here -- which fails `EXDEV`, so the record is never written and the
    /// PREVIOUS invocation's `{verified:true}` survives. Exactly the stale green
    /// this whole change exists to remove, reachable with
    /// `--verify-json=verdict.json`.
    #[test]
    fn a_bare_filename_stages_beside_its_target_not_in_the_system_temp_dir() {
        // The regression: a bare filename resolves to the working directory.
        assert_eq!(
            staging_directory(Path::new("verdict.json")),
            Path::new("."),
            "a bare filename must stage in the working directory; staging in \
             TMPDIR makes persist() a cross-filesystem rename"
        );
        assert_eq!(
            staging_directory(Path::new("./verdict.json")),
            Path::new(".")
        );

        // The control: a path that DOES name a directory still uses it, so the
        // fix is a corrected fallback rather than a blanket redirect to `.`.
        assert_eq!(
            staging_directory(Path::new("/tmp/run/verdict.json")),
            Path::new("/tmp/run")
        );
        assert_eq!(
            staging_directory(Path::new("sub/verdict.json")),
            Path::new("sub")
        );

        // Whatever it returns must never be empty: NamedTempFile::new_in("")
        // fails, which would turn every write into an error.
        for candidate in ["verdict.json", "./v.json", "/tmp/run/v.json", "sub/v.json"] {
            assert!(
                !staging_directory(Path::new(candidate))
                    .as_os_str()
                    .is_empty(),
                "{candidate}: staging directory must be usable"
            );
        }
    }

    /// End-to-end for the same defect: a BARE filename target, with a previous
    /// green already at it, must be replaced by the no-result stamp.
    ///
    /// Runs from a temporary working directory so the target really is a bare
    /// relative name. Under the old code this failed on any host where the
    /// working directory and `TMPDIR` are on different filesystems.
    #[test]
    fn a_bare_filename_target_is_overwritten_not_left_stale() {
        // `set_current_dir` is process-global; serialize against any other test
        // that touches it.
        static CWD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CWD.lock().unwrap_or_else(|e| e.into_inner());

        // Root the working directory in the SOURCE TREE, not in TMPDIR. A
        // plain `tempdir()` lands beside the staged file and the cross-
        // filesystem rename never happens, so the test would pass against the
        // defect -- measured: it did. On this host the checkout and /tmp are
        // distinct btrfs subvolumes (st_dev 46 vs 47), and cross-subvolume
        // rename(2) is EXDEV, which is what makes this end-to-end rather than
        // decorative.
        let dir = tempfile::Builder::new()
            .prefix("verify-json-bare-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let outcome = (|| {
            let bare = Path::new("verdict.json");
            plant_previous_green(bare);
            write_pending_verification_json(bare)?;
            Ok::<_, Error>(read_verdict(bare))
        })();

        std::env::set_current_dir(previous).unwrap();
        let now = outcome.expect("staging beside a bare filename must succeed");
        assert_eq!(now["verdict"], serde_json::json!("no_result"));
        assert_eq!(now["verified"], serde_json::json!(false));
    }

    #[test]
    fn pending_stamp_overwrites_a_previous_green_verdict() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();

        // A previous, successful invocation left a green record at this path.
        let out = output(0, b"hello\n", b"");
        let (log1, log2) = logs_with_identical_detlog();
        let good = compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();
        write_verification_json(&path, &good).unwrap();
        let previous: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(previous["verified"], serde_json::json!(true));
        assert_eq!(previous["bitwise_parity"], serde_json::json!(true));

        // A new invocation begins and will abort before reaching a verdict.
        write_pending_verification_json(&path).unwrap();

        let now: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(now["verdict"], serde_json::json!("no_result"));
        assert_eq!(now["verified"], serde_json::json!(false));
        assert_eq!(now["bitwise_parity"], serde_json::json!(false));
        assert_eq!(now["comparison"], serde_json::Value::Null);
        assert_eq!(now["compared_log_messages"], serde_json::Value::Null);
        assert!(
            now.get("dbt_counted_branches").is_none(),
            "no-result must not claim that a DBT branch-clock comparison completed"
        );
    }

    /// The positive side of FINDING 1: the pending stamp is not a dead end --
    /// a real verdict still publishes over it.
    #[test]
    fn terminal_verdict_replaces_the_pending_stamp() {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();

        write_pending_verification_json(&path).unwrap();
        let out = output(0, b"hello\n", b"");
        let (log1, log2) = logs_with_identical_detlog();
        let outcome =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();
        write_verification_json(&path, &outcome).unwrap();

        let published: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(published["verdict"], serde_json::json!("matched"));
        assert_eq!(published["verified"], serde_json::json!(true));
        assert_eq!(published["bitwise_parity"], serde_json::json!(true));
    }

    #[test]
    fn verification_report_json_carries_the_comparison() {
        let out = output(0, b"hello\n", b"");
        // NONEMPTY logs: parity may only be claimed when the comparison had
        // data. This test previously used `empty_logs()` and asserted
        // bitwise_parity = true, which codified a green over ZERO compared
        // events -- see `empty_log_comparison_matches_but_is_never_parity`.
        let (log1, log2) = logs_with_identical_detlog();
        let outcome =
            compare_with(&out, log1, &out, log2, LogCompareStrictness::Canonical).unwrap();

        let json = serde_json::to_string(&VerificationReport::from(&outcome)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["verified"], serde_json::json!(true));
        assert_eq!(parsed["verdict"], serde_json::json!("matched"));
        assert!(
            parsed.get("dbt_counted_branches").is_none(),
            "non-DBT reports must preserve the pre-field JSON shape"
        );
        assert_eq!(
            parsed["first_divergent_scheduler_turn"],
            serde_json::Value::Null
        );
        assert_eq!(
            parsed["first_divergent_virtual_nanoseconds"],
            serde_json::Value::Null
        );
        // The executed-work evidence travels with the verdict.
        assert!(parsed["compared_log_messages"]["left"].as_u64().unwrap() > 0);
        assert!(parsed["compared_log_messages"]["right"].as_u64().unwrap() > 0);
        // The single boolean a parity ratchet keys on: a matched, full-INFO,
        // unstripped comparison under a named canonical record envelope.
        assert_eq!(parsed["bitwise_parity"], serde_json::json!(true));
        assert_eq!(
            parsed["comparison"]["strictness"],
            serde_json::json!("canonical")
        );
        assert_eq!(
            parsed["comparison"]["strip_lines"],
            serde_json::json!(false)
        );
        assert_eq!(parsed["comparison"]["full_trace"], serde_json::json!(true));
        assert_eq!(parsed["comparison"]["log_scope"], serde_json::json!("info"));
        assert_eq!(
            parsed["comparison"]["record_envelope"],
            serde_json::json!("all_records_v1")
        );
        assert_eq!(
            parsed["comparison"]["compare_logs"],
            serde_json::json!(true)
        );
        // The contract's remaining clauses ("no ignore/skip filters") are carried
        // too, so a consumer can require their absence rather than assume it.
        assert_eq!(
            parsed["comparison"]["ignore_lines"],
            serde_json::json!(false)
        );
        assert_eq!(
            parsed["comparison"]["skip_commit"],
            serde_json::json!(false)
        );
        assert_eq!(
            parsed["comparison"]["skip_detlog"],
            serde_json::json!(false)
        );
    }

    // The bitwise-parity acceptance contract: a consumer must accept a `Matched`
    // as true bitwise parity ONLY under a full-INFO, unstripped, named canonical
    // record envelope and reject every weaker or opaque policy. This brackets
    // both sides: each canonical envelope fires, while stripped, output-only,
    // caller-defined, and ad hoc ignore/skip variants are refused.
    #[test]
    fn bitwise_parity_contract_accepts_only_named_canonical_envelopes() {
        // Positive: the exact qualifying comparison the `--verify-strict` path
        // produces.
        let full = ComparisonSpec::new(
            LogCompareStrictness::Canonical,
            true,
            false,
            true,
            RecordEnvelopePolicy::AllRecordsV1,
        );
        assert!(
            full.is_bitwise_parity(),
            "a full-INFO unstripped all-records comparison must qualify"
        );
        assert!(
            ComparisonSpec {
                record_envelope: RecordEnvelopePolicy::DbtEvidenceTransportV1,
                ..full
            }
            .is_bitwise_parity(),
            "the named DBT transport envelope is a disclosed canonical policy"
        );

        // Negatives: each independent weakening of the qualifying spec must be
        // refused, so no single relaxed dimension can pass as bitwise parity.
        let stripped = ComparisonSpec::new(
            LogCompareStrictness::Stripped,
            true,
            false,
            false,
            RecordEnvelopePolicy::AllRecordsV1,
        );
        assert!(
            !stripped.is_bitwise_parity(),
            "a stripped comparison normalizes away the parity-relevant data"
        );

        let output_only = ComparisonSpec {
            compare_logs: false,
            ..full
        };
        assert!(
            !output_only.is_bitwise_parity(),
            "an output-only fallback never compared the log stream"
        );

        for weakened in [
            ComparisonSpec {
                ignore_lines: true,
                ..full
            },
            ComparisonSpec {
                skip_commit: true,
                ..full
            },
            ComparisonSpec {
                skip_detlog: true,
                ..full
            },
            // full_trace off (Deterministic-mode subset) is also below bitwise.
            ComparisonSpec {
                full_trace: false,
                ..full
            },
            ComparisonSpec {
                log_scope: ComparedLogScope::Deterministic,
                ..full
            },
            ComparisonSpec {
                record_envelope: RecordEnvelopePolicy::CallerDefined,
                ..full
            },
        ] {
            assert!(
                !weakened.is_bitwise_parity(),
                "a filtered/subset comparison must not pass as bitwise parity: {weakened:?}"
            );
        }

        // A divergence is never bitwise parity even under the qualifying spec: the
        // report's boolean is the conjunction of the verdict and the contract.
        let diverged = VerificationOutcome {
            verdict: Verdict::Diverged,
            guest_status: ExitStatus::Exited(0),
            comparison: full,
            compared_log_messages: Some(ComparedLogCounts { left: 9, right: 9 }),
            dbt_counted_branches: None,
            compared_labels: ComparisonSideLabels::default(),
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
        };
        assert!(!VerificationReport::from(&diverged).bitwise_parity);
    }

    #[test]
    fn dbt_transport_only_is_disclosed_and_cannot_qualify_as_parity() {
        let output = output(0, b"hello\n", b"");
        let (left, right) = empty_logs();
        let transport = "1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized\n";
        fs::write(&left, transport).unwrap();
        fs::write(&right, transport).unwrap();

        let outcome = compare_with_envelope(
            &output,
            left,
            &output,
            right,
            LogCompareStrictness::Canonical,
            RecordEnvelope::dbt_evidence_transport_v1(),
        )
        .unwrap();
        let report = VerificationReport::from(&outcome);

        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(
            outcome.compared_log_messages,
            Some(ComparedLogCounts { left: 0, right: 0 })
        );
        assert_eq!(
            outcome.comparison.record_envelope,
            RecordEnvelopePolicy::DbtEvidenceTransportV1
        );
        assert!(!report.bitwise_parity);
    }

    #[test]
    fn dbt_real_detcore_record_matches_under_the_disclosed_envelope() {
        let output = output(0, b"hello\n", b"");
        let (left, right) = empty_logs();
        let log = "1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized\n\
2026-08-21T10:00:00.000000Z INFO detcore: DETLOG [syscall] getpid() = Ok(3)\n";
        fs::write(&left, log).unwrap();
        fs::write(&right, log).unwrap();

        let outcome = compare_with_envelope(
            &output,
            left,
            &output,
            right,
            LogCompareStrictness::Canonical,
            RecordEnvelope::dbt_evidence_transport_v1(),
        )
        .unwrap();
        let report = VerificationReport::from(&outcome);
        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(
            outcome.compared_log_messages,
            Some(ComparedLogCounts { left: 1, right: 1 })
        );
        assert!(report.bitwise_parity);
        assert_eq!(
            json["comparison"]["record_envelope"],
            "dbt_evidence_transport_v1"
        );
    }

    #[test]
    fn caller_defined_record_filter_is_disclosed_but_never_parity() {
        fn keep_everything(_record: &str) -> bool {
            true
        }

        let output = output(0, b"hello\n", b"");
        let (left, right) = logs_with_identical_detlog();
        let outcome = compare_with_envelope(
            &output,
            left,
            &output,
            right,
            LogCompareStrictness::Canonical,
            RecordEnvelope::caller_defined(keep_everything),
        )
        .unwrap();
        let report = VerificationReport::from(&outcome);
        let json = serde_json::to_value(&report).unwrap();

        assert!(outcome.verified());
        assert_eq!(
            outcome.comparison.record_envelope,
            RecordEnvelopePolicy::CallerDefined
        );
        assert!(!report.bitwise_parity);
        assert_eq!(json["comparison"]["record_envelope"], "caller_defined");
    }

    // Binds the `ComparisonSpec::new` no-filter assumption (and the
    // `compare_two_runs` debug_assert) to reality: the diff engine's default must
    // actually apply no line filters. If a future default started filtering, the
    // spec would silently misreport "no filters" — this catches that.
    #[test]
    fn default_log_diff_opts_apply_no_line_filters() {
        let default = logdiff::LogDiffOpts::default();
        assert!(default.ignore_lines.is_empty());
        assert!(!default.skip_commit);
        assert!(!default.skip_detlog);
    }
}
