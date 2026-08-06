#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Turn a silently-cancelled weekly "super" validation run into a LOUD alert.
//!
//! GitHub marks a job killed by `timeout-minutes` (or otherwise cancelled) with
//! conclusion `cancelled` — a grey, silent outcome that is indistinguishable
//! from a run that never had to do anything. A cancelled stress run is a
//! NO-RESULT, not a pass: absence must be loud, not silent. This helper is run
//! by a downstream workflow job that `needs` the `super` job with
//! `if: always()`, so it executes when `super` itself was cancelled while the
//! workflow run remains alive. A whole-run cancellation also cancels this
//! downstream job and therefore requires external observation. The helper
//! inspects the `super` job's result and fails (exit 1, red job) with an
//! `::error::` annotation whenever that result is anything other than a
//! genuine completion (`success`, `failure`) or a legitimate skip (`skipped`).
//!
//! Run locally to reproduce either branch:
//!
//! ```text
//! ./scripts/super_cancel_alert.rs --result cancelled   # exit 1, alert fires
//! ./scripts/super_cancel_alert.rs --result success     # exit 0, silent
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::env;
use std::process::ExitCode;

#[derive(Debug)]
struct Options {
    result: String,
    run_url: Option<String>,
    timeout_minutes: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
struct Decision {
    alert: bool,
    reason: &'static str,
}

fn usage() -> &'static str {
    "Usage: super_cancel_alert.rs --result RESULT [OPTIONS]\n\
\n\
Evaluate the weekly `super` validation job's result and alert LOUDLY when a\n\
cancelled/no-result outcome would otherwise be silent.\n\
\n\
Options:\n\
  --result RESULT        Result of the `super` job (needs.super.result):\n\
                           success | failure | cancelled | skipped | <other>\n\
  --run-url URL          Workflow-run URL to cite in the alert (optional)\n\
  --timeout-minutes N    Job timeout, cited in the alert hint (optional)\n\
  -h, --help             Show this help\n\
\n\
Exit status: 0 no alert (genuine completion or legitimate skip),\n\
             1 ALERT (cancelled / timed-out / unknown no-result),\n\
             2 operational error (bad arguments)."
}

fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let mut result: Option<String> = None;
    let mut run_url: Option<String> = None;
    let mut timeout_minutes: Option<u64> = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| {
            iter.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--result" => result = Some(value("--result")?),
            "--run-url" => run_url = Some(value("--run-url")?),
            "--timeout-minutes" => {
                timeout_minutes = Some(
                    value("--timeout-minutes")?
                        .parse()
                        .map_err(|_| "--timeout-minutes must be an integer".to_owned())?,
                );
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }

    let result = result.ok_or_else(|| "--result is required".to_owned())?;
    Ok(Some(Options {
        result,
        run_url,
        timeout_minutes,
    }))
}

/// Decide whether the `super` job's result must raise an alert.
///
/// A cancelled/timed-out run — or any unrecognised, empty, no-result outcome —
/// is silent by default and MUST alert. A genuine `success` or `failure` is
/// already surfaced by the `super` job itself (green/red), and a `skipped`
/// job legitimately did not run, so none of those alert here.
fn decide(result: &str) -> Decision {
    match result.trim() {
        "success" => Decision {
            alert: false,
            reason: "super validation completed successfully",
        },
        "failure" => Decision {
            alert: false,
            reason: "super validation ran and failed (already surfaced as a red job)",
        },
        "skipped" => Decision {
            alert: false,
            reason: "super validation was legitimately skipped (job condition false)",
        },
        "cancelled" => Decision {
            alert: true,
            reason: "super validation was CANCELLED — almost certainly the timeout-minutes wall; this is a NO-RESULT, not a pass",
        },
        _ => Decision {
            alert: true,
            reason: "super validation reported no recognised result (no-result / unknown outcome)",
        },
    }
}

fn annotation(kind: &str, message: &str) {
    if env::var_os("GITHUB_ACTIONS").is_some() {
        let escaped = message
            .replace('%', "%25")
            .replace('\r', "%0D")
            .replace('\n', "%0A");
        println!("::{kind} title=Weekly super validation::{escaped}");
    }
}

fn run(options: Options) -> bool {
    let decision = decide(&options.result);
    let context = {
        let mut parts = Vec::new();
        if let Some(url) = &options.run_url {
            parts.push(format!("run: {url}"));
        }
        if let Some(minutes) = options.timeout_minutes {
            parts.push(format!("timeout-minutes: {minutes}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        }
    };

    println!(
        "Weekly super validation result: {:?} -> {}",
        options.result,
        if decision.alert { "ALERT" } else { "ok" }
    );
    println!("{}{}", decision.reason, context);

    if decision.alert {
        annotation("error", &format!("{}{}", decision.reason, context));
    }
    decision.alert
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let options = match parse_options(env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("super_cancel_alert.rs: {error}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };
    if run(options) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genuine_completion_does_not_alert() {
        assert!(!decide("success").alert);
        assert!(!decide("failure").alert);
    }

    #[test]
    fn legitimate_skip_does_not_alert() {
        assert!(!decide("skipped").alert);
    }

    #[test]
    fn cancelled_run_alerts() {
        assert!(decide("cancelled").alert);
    }

    #[test]
    fn unknown_or_empty_result_alerts() {
        assert!(decide("").alert);
        assert!(decide("timed_out").alert);
        assert!(decide("neutral").alert);
    }

    #[test]
    fn result_is_trimmed_before_matching() {
        assert!(!decide("  success\n").alert);
        assert!(decide("  cancelled ").alert);
    }

    #[test]
    fn requires_result_argument() {
        let parsed = parse_options(Vec::<String>::new());
        assert!(parsed.is_err());
    }

    #[test]
    fn parses_optional_context_flags() {
        let options = parse_options(
            [
                "--result",
                "cancelled",
                "--run-url",
                "https://example/run/1",
                "--timeout-minutes",
                "360",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.result, "cancelled");
        assert_eq!(options.run_url.as_deref(), Some("https://example/run/1"));
        assert_eq!(options.timeout_minutes, Some(360));
    }
}
