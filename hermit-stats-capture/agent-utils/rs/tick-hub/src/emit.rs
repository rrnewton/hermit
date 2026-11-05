//! Stable `HEALTH`, `ACTION`, `NOTE`, and `ERROR` line formatting.

use indexmap::IndexMap;

use crate::text::is_whitespace;

/// Healthy freshness status.
pub const HEALTH_STATUS_OK: &str = "ok";
/// Stale freshness status.
pub const HEALTH_STATUS_STALE: &str = "stale";
/// Missing freshness status.
pub const HEALTH_STATUS_MISSING: &str = "missing";

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn needs_quote(value: &str) -> bool {
    value.is_empty()
        || value.chars().any(is_whitespace)
        || value.contains('"')
        || value.contains('\\')
}

fn fmt_value(value: &str) -> String {
    if needs_quote(value) {
        quote(value)
    } else {
        value.to_string()
    }
}

/// Format an action line, preserving field insertion order.
pub fn format_action(skill: &str, fields: &IndexMap<String, String>, title: &str) -> String {
    let mut parts = vec![format!("ACTION: {skill}")];
    for (key, value) in fields {
        parts.push(format!("{key}={}", fmt_value(value)));
    }
    if !title.is_empty() {
        parts.push(format!("title={}", quote(title)));
    }
    parts.join(" ")
}

/// Format an informational note.
pub fn format_note(text: &str) -> String {
    format!("NOTE: {text}")
}

/// Format an error line.
pub fn format_error(text: &str) -> String {
    format!("ERROR: {text}")
}

/// Format a freshness-health line.
pub fn format_health(
    name: &str,
    status: &str,
    age_secs: Option<i64>,
    threshold_secs: i64,
    detail: &str,
) -> String {
    let age = age_secs.map_or_else(|| "NA".to_string(), |value| value.to_string());
    let escaped_detail = detail.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "HEALTH: {name} {status} age_secs={age} threshold_secs={threshold_secs} detail=\"{escaped_detail}\""
    )
}

/// Format a gate that could not determine its condition.
///
/// Deliberately built without `emit.title` or normal interpolation. A captured
/// summary may be appended as optional detail, but the gate name alone is
/// sufficient, so the line remains emittable when the gate printed nothing.
pub fn format_no_result(name: &str, detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        format!("NO_RESULT: {name} could not determine its condition; this is not a pass")
    } else {
        format!(
            "NO_RESULT: {name} could not determine its condition; this is not a pass ({detail})"
        )
    }
}

/// Format a quiet reminder whose declared dependency has no result.
pub fn format_unevaluable(name: &str, dependencies: &[String]) -> String {
    format!(
        "NO_RESULT: {name} is unevaluable because dependency {} could not determine its condition; this is not a pass",
        dependencies.join(",")
    )
}

/// Format a gate that ran and found nothing wrong.
pub fn format_clean(name: &str) -> String {
    format!("CLEAN: {name} ran and found nothing to report")
}

/// Format a reminder not evaluated because one or more required flags are off.
pub fn format_suppressed(name: &str, missing_flags: &[String]) -> String {
    let flags = if missing_flags.is_empty() {
        "(none recorded)".to_string()
    } else {
        missing_flags.join(",")
    };
    format!("SUPPRESSED: {name} did not run; required flag(s) not set: {flags}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_bare_and_quoted_fields_match_contract() {
        let fields = IndexMap::from([
            ("branch".into(), "main".into()),
            ("runs".into(), "run 1 failed".into()),
            ("empty".into(), "".into()),
            ("separator".into(), "a\u{1c}b".into()),
        ]);
        assert_eq!(
            format_action("ci-health-red", &fields, "CI on main is \"red\""),
            "ACTION: ci-health-red branch=main runs=\"run 1 failed\" empty=\"\" separator=\"a\u{1c}b\" title=\"CI on main is \\\"red\\\"\""
        );
    }

    #[test]
    fn note_error_and_health_match_contract() {
        assert_eq!(format_note("hello world"), "NOTE: hello world");
        assert_eq!(format_error("boom"), "ERROR: boom");
        assert_eq!(
            format_health("db", "missing", None, 100, "snap"),
            "HEALTH: db missing age_secs=NA threshold_secs=100 detail=\"snap\""
        );
    }
}
