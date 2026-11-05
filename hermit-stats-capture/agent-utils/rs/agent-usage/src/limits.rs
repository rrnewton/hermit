//! Rate-limit (HTTP 429) evidence from local files, for hosts whose real limit is a gateway's
//! request rate rather than a plan window.
//!
//! What is visible differs by harness:
//!
//! - **Claude Code** writes an API error into the transcript only when its retries are exhausted
//!   (a synthetic assistant message with `isApiErrorMessage` and `apiErrorStatus`). A 429 that a
//!   retry got past leaves no local trace, so these counts are a floor.
//! - **Codex** logs every retried request to its log database (`logs_<n>.sqlite`, target
//!   `codex_core::responses_retry`, `sampling_error=unexpected status <code> ...`), so its counts
//!   include 429s that a retry got past.
//!
//! A 429's text says whose limit it was: a gateway's per-user request limit names the counted key
//! and `count/max in <window>s`; a provider quota (`RESOURCE_EXHAUSTED`, `Quota exceeded`) is
//! shared by everyone behind the same provider project.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Whose limit a 429 came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A gateway's per-user request-rate limit (`rate limit exceeded for <key>: N/M in Ws`).
    GatewayUser,
    /// The model provider's quota (`RESOURCE_EXHAUSTED`, `Quota exceeded`), shared beyond one user.
    ProviderQuota,
    /// A plan's usage limit or anything else.
    Other,
}

/// One observed 429.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Unix seconds.
    pub ts: i64,
    /// Whose limit it was.
    pub kind: Kind,
    /// For a gateway limit: requests counted, the maximum, and the window in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<(u64, u64, u64)>,
    /// The error text, shortened.
    pub detail: String,
}

/// Classify a 429's error text. Returns the kind and, for a gateway limit, `(count, max, window)`.
pub fn classify(text: &str) -> (Kind, Option<(u64, u64, u64)>) {
    if let Some(at) = text.find("rate limit exceeded for ") {
        let rest = &text[at + "rate limit exceeded for ".len()..];
        // `<key>: <count>/<max> in <window>s`
        let parsed = rest.split_once(": ").and_then(|(_, tail)| {
            let (count, tail) = tail.split_once('/')?;
            let (max, tail) = tail.split_once(" in ")?;
            let window: String = tail.chars().take_while(char::is_ascii_digit).collect();
            Some((
                count.trim().parse().ok()?,
                max.trim().parse().ok()?,
                window.parse().ok()?,
            ))
        });
        return (Kind::GatewayUser, parsed);
    }
    if text.contains("RESOURCE_EXHAUSTED")
        || text.contains("Quota exceeded")
        || text.contains("Resource exhausted")
    {
        return (Kind::ProviderQuota, None);
    }
    (Kind::Other, None)
}

/// An [`Event`] from a 429's text.
pub fn event(ts: i64, text: &str) -> Event {
    let (kind, count) = classify(text);
    Event {
        ts,
        kind,
        count,
        detail: text.chars().take(240).collect(),
    }
}

/// Error counts in one minute bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Errors {
    /// HTTP 429 responses.
    #[serde(default)]
    pub rate_limited: u64,
    /// Every other API error.
    #[serde(default)]
    pub other: u64,
}

impl Errors {
    /// Field-wise sum.
    pub fn add(&mut self, other: &Errors) {
        self.rate_limited += other.rate_limited;
        self.other += other.other;
    }
}

/// Codex's newest `logs_<n>.sqlite`.
pub fn codex_log_db(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(n) = name
            .to_string_lossy()
            .strip_prefix("logs_")
            .and_then(|r| r.strip_suffix(".sqlite"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| n > *b) {
            best = Some((n, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// The HTTP status a Codex retry line reports (`unexpected status 429 Too Many Requests: ...`),
/// and the text from there on.
pub fn codex_retry_status(body: &str) -> Option<(u16, &str)> {
    let at = body.find("unexpected status ")?;
    let tail = &body[at + "unexpected status ".len()..];
    let code: String = tail.chars().take_while(char::is_ascii_digit).collect();
    Some((code.parse().ok()?, tail))
}

/// Codex's retried requests, kept incrementally: the log database's row ids only grow, so after
/// the first read (one indexed range on `ts` for the last 24 hours) each call reads only rows
/// added since, by primary key. Stored as `codex-logs.json` in the cache directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexLogIndex {
    /// The log database the index belongs to.
    #[serde(default)]
    pub db: String,
    /// Highest row id seen.
    #[serde(default)]
    pub last_id: i64,
    /// Retried requests in the last 24 hours: `(ts, HTTP status, text)`.
    #[serde(default)]
    pub events: Vec<(i64, u16, String)>,
}

/// How long retried requests are kept.
pub const CODEX_HORIZON: i64 = 86_400;

impl CodexLogIndex {
    /// Load from `path`; missing or unreadable starts empty.
    pub fn load(path: &Path) -> CodexLogIndex {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write atomically.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let text = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
    }

    /// Read new rows from the newest log database under `dir`, through the `sqlite3` command
    /// (`AGENT_USAGE_SQLITE3` overrides). `None` when there is no database or tool.
    pub fn update(&mut self, dir: &Path, now: i64) -> Option<()> {
        let sqlite = std::env::var("AGENT_USAGE_SQLITE3").unwrap_or_else(|_| "sqlite3".to_string());
        self.update_with(dir, now, &sqlite)
    }

    /// [`CodexLogIndex::update`] with an explicit `sqlite3` command.
    pub fn update_with(&mut self, dir: &Path, now: i64, sqlite: &str) -> Option<()> {
        let db = codex_log_db(dir)?;
        let name = db.display().to_string();
        if self.db != name {
            *self = CodexLogIndex {
                db: name,
                ..CodexLogIndex::default()
            };
        }
        let since = now - CODEX_HORIZON;
        let filter = if self.last_id == 0 {
            format!("ts >= {since}")
        } else {
            format!("id > {}", self.last_id)
        };
        // The first row carries the table's highest id (ts = -1 marks it), so the next call can
        // start after it even when nothing matched.
        let query = format!(
            "select max(id) as id, -1 as ts, '' as b from logs union all \
             select id, ts, substr(feedback_log_body, -1200) from logs \
             where {filter} and target = 'codex_core::responses_retry';"
        );
        let out = Command::new(sqlite)
            .args(["-readonly", "-batch", "-json"])
            .arg(&db)
            .arg(query)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let rows: Vec<serde_json::Value> = if text.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&text).ok()?
        };
        let previous = self.last_id;
        let mut top = self.last_id;
        for row in &rows {
            let id = row
                .get("id")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let ts = row
                .get("ts")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            if ts == -1 {
                if id < self.last_id {
                    // The database was replaced or truncated: start again next time.
                    *self = CodexLogIndex::default();
                    return Some(());
                }
                top = top.max(id);
                continue;
            }
            if id <= previous {
                continue; // already counted
            }
            top = top.max(id);
            let body = row
                .get("b")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if let Some((code, tail)) = codex_retry_status(body) {
                if ts >= since {
                    self.events
                        .push((ts, code, tail.chars().take(600).collect()));
                }
            }
        }
        self.last_id = top;
        self.events.retain(|(ts, _, _)| *ts >= since);
        self.events.sort_by_key(|e| e.0);
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_gateway_provider_and_other() {
        let (k, c) =
            classify("rate limit exceeded for u-12345::auto-0::up-vertex::w-60s: 436/425 in 60s");
        assert_eq!(k, Kind::GatewayUser);
        assert_eq!(c, Some((436, 425, 60)));
        let (k, c) = classify("unexpected status 429 Too Many Requests: rate limit exceeded for u-1::w-600s: 901/900 in 600s, url: x");
        assert_eq!((k, c), (Kind::GatewayUser, Some((901, 900, 600))));
        assert_eq!(classify("rate limit exceeded for something odd").1, None);
        let vertex = r#"API Error: Request rejected (429) · [{"error":{"code":429,"message":"Resource exhausted. Please try again later.","status":"RESOURCE_EXHAUSTED"}}]"#;
        assert_eq!(classify(vertex).0, Kind::ProviderQuota);
        assert_eq!(
            classify("Quota exceeded for quota metric 'Online prediction requests'").0,
            Kind::ProviderQuota
        );
        assert_eq!(classify("usage limit reached for your plan").0, Kind::Other);
    }

    #[test]
    fn codex_log_index_reads_incrementally() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agent-usage-cl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("logs_2.sqlite"), "").unwrap();
        // A fake sqlite3 that records its query and answers from a file.
        let fake = dir.join("sqlite3");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nfor a; do q=\"$a\"; done\necho \"$q\" >> {d}/queries\ncat {d}/answer\n",
                d = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let answer = |text: &str| std::fs::write(dir.join("answer"), text).unwrap();
        let fake = fake.to_str().unwrap();
        let mut idx = CodexLogIndex::default();
        answer(
            r#"[{"id":500,"ts":-1,"b":""},{"id":480,"ts":99000,"b":"sampling_error=unexpected status 429 Too Many Requests: x"}]"#,
        );
        idx.update_with(&dir, 100_000, fake).unwrap();
        assert_eq!(idx.last_id, 500);
        assert_eq!(idx.events.len(), 1);
        answer(r#"[{"id":510,"ts":-1,"b":""}]"#);
        idx.update_with(&dir, 100_100, fake).unwrap();
        assert_eq!(idx.last_id, 510);
        let queries = std::fs::read_to_string(dir.join("queries")).unwrap();
        let lines: Vec<_> = queries.lines().collect();
        assert!(lines[0].contains("ts >= 13600"), "{}", lines[0]);
        assert!(lines[1].contains("id > 500"), "{}", lines[1]);
        // Events older than 24 hours fall out.
        answer(r#"[{"id":510,"ts":-1,"b":""}]"#);
        idx.update_with(&dir, 99_000 + CODEX_HORIZON + 1, fake)
            .unwrap();
        assert!(idx.events.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn codex_retry_lines() {
        let body = "x: retrying sampling request (1/5 in 196ms)... retries=1 max_retries=5 sampling_error=unexpected status 404 Not Found: The model does not exist, url: https://h/v1/responses";
        let (code, tail) = codex_retry_status(body).unwrap();
        assert_eq!(code, 404);
        assert!(tail.starts_with("404 Not Found"));
        assert!(codex_retry_status("stream disconnected before completion").is_none());
    }
}
