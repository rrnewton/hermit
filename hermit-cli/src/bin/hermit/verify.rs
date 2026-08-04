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

pub(crate) struct ComparedRun<'a> {
    pub output: &'a Output,
    pub log: TempPath,
}

pub(crate) struct ComparisonOptions<'a> {
    pub success_message: &'a str,
    pub failure_message: &'a str,
    /// Controls only how much diff *output* is printed (a larger syscall-history
    /// window), NOT the comparison semantics. Comparison strictness is carried
    /// separately in [`Self::strictness`] so a quiet run can still be
    /// bitwise-strict — the two knobs were historically conflated behind a single
    /// `verbose` flag, which made the only bitwise comparison also the loudest.
    pub verbose: bool,
    /// How strictly the internal DETLOG event stream is compared. This is the
    /// condition the verdict rests on, and is recorded verbatim in the resulting
    /// [`VerificationOutcome`] so a consumer can tell a stripped match from a
    /// bitwise one.
    pub strictness: LogCompareStrictness,
    pub compare_logs: bool,
}

/// How strictly two runs' internal logs are compared — the condition a
/// [`Verdict`] rests on.
///
/// A bare "matched" verdict is meaningless without this: a [`Self::Stripped`]
/// comparison normalizes away numeric values, addresses, tmp paths, and — most
/// importantly — the virtual-time timestamps and syscall argument/result values
/// that bitwise parity exists to check, so a `Matched` verdict under `Stripped`
/// asserts only "matched after normalizing known-nondeterministic data", NOT
/// bitwise identity. A [`Self::Bitwise`] comparison keeps every byte and
/// compares the full captured trace, so a `Matched` verdict there is a genuine
/// bitwise-parity claim. Carrying the strictness beside the verdict is the same
/// discipline as recording the `-j` a byte count was measured at: the value is
/// uninterpretable without the condition that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogCompareStrictness {
    /// `strip_lines = true`, comparing the deterministic Detcore/scheduler
    /// message subset. Tolerant of limited nondeterminism (numbers, addresses,
    /// tmp paths, and timestamps are normalized before diffing).
    Stripped,
    /// `strip_lines = false`, comparing the full captured trace. Every byte of
    /// every log message must match, including virtual-time timestamps and raw
    /// syscall argument/result values.
    Bitwise,
}

/// The exact comparison that produced a [`Verdict`], carried beside it so a bare
/// "verified" can always say *which* comparison certified it.
///
/// The high-level [`Self::strictness`] and the concrete flags it expands to are
/// both recorded: a JSON consumer keying a bitwise-parity ratchet on the verdict
/// can require `strip_lines == false` and `full_trace == true` directly, rather
/// than having to know how a strictness label maps onto the diff engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ComparisonSpec {
    /// The strictness label the comparison ran under.
    pub strictness: LogCompareStrictness,
    /// Whether the internal DETLOG event stream was compared at all. When
    /// `false` (e.g. KVM concurrent mode) only stdout/stderr/exit status were
    /// compared and the strictness fields describe a log comparison that did not
    /// run — a consumer must not read such a verdict as bitwise log parity.
    pub compare_logs: bool,
    /// Concrete: were numeric values, addresses, tmp paths, and timestamps
    /// normalized away before diffing?
    pub strip_lines: bool,
    /// Concrete: was the full captured trace compared (vs. the deterministic
    /// subset)?
    pub full_trace: bool,
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
    pub fn new(strictness: LogCompareStrictness, compare_logs: bool) -> Self {
        let (strip_lines, full_trace) = match strictness {
            LogCompareStrictness::Stripped => (true, false),
            LogCompareStrictness::Bitwise => (false, true),
        };
        ComparisonSpec {
            strictness,
            compare_logs,
            strip_lines,
            full_trace,
            // The `--verify` code paths never expose the diff engine's line
            // filters, so the comparison they produce applies none. These are
            // recorded (not merely assumed) so a bitwise consumer can *require*
            // their absence rather than trust that no CLI surface enables them;
            // `diff_options_apply_no_line_filters` binds these values to the
            // actual `LogDiffOpts` the engine sees.
            ignore_lines: false,
            skip_commit: false,
            skip_detlog: false,
        }
    }

    /// The `LogComparisonMode` this spec selects for the diff engine.
    fn log_comparison_mode(&self) -> LogComparisonMode {
        if self.full_trace {
            LogComparisonMode::FullTrace
        } else {
            LogComparisonMode::Deterministic
        }
    }

    /// Does this comparison satisfy the bitwise INFO-parity contract a
    /// determinism / record-replay ratchet must require before it may read a
    /// `Matched` verdict as *true bitwise parity*? A bare `verified` is not
    /// enough: `verified` can rest on a stripped compare, a filtered subset, or
    /// an output-only fallback, all of which normalize or omit exactly the data
    /// (virtual-time timestamps, raw syscall argument/result values, whole event
    /// classes) that bitwise parity exists to check.
    ///
    /// All clauses must hold:
    /// - the full INFO event stream was compared ([`Self::full_trace`]), which is
    ///   what carries exact virtual timestamps and syscall argument/result values;
    /// - no line-stripping normalization ran (`!strip_lines`);
    /// - no ignore/skip filter dropped any line or event class
    ///   (`!ignore_lines && !skip_commit && !skip_detlog`);
    /// - the internal log stream was actually compared, not skipped for an
    ///   output-only fallback ([`Self::compare_logs`]).
    ///
    /// A consumer asking for bitwise parity must reject `Matched` under every
    /// weaker comparison; this predicate is that single acceptance rule.
    pub fn is_bitwise_parity(&self) -> bool {
        self.compare_logs
            && self.full_trace
            && !self.strip_lines
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
    /// status, and — unless disabled — the internal DETLOG event stream).
    Matched,
    /// The two runs diverged; verification failed.
    Diverged,
}

/// The full outcome of comparing two runs: the verification [`Verdict`] plus the
/// guest exit status, so a caller never has to infer either one from the other.
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    pub verdict: Verdict,
    /// Exit status of the second (replay / repeat) run, propagated verbatim.
    pub guest_status: ExitStatus,
    /// The exact comparison that produced [`Self::verdict`], carried so a
    /// consumer never has to assume which comparison a "matched" rests on.
    pub comparison: ComparisonSpec,
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
            Verdict::Diverged => Err(Error::msg(
                "Mismatch between run 1 and run 2 outputs (logs retained).",
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
/// `verified` under a stripped comparison, a filtered subset, or an output-only
/// fallback is not a bitwise-parity claim. Such a consumer reads
/// [`Self::bitwise_parity`] (or checks the `comparison` fields directly), which
/// is `true` only when the verdict rests on a full-INFO, unfiltered, unstripped
/// log comparison.
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
    /// The comparison that produced the verdict. Without this a bitwise-parity
    /// consumer cannot distinguish a stripped match from a bitwise one.
    pub comparison: ComparisonSpec,
    /// The guest's exit code, if it exited normally.
    pub guest_exit_code: Option<i32>,
    /// The guest's terminating signal number, if it was killed by a signal.
    pub guest_signal: Option<i32>,
}

impl From<&VerificationOutcome> for VerificationReport {
    fn from(outcome: &VerificationOutcome) -> Self {
        VerificationReport {
            verified: outcome.verified(),
            // Bitwise parity is a conjunction: the runs matched AND the
            // comparison was strict enough for the match to *mean* bitwise
            // identity. A `Diverged` verdict is never bitwise parity.
            bitwise_parity: outcome.verified() && outcome.comparison.is_bitwise_parity(),
            verdict: outcome.verdict,
            comparison: outcome.comparison,
            guest_exit_code: outcome.guest_status.code(),
            guest_signal: outcome.guest_status.signal(),
        }
    }
}

/// Write the verification report as a single JSON line to `path`.
///
/// This is the exit-code-independent verdict channel: the record it writes is
/// true or false based on whether verification matched, regardless of what the
/// guest exited with.
pub fn write_verification_json(path: &Path, outcome: &VerificationOutcome) -> Result<(), Error> {
    let report = VerificationReport::from(outcome);
    let json = serde_json::to_string(&report)?;
    std::fs::write(path, format!("{json}\n"))
        .with_context(|| format!("writing verification verdict to {}", path.display()))?;
    Ok(())
}

/// Reject an explicit log level that would suppress the events verification compares.
///
/// With no explicit level, the verification paths select `DEBUG` internally.
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

pub fn temp_log_files(name1: &str, name2: &str) -> io::Result<(NamedTempFile, NamedTempFile)> {
    let file1 = tempfile::Builder::new()
        .prefix(&format!("{}_log_", name1))
        .rand_bytes(5)
        .tempfile()?;
    let file2 = tempfile::Builder::new()
        .prefix(&format!("{}_log_", name2))
        .rand_bytes(5)
        .tempfile()?;

    Ok((file1, file2))
}

pub fn setup_double_run(
    global: &GlobalOpts,
    name1: &str,
    name2: &str,
) -> ((GlobalOpts, NamedTempFile), (GlobalOpts, NamedTempFile)) {
    let (file1, file2) = temp_log_files(name1, name2).unwrap();

    let path1 = PathBuf::from(file1.path());
    let path2 = PathBuf::from(file2.path());

    // Override global settings.  Unfortunately we lose the log output to the
    // screen.
    let mut global = global.clone();
    global.log_file = Some(path1);
    global.log = Some(LevelFilter::DEBUG);

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

pub fn compare_two_runs(
    first: ComparedRun<'_>,
    second: ComparedRun<'_>,
    options: ComparisonOptions<'_>,
) -> Result<VerificationOutcome, Error> {
    let ComparedRun {
        output: out1,
        log: log1,
    } = first;
    let ComparedRun {
        output: out2,
        log: log2,
    } = second;
    let mut failed = false;

    // Resolve the strictness label to concrete diff flags once, and carry the
    // resulting spec through to the verdict so the returned outcome records
    // exactly which comparison certified it.
    let spec = ComparisonSpec::new(options.strictness, options.compare_logs);

    if out1.stdout != out2.stdout {
        failed = true;
        eprintln!("Mismatch in stdout between run 1 and run 2:");
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
        eprintln!("Mismatch in stderr between run 1 and run 2:");
        let str1 = String::from_utf8_lossy(&out1.stderr);
        let str2 = String::from_utf8_lossy(&out2.stderr);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    if options.compare_logs {
        eprintln!(
            ":: {} {} and {}",
            "Comparing logs...".yellow().bold(),
            log1.display(),
            log2.display()
        );
        // The comparison semantics come from `spec` (strip_lines + mode); only
        // the printed syscall-history depth still tracks `verbose`. Historically
        // both were flipped together, so the sole bitwise comparison was also the
        // loudest — decoupling them lets a quiet run be bitwise-strict.
        let diff_options = logdiff::LogDiffOpts {
            strip_lines: spec.strip_lines,
            comparison: spec.log_comparison_mode(),
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
            diff_options.ignore_lines.is_empty() == !spec.ignore_lines,
            "ComparisonSpec.ignore_lines must match the diff engine's ignore_lines"
        );

        if logdiff::log_diff(log1.as_ref(), log2.as_ref(), &diff_options) {
            failed = true;
            eprintln!(":: {}", "Log differences found between runs.".red().bold());
            eprintln!(
                ":: {}: {} {}",
                "Respective Logs retained for further inspection".red(),
                log1.display(),
                log2.display()
            );
        }
    } else {
        eprintln!(
            ":: KVM concurrent mode: comparing guest output and exit status; internal syscall trace order is not deterministic"
        );
    }

    if out1.status != out2.status {
        failed = true;
        eprintln!(
            "Mismatch in exit status between run 1 and run 2: {}",
            Comparison::new(&out1.status, &out2.status)
        );
    }

    if failed {
        eprintln!(":: {}", options.failure_message.red().bold());
        let _ = log1.keep()?;
        let _ = log2.keep()?;
        // Divergence is a verification *verdict*, not an I/O error: return it as
        // a value carrying the guest exit status. `Err` stays reserved for
        // genuine failures (e.g. the `.keep()?` above). Callers that want the
        // historical "divergence -> nonzero process exit" behavior use
        // `VerificationOutcome::into_exit_status`.
        Ok(VerificationOutcome {
            verdict: Verdict::Diverged,
            guest_status: out2.status,
            comparison: spec,
        })
    } else {
        // Allow the NamedTempFiles to be deleted in this case:
        let mut unsupported = unsupported_syscalls_from_log(log1.as_ref())?;
        unsupported.extend(unsupported_syscalls_from_log(log2.as_ref())?);
        if let Some(message) = detcore::format_unsupported_syscall_warning(&unsupported) {
            eprintln!("WARNING: {message}");
        }
        eprintln!(":: {}", options.success_message.green().bold());
        Ok(VerificationOutcome {
            verdict: Verdict::Matched,
            guest_status: out2.status,
            comparison: spec,
        })
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

    fn global_with_log(log: Option<LevelFilter>) -> GlobalOpts {
        GlobalOpts {
            log,
            log_file: None,
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

    fn compare_with(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
        strictness: LogCompareStrictness,
    ) -> Result<VerificationOutcome, Error> {
        compare_two_runs(
            ComparedRun {
                output: left,
                log: left_log,
            },
            ComparedRun {
                output: right,
                log: right_log,
            },
            ComparisonOptions {
                success_message: "verified",
                failure_message: "failed",
                verbose: false,
                strictness,
                compare_logs: true,
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
        format!("DEBUG detcore: [dtid 2] DETLOG [syscall] write(fd=1, count={value})\n")
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
        assert_eq!(report.comparison.strictness, LogCompareStrictness::Stripped);
        // Collapsing to the legacy exit convention still propagates the guest
        // code; the verdict channel above is what a caller keys on.
        assert_eq!(outcome.into_exit_status().unwrap(), ExitStatus::Exited(3));
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
            },
            ComparedRun {
                output: &right,
                log: right_log,
            },
            ComparisonOptions {
                success_message: "verified",
                failure_message: "failed",
                verbose: false,
                strictness: LogCompareStrictness::Stripped,
                compare_logs: false,
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
        let stripped = ComparisonSpec::new(LogCompareStrictness::Stripped, true);
        assert!(stripped.strip_lines);
        assert!(!stripped.full_trace);
        assert_eq!(
            stripped.log_comparison_mode(),
            LogComparisonMode::Deterministic
        );

        let bitwise = ComparisonSpec::new(LogCompareStrictness::Bitwise, true);
        assert!(!bitwise.strip_lines);
        assert!(bitwise.full_trace);
        assert_eq!(bitwise.log_comparison_mode(), LogComparisonMode::FullTrace);
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

        // Bitwise: the same inputs, but every byte compared -> Diverged. The
        // verdict flips on the comparison mode alone, and the outcome records it.
        let (log1, log2) = empty_logs();
        let path1 = log1.to_path_buf();
        let path2 = log2.to_path_buf();
        fs::write(&path1, detlog_with_value(100)).unwrap();
        fs::write(&path2, detlog_with_value(200)).unwrap();
        let bitwise = compare_with(&out, log1, &out, log2, LogCompareStrictness::Bitwise).unwrap();
        assert_eq!(bitwise.verdict, Verdict::Diverged);
        assert!(!bitwise.verified());
        assert_eq!(bitwise.comparison.strictness, LogCompareStrictness::Bitwise);
        assert!(!bitwise.comparison.strip_lines);
        assert!(bitwise.comparison.full_trace);
        // A `--verify-json` consumer reads the strictness from the report and so
        // can refuse to treat a stripped match as bitwise parity.
        let report = VerificationReport::from(&bitwise);
        assert!(!report.verified);
        assert!(!report.comparison.strip_lines);

        // Diverged bitwise runs retain their logs (`.keep()`); clean them up.
        let _ = fs::remove_file(path1);
        let _ = fs::remove_file(path2);
    }

    // The `--verify-json` payload names the comparison in the JSON itself, so a
    // downstream ratchet can gate on bitwise parity without out-of-band knowledge.
    #[test]
    fn verification_report_json_carries_the_comparison() {
        let out = output(0, b"hello\n", b"");
        let (log1, log2) = empty_logs();
        let outcome = compare_with(&out, log1, &out, log2, LogCompareStrictness::Bitwise).unwrap();

        let json = serde_json::to_string(&VerificationReport::from(&outcome)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["verified"], serde_json::json!(true));
        assert_eq!(parsed["verdict"], serde_json::json!("matched"));
        // The single boolean a parity ratchet keys on: a matched, full-INFO,
        // unstripped, unfiltered comparison.
        assert_eq!(parsed["bitwise_parity"], serde_json::json!(true));
        assert_eq!(
            parsed["comparison"]["strictness"],
            serde_json::json!("bitwise")
        );
        assert_eq!(
            parsed["comparison"]["strip_lines"],
            serde_json::json!(false)
        );
        assert_eq!(parsed["comparison"]["full_trace"], serde_json::json!(true));
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
    // as true bitwise parity ONLY under a full-INFO, unstripped, unfiltered,
    // log-comparing spec — and reject it under every weaker one. This brackets
    // both sides: the one qualifying spec fires, and each single-clause weakening
    // (stripped, output-only, and each ignore/skip filter) is refused. Without
    // this, three different facts (stripped compare, output-only fallback,
    // filtered subset) would all masquerade as the same `verified == true`.
    #[test]
    fn bitwise_parity_contract_accepts_only_full_unfiltered_comparison() {
        // Positive: the exact qualifying comparison the `--verify-strict` path
        // produces.
        let full = ComparisonSpec::new(LogCompareStrictness::Bitwise, true);
        assert!(
            full.is_bitwise_parity(),
            "a full-INFO unstripped unfiltered comparison must qualify"
        );

        // Negatives: each independent weakening of the qualifying spec must be
        // refused, so no single relaxed dimension can pass as bitwise parity.
        let stripped = ComparisonSpec::new(LogCompareStrictness::Stripped, true);
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
        };
        assert!(!VerificationReport::from(&diverged).bitwise_parity);
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
