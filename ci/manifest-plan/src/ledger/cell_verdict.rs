//! Shared interpretation of one producer-written raw cell's ordinary comparison.
//! This is the unchanged validator policy, reused by public evidence derivation.

use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use super::CellVerdict;
use super::ComparedLogCounts;
use super::ComparisonSpec;
use super::ComparisonTier;
use super::RequiredNullable;
use crate::canonical_verdict::InfrastructureError;
use crate::canonical_verdict::Verdict as VerificationVerdict;
use crate::canonical_verdict::VerificationReport;

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("per-cell result has no nonempty {key}"))
}

fn preserved_reason<'a>(row: &'a Value, attempt: Option<&'a Value>) -> Option<&'a str> {
    attempt
        .and_then(|value| value.get("reason"))
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .or_else(|| {
            row.get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
        })
}

fn canonical_report(
    value: Value,
    bytes: &[u8],
    expected_virtualize_time: bool,
) -> Result<Option<(VerificationReport, ComparisonSpec, ComparedLogCounts)>, String> {
    // `VerificationReport` owns the complete current top-level report. The
    // ledger types additionally deny unknown comparison/count fields, which
    // preserves schema 7's exact shape without a second hard-coded key list.
    let report = VerificationReport::from_current_json_slice(bytes)?;
    if report.verdict == VerificationVerdict::InfrastructureError {
        return Err(match report.infrastructure_error.as_ref() {
            Some(InfrastructureError::SkidOvershoot { count }) => {
                format!("recorded infrastructure_error: {count} HERMIT_SKID_OVERSHOOT report(s)")
            }
            None => unreachable!("typed report parser requires an infrastructure error"),
        });
    }
    let comparison = value
        .get("comparison")
        .cloned()
        .ok_or("incomplete cell comparison: missing `comparison`")?;
    let comparison = serde_json::from_value::<ComparisonSpec>(comparison)
        .map_err(|error| format!("incomplete cell comparison: {error}"))?;
    let compared_log_messages = serde_json::from_value::<RequiredNullable<ComparedLogCounts>>(
        value
            .get("compared_log_messages")
            .cloned()
            .ok_or("incomplete cell comparison: missing `compared_log_messages`")?,
    )
    .map_err(|error| format!("incomplete cell comparison counts: {error}"))?;
    if !comparison.is_canonical_bitwise_info_v1_for_time_policy(
        expected_virtualize_time,
        &compared_log_messages,
    ) {
        return Ok(None);
    }
    let RequiredNullable::Value(compared_log_messages) = compared_log_messages else {
        return Ok(None);
    };
    Ok(Some((report, comparison, compared_log_messages)))
}

pub fn cell_verdict_from_source(row: &Value) -> Result<CellVerdict, String> {
    let mode = string(row, "mode")?;
    if mode == "naked" || mode == "custom" {
        return Ok(CellVerdict::PerformsNoComparisonByDesign {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: format!("{mode} mode does not perform canonical two-run comparison"),
        });
    }
    // Verify and chaos compare independent executions and require virtual
    // time. Replay compares one recording with its replay and deliberately
    // leaves time real. Bind the receipt to that producer policy: accepting
    // either boolean would weaken the comparison requirement.
    let expected_virtualize_time = match mode {
        "replay" => false,
        "verify" | "chaos" => true,
        _ => {
            return Err(format!(
                "unsupported cell mode `{mode}` has no declared canonical comparison policy"
            ));
        }
    };
    let Some(attempts) = row.get("attempts").and_then(Value::as_array) else {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: preserved_reason(row, None)
                .unwrap_or("cell emitted no typed attempts")
                .into(),
        });
    };
    let mut reports = Vec::new();
    let mut unavailable_reason = None;
    for (index, attempt) in attempts.iter().enumerate() {
        let preserved_reason = preserved_reason(row, Some(attempt)).map(str::to_owned);
        let Some(raw) = attempt.get("verification_report").and_then(Value::as_str) else {
            unavailable_reason = Some(preserved_reason.clone().unwrap_or_else(|| {
                format!("attempt {} emitted no typed verification report", index + 1)
            }));
            continue;
        };
        let expected_sha = attempt
            .get("verification_report_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("attempt {} omitted verification_report_sha256", index + 1))?;
        if hex_digest(raw.as_bytes()) != expected_sha {
            return Err(format!(
                "attempt {} verification_report_sha256 mismatch",
                index + 1
            ));
        }
        let value = serde_json::from_str::<Value>(raw).map_err(|error| {
            format!(
                "attempt {} verification report is malformed: {error}",
                index + 1
            )
        })?;
        match canonical_report(value, raw.as_bytes(), expected_virtualize_time) {
            Ok(Some(report)) => reports.push(report),
            Ok(None) => {
                unavailable_reason = Some(preserved_reason.clone().unwrap_or_else(|| {
                    format!(
                        "attempt {} did not compare canonical nonzero INFO evidence",
                        index + 1
                    )
                }));
            }
            Err(error) => {
                unavailable_reason = Some(
                    preserved_reason.unwrap_or_else(|| format!("attempt {} {error}", index + 1)),
                )
            }
        }
    }
    let classify = |(report, _, _): &(VerificationReport, ComparisonSpec, ComparedLogCounts)| {
        let matched = report.verified
            && report.verdict == VerificationVerdict::Matched
            && report.bitwise_parity;
        let diverged = report.verdict == VerificationVerdict::Diverged && !report.bitwise_parity;
        (matched, diverged)
    };
    // A genuine canonical divergence is sticky across sibling attempts. Missing
    // or weaker evidence may prevent a clean leg, but it must never erase a red
    // leg merely because it was observed before or after that divergence.
    if let Some((_, comparison, compared_log_messages)) =
        reports.iter().find(|report| classify(report).1)
    {
        return Ok(CellVerdict::ComparedAndDiverged {
            comparison_tier: ComparisonTier::CanonicalBitwise,
            comparison: comparison.clone(),
            bitwise_parity: false,
            compared_log_messages: RequiredNullable::Value(compared_log_messages.clone()),
        });
    }
    if reports.is_empty() || unavailable_reason.is_some() {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: unavailable_reason.unwrap_or_else(|| {
                preserved_reason(row, None)
                    .unwrap_or("cell emitted no typed verification report")
                    .into()
            }),
        });
    }
    if reports.iter().any(|report| !classify(report).0) {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: "typed canonical report was neither a match nor a divergence".into(),
        });
    }
    if string(row, "outcome")? != "PASS" {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: row
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or("cell outcome was not PASS despite matched comparison evidence")
                .into(),
        });
    }
    let (_, comparison, compared_log_messages) = reports.last().expect("nonempty reports");
    Ok(CellVerdict::ComparedAndMatched {
        comparison_tier: ComparisonTier::CanonicalBitwise,
        comparison: comparison.clone(),
        bitwise_parity: true,
        compared_log_messages: RequiredNullable::Value(compared_log_messages.clone()),
    })
}
