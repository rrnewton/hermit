//! Local token burn from Claude Code transcripts, kept as an incremental index.
//!
//! Claude Code appends one JSON line per assistant content block to
//! `<config>/projects/**/<session>.jsonl` (subagents in their own files), each carrying the
//! message's `usage`. A message split over several lines repeats the same id and usage, so
//! consecutive repeats are counted once. This works for every provider, including those with no
//! plan limits, and costs no request.
//!
//! The index remembers each file's byte offset and keeps per-minute token buckets for the last
//! [`HORIZON`] seconds, so a repeated call reads only what was appended since. A file seen for the
//! first time is entered at the first line inside the horizon, found by binary search on the
//! line timestamps, so a cold start does not read days of old transcript.

use crate::limits::{self, Errors, Event};
use crate::model::Tokens;
use crate::timefmt::parse_iso8601;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// How far back the index keeps buckets: 25 hours, enough for a 24-hour window.
pub const HORIZON: i64 = 25 * 3_600;
/// Files at most this large are read from the start when first seen.
const SMALL_FILE: u64 = 4 << 20;
/// Message ids remembered per file to drop repeated lines of one message.
const RECENT_IDS: usize = 8;

/// Where the index stands in one transcript file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    /// Inode, to notice a file replaced under the same name.
    pub ino: u64,
    /// Bytes consumed (always at a line boundary).
    pub offset: u64,
    /// Most recent message ids, newest last.
    #[serde(default)]
    pub recent_ids: Vec<String>,
}

/// The persisted index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// Format version.
    #[serde(default)]
    pub v: u32,
    /// Per-file progress, keyed by path.
    #[serde(default)]
    pub files: BTreeMap<String, FileState>,
    /// Token totals per minute (key: Unix seconds at the start of the minute).
    #[serde(default)]
    pub buckets: BTreeMap<i64, Tokens>,
    /// Unix seconds of the last scan.
    #[serde(default)]
    pub scanned_at: i64,
    /// API errors per minute (HTTP 429 and others) that exhausted Claude Code's retries.
    #[serde(default)]
    pub errors: BTreeMap<i64, Errors>,
    /// The newest HTTP 429 inside the horizon.
    #[serde(default)]
    pub last_rate_limit: Option<Event>,
}

/// Index format version. An index written by an older version is rebuilt from the horizon, so
/// fields it never collected (API errors, from version 2) are filled in.
pub const INDEX_VERSION: u32 = 2;

/// What one scan did, for benchmarks and `--verbose`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ScanStats {
    /// Transcript files inside the horizon.
    pub files: u64,
    /// Bytes read.
    pub bytes_read: u64,
    /// Assistant messages counted.
    pub messages: u64,
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    // Transcripts are UTF-8 JSON: validating is SIMD-fast and lets `str::contains` (two-way
    // search) do the work, far quicker than a naive window scan over 100 KB tool-result lines.
    match (std::str::from_utf8(haystack), std::str::from_utf8(needle)) {
        (Ok(h), Ok(n)) => h.contains(n),
        _ => haystack.windows(needle.len()).any(|w| w == needle),
    }
}

/// Parse one transcript line into `(unix seconds, message id, tokens)` when it is an assistant
/// message with usage.
pub fn parse_line(line: &[u8]) -> Option<(i64, Option<String>, Tokens)> {
    if !contains(line, b"\"assistant\"") || !contains(line, b"\"usage\"") {
        return None;
    }
    let parsed: Line = serde_json::from_slice(line).ok()?;
    let message = parsed.message?;
    if message.model.as_deref() == Some("<synthetic>") {
        return None;
    }
    let usage = message.usage?;
    let ts = parse_iso8601(parsed.timestamp.as_deref()?)?;
    let tokens = Tokens {
        requests: 1,
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write: usage.cache_creation_input_tokens,
        total: usage.input_tokens
            + usage.output_tokens
            + usage.cache_read_input_tokens
            + usage.cache_creation_input_tokens,
    };
    Some((ts, message.id, tokens))
}

#[derive(Deserialize)]
struct ErrorLine {
    timestamp: Option<String>,
    #[serde(rename = "apiErrorStatus")]
    api_error_status: Option<u16>,
    message: Option<ErrorMessage>,
}

#[derive(Deserialize)]
struct ErrorMessage {
    #[serde(default)]
    content: Vec<serde_json::Value>,
}

/// Parse an API-error line (Claude Code writes one when its retries are exhausted) into
/// `(unix seconds, HTTP status, error text)`.
pub fn parse_error_line(line: &[u8]) -> Option<(i64, Option<u16>, String)> {
    if !contains(line, b"\"isApiErrorMessage\":true") {
        return None;
    }
    let parsed: ErrorLine = serde_json::from_slice(line).ok()?;
    let ts = parse_iso8601(parsed.timestamp.as_deref()?)?;
    let text = parsed
        .message
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some((ts, parsed.api_error_status, text))
}

impl Index {
    /// API errors in the last `seconds` before `now`.
    pub fn errors_in(&self, now: i64, seconds: i64) -> Errors {
        let mut total = Errors::default();
        for (_, e) in self.errors.range(now - seconds..) {
            total.add(e);
        }
        total
    }

    /// Requests in the busiest single minute of the last `seconds`.
    pub fn peak_minute(&self, now: i64, seconds: i64) -> u64 {
        self.buckets
            .range(now - seconds..)
            .map(|(_, t)| t.requests)
            .max()
            .unwrap_or(0)
    }
}

fn line_timestamp(line: &[u8]) -> Option<i64> {
    let key = b"\"timestamp\":\"";
    let at = line.windows(key.len()).rposition(|w| w == key)? + key.len();
    let end = line[at..].iter().position(|&b| b == b'"')? + at;
    parse_iso8601(std::str::from_utf8(&line[at..end]).ok()?)
}

/// First timestamp found at or after byte `pos` (skipping the partial line at `pos`), with the
/// offset of the line that carries it.
fn timestamp_after(file: &mut File, pos: u64, size: u64) -> Option<(u64, i64)> {
    file.seek(SeekFrom::Start(pos)).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut at = pos;
    if pos > 0 {
        let n = reader.read_until(b'\n', &mut line).ok()?;
        at += n as u64;
    }
    // Look at most 4 MiB past `pos` for a line with a timestamp.
    while at < size && at - pos < (4 << 20) {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).ok()?;
        if n == 0 {
            break;
        }
        if let Some(ts) = line_timestamp(&line) {
            return Some((at, ts));
        }
        at += n as u64;
    }
    None
}

/// Byte offset of a line boundary at or before the first line stamped at or after `cutoff`.
pub fn start_offset(path: &Path, size: u64, cutoff: i64) -> u64 {
    if size <= SMALL_FILE {
        return 0;
    }
    let Ok(mut file) = File::open(path) else {
        return 0;
    };
    let (mut lo, mut hi) = (0u64, size);
    while hi - lo > (256 << 10) {
        let mid = lo + (hi - lo) / 2;
        match timestamp_after(&mut file, mid, size) {
            Some((line_at, ts)) if ts < cutoff => lo = line_at,
            _ => hi = mid,
        }
    }
    lo
}

fn walk(dir: &Path, cutoff: i64, out: &mut Vec<(PathBuf, std::fs::Metadata)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            walk(&path, cutoff, out);
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            if let Ok(meta) = entry.metadata() {
                if meta.mtime() >= cutoff {
                    out.push((path, meta));
                }
            }
        }
    }
}

impl Index {
    /// Load from `path`; a missing or unreadable index starts empty.
    pub fn load(path: &Path) -> Index {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write atomically (temporary file, then rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let text = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
    }

    /// Bring the index up to date with every transcript under `projects`.
    pub fn scan(&mut self, projects: &Path, now: i64) -> ScanStats {
        let cutoff = now - HORIZON;
        if self.v != INDEX_VERSION {
            *self = Index::default();
        }
        let mut found = Vec::new();
        walk(projects, cutoff, &mut found);
        let mut stats = ScanStats {
            files: found.len() as u64,
            ..ScanStats::default()
        };
        let mut seen = std::collections::BTreeSet::new();
        for (path, meta) in found {
            let key = path.to_string_lossy().into_owned();
            seen.insert(key.clone());
            let size = meta.len();
            let mut state = match self.files.remove(&key) {
                Some(s) if s.ino == meta.ino() && s.offset <= size => s,
                _ => FileState {
                    ino: meta.ino(),
                    offset: start_offset(&path, size, cutoff),
                    recent_ids: Vec::new(),
                },
            };
            if state.offset < size {
                self.read_from(&path, &mut state, cutoff, &mut stats);
            }
            self.files.insert(key, state);
        }
        self.files.retain(|k, _| seen.contains(k));
        self.buckets.retain(|&minute, _| minute >= cutoff - 60);
        self.scanned_at = now;
        self.errors.retain(|&minute, _| minute >= cutoff - 60);
        if self.last_rate_limit.as_ref().is_some_and(|e| e.ts < cutoff) {
            self.last_rate_limit = None;
        }
        self.v = INDEX_VERSION;
        stats
    }

    fn read_from(
        &mut self,
        path: &Path,
        state: &mut FileState,
        cutoff: i64,
        stats: &mut ScanStats,
    ) {
        let Ok(mut file) = File::open(path) else {
            return;
        };
        if file.seek(SeekFrom::Start(state.offset)).is_err() {
            return;
        }
        let mut reader = BufReader::with_capacity(1 << 20, (&mut file).take(u64::MAX));
        let mut line = Vec::new();
        loop {
            line.clear();
            let Ok(n) = reader.read_until(b'\n', &mut line) else {
                break;
            };
            if n == 0 || line.last() != Some(&b'\n') {
                break; // EOF, or a line still being written: leave it for next time.
            }
            state.offset += n as u64;
            stats.bytes_read += n as u64;
            // Both message kinds counted here are assistant lines; skip everything else after
            // one search (user and tool-result lines are the large ones).
            if !contains(&line, b"\"assistant\"") {
                continue;
            }
            if let Some((ts, status, text)) = parse_error_line(&line) {
                if ts >= cutoff {
                    let bucket = self.errors.entry(ts - ts.rem_euclid(60)).or_default();
                    if status == Some(429) {
                        bucket.rate_limited += 1;
                        if self.last_rate_limit.as_ref().is_none_or(|e| e.ts <= ts) {
                            self.last_rate_limit = Some(limits::event(ts, &text));
                        }
                    } else {
                        bucket.other += 1;
                    }
                }
                continue;
            }
            let Some((ts, id, tokens)) = parse_line(&line) else {
                continue;
            };
            if let Some(id) = id {
                if state.recent_ids.contains(&id) {
                    continue;
                }
                state.recent_ids.push(id);
                if state.recent_ids.len() > RECENT_IDS {
                    state.recent_ids.remove(0);
                }
            }
            if ts < cutoff {
                continue;
            }
            stats.messages += 1;
            self.buckets
                .entry(ts - ts.rem_euclid(60))
                .or_default()
                .add(&tokens);
        }
    }

    /// Token totals for the last `seconds` before `now`.
    pub fn window(&self, now: i64, seconds: i64) -> Tokens {
        let mut total = Tokens::default();
        for (_, t) in self.buckets.range(now - seconds..) {
            total.add(t);
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-usage-tr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_assistant_lines_only() {
        let fixture = include_str!("../tests/fixtures/claude-transcript.jsonl");
        let parsed: Vec<_> = fixture
            .lines()
            .filter_map(|l| parse_line(l.as_bytes()))
            .collect();
        // user line, synthetic message and summary are skipped; the split message appears twice.
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].1.as_deref(), Some("msg_01"));
        assert_eq!(parsed[0].2.output, 120);
        assert_eq!(parsed[0].2.cache_read, 50_000);
        assert_eq!(parsed[0].2.total, 3 + 120 + 50_000 + 1_000);
    }

    #[test]
    fn incremental_scan_counts_each_message_once() {
        let dir = tmpdir("inc");
        let proj = dir.join("projects").join("p");
        std::fs::create_dir_all(proj.join("s").join("subagents")).unwrap();
        let fixture = include_str!("../tests/fixtures/claude-transcript.jsonl");
        std::fs::write(proj.join("s.jsonl"), fixture).unwrap();
        let now = parse_iso8601("2026-10-10T11:00:00Z").unwrap();
        let mut index = Index::default();
        let stats = index.scan(&dir.join("projects"), now);
        assert_eq!(stats.files, 1);
        assert_eq!(stats.messages, 2);
        let w = index.window(now, 3_600);
        assert_eq!(w.requests, 2);
        assert_eq!(w.output, 120 + 40);
        // Older than 15 minutes: msg_01 at 10:30, msg_02 at 10:50.
        assert_eq!(index.window(now, 15 * 60).requests, 1);
        // The API error at 10:51 (no status) and the 429 at 10:55.
        let e = index.errors_in(now, 3_600);
        assert_eq!((e.rate_limited, e.other), (1, 1));
        let last = index.last_rate_limit.clone().unwrap();
        assert_eq!(last.kind, crate::limits::Kind::GatewayUser);
        assert_eq!(last.count, Some((436, 425, 60)));
        assert_eq!(index.peak_minute(now, 3_600), 1);

        // Append a subagent file and a partial line; only complete new lines count.
        let sub = proj.join("s").join("subagents").join("agent-a.jsonl");
        let mut f = std::fs::File::create(&sub).unwrap();
        writeln!(f, r#"{{"type":"assistant","timestamp":"2026-10-10T10:58:00Z","message":{{"id":"msg_09","model":"m","usage":{{"input_tokens":1,"output_tokens":9}}}}}}"#).unwrap();
        write!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-10-10T10:59:00Z""#
        )
        .unwrap();
        drop(f);
        let stats = index.scan(&dir.join("projects"), now);
        assert_eq!(stats.files, 2);
        assert_eq!(stats.messages, 1);
        assert_eq!(index.window(now, 3_600).requests, 3);
        // A second scan with nothing new reads nothing.
        let stats = index.scan(&dir.join("projects"), now);
        assert_eq!(stats.bytes_read, 0);
        // Persist and reload.
        let path = dir.join("index.json");
        index.save(&path).unwrap();
        assert_eq!(Index::load(&path), index);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn binary_search_skips_old_lines() {
        let dir = tmpdir("bs");
        let path = dir.join("big.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let base = parse_iso8601("2026-10-01T00:00:00Z").unwrap();
        let pad = "x".repeat(900);
        // 10,000 lines one minute apart, ~9.5 MB.
        for i in 0..10_000i64 {
            let ts = crate::timefmt::rfc3339_utc(base + i * 60);
            writeln!(f, r#"{{"type":"user","pad":"{pad}","timestamp":"{ts}"}}"#).unwrap();
        }
        drop(f);
        let size = std::fs::metadata(&path).unwrap().len();
        let cutoff = base + 9_000 * 60;
        let off = start_offset(&path, size, cutoff);
        assert!(off > 0 && off < size);
        // The line at `off` is at or before the cutoff and within a few hundred lines of it.
        let mut file = File::open(&path).unwrap();
        let (_, ts) = timestamp_after(&mut file, off, size).unwrap();
        assert!(ts <= cutoff, "{ts} > {cutoff}");
        assert!(
            cutoff - ts < 600 * 60,
            "started {} minutes early",
            (cutoff - ts) / 60
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
