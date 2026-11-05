//! Append-only audit log: one JSON line per decision, per account.
//!
//! Records hold the time (UTC and US Eastern), host, process ID, class, cost, command, a short
//! argument summary with every inline body replaced by `<N bytes>` and header values redacted,
//! the event, and for completed calls the exit status and time waited. They never contain
//! request bodies, tokens or environment variables. The log is rotated to `.1` past 10 MiB;
//! write it only while holding the account lock so rotation cannot race.

use crate::timefmt::{eastern_stamp, rfc3339_utc};
use serde::Serialize;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Rotate the log past this size, bytes.
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// Longest argument summary kept, characters.
pub const SUMMARY_CHARS: usize = 300;

/// One audit record.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    /// UTC time, RFC 3339.
    pub ts: String,
    /// US Eastern wall time.
    pub et: String,
    /// Short host name.
    pub host: String,
    /// gh-paced process ID.
    pub pid: u32,
    /// `admit`, `throttle`, `refuse`, `pushback`, `exit`, `refresh`, `guard`.
    pub event: String,
    /// Request class.
    pub class: String,
    /// Tokens charged.
    pub cost: u32,
    /// Command (`pr comment`, `api GET repos/o/r`).
    pub command: String,
    /// Redacted, truncated argument summary.
    pub argv: String,
    /// Child exit status (`exit` events), or gh-paced's own refusal status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rc: Option<i32>,
    /// Signal that killed the child, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Seconds this invocation slept before it was admitted or refused.
    pub waited_secs: f64,
    /// Short human-readable detail (reason for a throttle, refusal or pushback).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl Record {
    /// A record stamped at `now`, with the remaining fields filled by the caller.
    pub fn at(now: f64, host: &str, event: &str) -> Self {
        Self {
            ts: rfc3339_utc(now),
            et: eastern_stamp(now),
            host: host.to_string(),
            pid: std::process::id(),
            event: event.to_string(),
            class: String::new(),
            cost: 0,
            command: String::new(),
            argv: String::new(),
            rc: None,
            signal: None,
            waited_secs: 0.0,
            detail: String::new(),
        }
    }
}

/// Join redacted arguments, clipping long tokens and the whole summary.
pub fn summarize(args: &[String]) -> String {
    let mut out = String::new();
    for a in args {
        let token: String = if a.chars().count() > 80 {
            let head: String = a.chars().take(60).collect();
            format!("{head}...<{} bytes>", a.len())
        } else {
            a.clone()
        };
        let token = token.replace(['\n', '\r', '\t'], " ");
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&token);
        if out.chars().count() > SUMMARY_CHARS {
            let clipped: String = out.chars().take(SUMMARY_CHARS).collect();
            return format!("{clipped}...");
        }
    }
    out
}

/// Append one record, rotating first when the log is too large. Errors are returned for the
/// caller to report; they never stop a call.
pub fn append(path: &Path, record: &Record) -> Result<(), String> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > ROTATE_BYTES {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(".1");
            let _ = std::fs::rename(path, rotated);
        }
    }
    let mut line = serde_json::to_vec(record).map_err(|e| format!("audit encode: {e}"))?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot open audit log {}: {e}", path.display()))?;
    file.write_all(&line)
        .map_err(|e| format!("cannot write audit log {}: {e}", path.display()))
}

/// The last `n` records of a log (unparseable lines are skipped).
pub fn tail(path: &Path, n: usize) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_clips_and_flattens() {
        let args: Vec<String> = vec![
            "pr".into(),
            "comment".into(),
            "a\nb".into(),
            "x".repeat(200),
        ];
        let s = summarize(&args);
        assert!(s.starts_with("pr comment a b "));
        assert!(s.contains("...<200 bytes>"));
        let many: Vec<String> = (0..200).map(|i| format!("arg{i}")).collect();
        assert!(summarize(&many).chars().count() <= SUMMARY_CHARS + 3);
    }

    #[test]
    fn append_and_tail() {
        let dir = std::env::temp_dir().join(format!("gh-paced-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("a.audit.jsonl");
        let _ = std::fs::remove_file(&path);
        for event in ["admit", "exit"] {
            let mut r = Record::at(1_790_000_000.0, "host", event);
            r.class = "read".into();
            append(&path, &r).expect("append");
        }
        let t = tail(&path, 5);
        assert_eq!(t.len(), 2);
        assert_eq!(t[1]["event"], "exit");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
