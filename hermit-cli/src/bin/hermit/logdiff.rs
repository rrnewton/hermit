/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use clap::Parser;
use detcore::logdiff;
use reverie::process::ExitStatus;
use serde::Serialize;
use tempfile::NamedTempFile;

use super::global_opts::GlobalOpts;

/// Command-line options for the "logdiff" subcommand.
#[derive(Debug, Parser)]
pub struct LogDiffCLIOpts {
    /// A log to print canonically, or the first of two logs to compare.
    file_a: PathBuf,
    /// Optional second log. With one input, print the canonical INFO stream
    /// used by strict verification; with two inputs, compare them.
    file_b: Option<PathBuf>,

    /// Write one machine-readable JSON line for a two-log comparison.
    #[clap(long, value_name = "FILE")]
    json: Option<PathBuf>,

    #[clap(flatten)]
    pub more: logdiff::LogDiffOpts,
}

impl LogDiffCLIOpts {
    /// Construct LogDiffOpts to compare two files.
    pub fn new(a: &Path, b: &Path) -> Self {
        Self {
            file_a: PathBuf::from(a),
            file_b: Some(PathBuf::from(b)),
            json: None,
            more: Default::default(),
        }
    }

    /// Print one log canonically or compare two logs.
    pub fn main(&self, _global: &GlobalOpts) -> ExitStatus {
        let Some(file_b) = &self.file_b else {
            if self.json.is_some() {
                eprintln!("hermit log-diff: --json requires two input logs");
                return ExitStatus::Exited(2);
            }
            return match logdiff::write_canonical_info(&self.file_a, &mut std::io::stdout().lock())
            {
                Ok(0) => {
                    eprintln!(
                        "hermit log-diff: {} contains no comparable INFO messages",
                        self.file_a.display()
                    );
                    ExitStatus::Exited(2)
                }
                Ok(_) => ExitStatus::Exited(0),
                Err(error) => {
                    eprintln!(
                        "hermit log-diff: could not canonicalize {}: {error}",
                        self.file_a.display()
                    );
                    ExitStatus::Exited(2)
                }
            };
        };

        let mut options = self.more.clone();
        options.comparison = logdiff::LogComparisonMode::Info;
        options.canonicalize_addresses = true;

        if let Some(path) = &self.json
            && let Err(error) = write_json(path, &pending_json_report(&options))
        {
            eprintln!(
                "hermit log-diff: could not initialize JSON report at {}: {error}",
                path.display()
            );
            return ExitStatus::Exited(2);
        }

        let summary = match logdiff::try_log_diff_detailed(&self.file_a, file_b, &options) {
            Ok(summary) => summary,
            Err(error) => {
                eprintln!(
                    "hermit log-diff: could not compare {} and {}: {error}",
                    self.file_a.display(),
                    file_b.display()
                );
                return ExitStatus::Exited(2);
            }
        };
        if let Some(path) = &self.json
            && let Err(error) = write_json(path, &json_report(&summary, &options))
        {
            eprintln!(
                "hermit log-diff: could not write JSON report to {}: {error}",
                path.display()
            );
            return ExitStatus::Exited(2);
        }
        if summary.diff_found {
            ExitStatus::Exited(1)
        } else if summary.matched_with_evidence() {
            ExitStatus::Exited(0)
        } else {
            eprintln!("hermit log-diff: no comparable messages; refusing to report a match");
            ExitStatus::Exited(2)
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonVerdict {
    NoResult,
    Matched,
    Diverged,
    NoComparableMessages,
}

#[derive(Serialize)]
struct JsonMessageCounts {
    left: usize,
    right: usize,
}

#[derive(Serialize)]
struct JsonComparison {
    stream: &'static str,
    canonicalize_host_addresses: bool,
}

#[derive(Serialize)]
struct JsonReport {
    verdict: JsonVerdict,
    selected_messages: JsonMessageCounts,
    comparison: JsonComparison,
    first_divergent_scheduler_turn: Option<u64>,
    first_divergent_virtual_nanoseconds: Option<u64>,
}

fn json_report(summary: &logdiff::LogDiffSummary, options: &logdiff::LogDiffOpts) -> JsonReport {
    let verdict = if summary.diff_found {
        JsonVerdict::Diverged
    } else if summary.compared_left == 0 && summary.compared_right == 0 {
        JsonVerdict::NoComparableMessages
    } else {
        JsonVerdict::Matched
    };
    let stream = match options.comparison {
        logdiff::LogComparisonMode::Info => "info",
        logdiff::LogComparisonMode::FullTrace => "full_trace",
    };
    JsonReport {
        verdict,
        selected_messages: JsonMessageCounts {
            left: summary.compared_left,
            right: summary.compared_right,
        },
        comparison: JsonComparison {
            stream,
            canonicalize_host_addresses: options.canonicalize_addresses,
        },
        first_divergent_scheduler_turn: summary.first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds: summary.first_divergent_virtual_nanoseconds,
    }
}

fn pending_json_report(options: &logdiff::LogDiffOpts) -> JsonReport {
    let summary = logdiff::LogDiffSummary {
        diff_found: false,
        compared_left: 0,
        compared_right: 0,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
    };
    let mut report = json_report(&summary, options);
    report.verdict = JsonVerdict::NoResult;
    report
}

fn write_json(path: &Path, report: &JsonReport) -> std::io::Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, report).map_err(std::io::Error::other)?;
    temporary.write_all(b"\n")?;
    temporary.flush()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn one_log_and_two_log_json_forms_parse() {
        let one = LogDiffCLIOpts::try_parse_from(["log-diff", "run.log"]).unwrap();
        assert_eq!(one.file_b, None);

        let two = LogDiffCLIOpts::try_parse_from([
            "log-diff",
            "left.log",
            "right.log",
            "--json",
            "diff.json",
        ])
        .unwrap();
        assert_eq!(two.file_b, Some(PathBuf::from("right.log")));
        assert_eq!(two.json, Some(PathBuf::from("diff.json")));
    }

    #[test]
    fn json_report_distinguishes_divergence_match_and_no_messages() {
        let options = logdiff::LogDiffOpts::default();
        let summary = logdiff::LogDiffSummary {
            diff_found: true,
            compared_left: 8,
            compared_right: 8,
            first_divergent_scheduler_turn: Some(17),
            first_divergent_virtual_nanoseconds: Some(123),
        };
        let value = serde_json::to_value(json_report(&summary, &options)).unwrap();
        assert_eq!(value["verdict"], "diverged");
        assert_eq!(value["selected_messages"]["left"], 8);
        assert_eq!(value["comparison"]["stream"], "info");
        assert_eq!(value["first_divergent_scheduler_turn"], 17);
        assert_eq!(value["first_divergent_virtual_nanoseconds"], 123);

        let matched = logdiff::LogDiffSummary {
            diff_found: false,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            ..summary
        };
        assert_eq!(
            serde_json::to_value(json_report(&matched, &options)).unwrap()["verdict"],
            "matched"
        );

        let empty = logdiff::LogDiffSummary {
            compared_left: 0,
            compared_right: 0,
            ..matched
        };
        assert_eq!(
            serde_json::to_value(json_report(&empty, &options)).unwrap()["verdict"],
            "no_comparable_messages"
        );
    }

    #[test]
    fn deleted_comparison_relaxations_are_rejected_and_json_starts_as_no_result() {
        for removed in [
            "--unsafe-strip-lines",
            "--strip-lines",
            "--ignore-lines=difference",
            "--skip-commit",
            "--skip-detlog",
            "--git-diff",
            "--include-detlogs=syscall",
            "--canonical-info",
        ] {
            assert!(
                LogDiffCLIOpts::try_parse_from(["log-diff", "left.log", "right.log", removed])
                    .is_err(),
                "deleted comparison option still parsed: {removed}"
            );
        }

        let options = logdiff::LogDiffOpts::default();
        let report = pending_json_report(&options);
        assert_eq!(
            serde_json::to_value(report).unwrap()["verdict"],
            "no_result"
        );
    }

    #[test]
    fn json_report_is_one_line_and_replaces_the_previous_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("comparison.json");
        let options = logdiff::LogDiffOpts::default();

        write_json(&path, &pending_json_report(&options)).unwrap();
        let pending = std::fs::read_to_string(&path).unwrap();
        assert_eq!(pending.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pending).unwrap()["verdict"],
            "no_result"
        );

        let summary = logdiff::LogDiffSummary {
            diff_found: false,
            compared_left: 3,
            compared_right: 3,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
        };
        write_json(&path, &json_report(&summary, &options)).unwrap();
        let terminal = std::fs::read_to_string(&path).unwrap();
        assert_eq!(terminal.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&terminal).unwrap()["verdict"],
            "matched"
        );
    }
}
