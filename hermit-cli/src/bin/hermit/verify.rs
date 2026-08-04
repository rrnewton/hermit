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
    pub verbose: bool,
    pub compare_logs: bool,
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
/// verification result, while `guest_exit_code`/`guest_signal` describe the
/// guest's own termination. A consumer keys its decision on `verified` alone and
/// reads the guest status separately, instead of overloading a single exit code.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    /// True iff the two runs matched (the verdict as a boolean).
    pub verified: bool,
    /// The verdict as a stable string ("matched" / "diverged").
    pub verdict: Verdict,
    /// The guest's exit code, if it exited normally.
    pub guest_exit_code: Option<i32>,
    /// The guest's terminating signal number, if it was killed by a signal.
    pub guest_signal: Option<i32>,
}

impl From<&VerificationOutcome> for VerificationReport {
    fn from(outcome: &VerificationOutcome) -> Self {
        VerificationReport {
            verified: outcome.verified(),
            verdict: outcome.verdict,
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
        let mut diff_options = logdiff::LogDiffOpts {
            strip_lines: true,
            syscall_history: 5,
            ..Default::default()
        };
        if options.verbose {
            diff_options.comparison = logdiff::LogComparisonMode::FullTrace;
            diff_options.strip_lines = false;
            diff_options.syscall_history = 10;
        }

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

    fn compare(
        left: &Output,
        left_log: TempPath,
        right: &Output,
        right_log: TempPath,
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
                compare_logs: true,
            },
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

    #[test]
    fn identical_outputs_verify_successfully() {
        let left = output(0, b"hello\n", b"");
        let right = left.clone();
        let (log1, log2) = empty_logs();

        let outcome = compare(&left, log1, &right, log2).unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert!(outcome.verified());
        assert_eq!(outcome.guest_status, ExitStatus::Exited(0));
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
                compare_logs: false,
            },
        )
        .unwrap();
        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(outcome.guest_status, ExitStatus::Exited(0));
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
}
