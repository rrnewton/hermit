//! One polling step: read the providers that are stale, append them to the history, refresh the
//! transcript index. Everything happens under the cache lock, so concurrent callers do not all
//! poll: the second one waits, re-reads the history and finds the first one's fresh sample.

use crate::history::{self, Lock};
use crate::model::Sample;
use crate::paths::{claude_dir, codex_dir, ensure_dir, CachePaths, Env};
use crate::transcripts::{Index, ScanStats};
use crate::{claude, codex};

/// What to do.
#[derive(Debug, Clone)]
pub struct PollOptions {
    /// Providers to consider.
    pub providers: Vec<&'static str>,
    /// Reuse a sample younger than this many seconds; `None` never polls (cached only); `Some(0)`
    /// always polls.
    pub max_age: Option<i64>,
    /// Refresh the Claude transcript index.
    pub scan_transcripts: bool,
}

/// What a step produced.
#[derive(Debug, Clone)]
pub struct PollResult {
    /// The history after the step (including the samples just taken).
    pub samples: Vec<Sample>,
    /// Providers polled in this step.
    pub fresh: Vec<&'static str>,
    /// The transcript index, when scanned.
    pub index: Option<Index>,
    /// What the scan read.
    pub scan: Option<ScanStats>,
    /// Codex's retried requests in the last 24 hours `(ts, status, text)`, when scanned and its
    /// log database is readable.
    pub codex_retries: Option<Vec<(i64, u16, String)>>,
}

/// Take one reading of `provider`.
pub fn read_provider(provider: &str, env: Env, now: i64) -> Sample {
    match provider {
        "claude" => claude::read(env, now),
        _ => codex::read(env, now),
    }
}

/// The Claude usage endpoint's minimum polling interval (`AGENT_USAGE_CLAUDE_MIN_INTERVAL`, else
/// [`claude::MIN_INTERVAL`]).
pub fn claude_floor(env: Env) -> i64 {
    env("AGENT_USAGE_CLAUDE_MIN_INTERVAL")
        .and_then(|v| v.parse().ok())
        .unwrap_or(claude::MIN_INTERVAL)
}

/// Whether `provider` should be read now: its newest sample is at least `max_age` old, no
/// `Retry-After` back-off from it is still running, and, for a sample that came from the Claude
/// usage endpoint, at least `claude_floor` seconds have passed (that endpoint rate-limits).
pub fn should_poll(
    samples: &[Sample],
    provider: &str,
    now: i64,
    max_age: i64,
    claude_floor: i64,
) -> bool {
    let Some(latest) = history::latest(samples, provider) else {
        return true;
    };
    if latest.backoff_until.is_some_and(|until| now < until) {
        return false;
    }
    let mut min_age = max_age;
    if provider == "claude" && latest.source.as_deref() == Some("oauth-usage") {
        min_age = min_age.max(claude_floor);
    }
    now - latest.ts >= min_age
}

/// Run one step.
pub fn step(
    paths: &CachePaths,
    env: Env,
    opts: &PollOptions,
    now: i64,
) -> Result<PollResult, String> {
    ensure_dir(&paths.dir)?;
    let _lock = Lock::acquire(&paths.lock())?;
    let mut samples = history::read(&paths.history());
    let mut fresh = Vec::new();
    let mut taken = Vec::new();
    if let Some(max_age) = opts.max_age {
        for &provider in &opts.providers {
            if should_poll(&samples, provider, now, max_age, claude_floor(env)) {
                taken.push(read_provider(provider, env, now));
                fresh.push(provider);
            }
        }
    }
    if !taken.is_empty() {
        history::append(&paths.history(), &taken)?;
        samples.extend(taken);
        samples.sort_by_key(|s| s.ts);
    }
    let (index, scan) = if opts.scan_transcripts && opts.providers.contains(&"claude") {
        let mut index = Index::load(&paths.claude_index());
        let stats = match claude_dir(env) {
            Ok(dir) => index.scan(&dir.join("projects"), now),
            Err(_) => ScanStats::default(),
        };
        index.save(&paths.claude_index())?;
        (Some(index), Some(stats))
    } else {
        (None, None)
    };
    let codex_retries = if opts.scan_transcripts && opts.providers.contains(&"codex") {
        codex_dir(env).ok().and_then(|dir| {
            let mut logs = crate::limits::CodexLogIndex::load(&paths.codex_logs());
            logs.update(&dir, now)?;
            let _ = logs.save(&paths.codex_logs());
            Some(logs.events)
        })
    } else {
        None
    };
    Ok(PollResult {
        samples,
        fresh,
        index,
        scan,
        codex_retries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    fn sample(provider: &str, ts: i64, source: &str) -> Sample {
        let mut s = Sample::new(provider, ts, Status::Ok);
        s.source = Some(source.into());
        s
    }

    #[test]
    fn polling_rules() {
        assert!(should_poll(&[], "claude", 1_000, 120, 300));
        let codex = [sample("codex", 1_000, "app-server")];
        assert!(!should_poll(&codex, "codex", 1_100, 120, 300));
        assert!(should_poll(&codex, "codex", 1_120, 120, 300));
        assert!(should_poll(&codex, "codex", 1_000, 0, 300));
        // The Claude endpoint is never asked more often than the floor, even with --max-age 0.
        let claude = [sample("claude", 1_000, "oauth-usage")];
        assert!(!should_poll(&claude, "claude", 1_200, 0, 300));
        assert!(should_poll(&claude, "claude", 1_300, 0, 300));
        // ...but a reading that never contacted it (no login) is cheap to repeat.
        let none = [sample("claude", 1_000, "none")];
        assert!(should_poll(&none, "claude", 1_120, 120, 300));
        // A Retry-After back-off holds until it expires.
        let mut limited = sample("claude", 1_000, "oauth-usage");
        limited.status = Status::Error;
        limited.backoff_until = Some(3_000);
        assert!(!should_poll(&[limited.clone()], "claude", 2_999, 0, 300));
        assert!(should_poll(&[limited], "claude", 3_000, 0, 300));
    }
}
