//! Where things live: the cache directory and each harness's home.
//!
//! Every lookup takes an environment accessor so tests can supply their own.

use std::path::{Path, PathBuf};

/// Environment accessor: `env("HOME")`.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment, ignoring empty values.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn home(env: Env) -> Result<PathBuf, String> {
    env("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

/// Cache directory: `$AGENT_USAGE_DIR`, else `$XDG_CACHE_HOME/agent-usage`, else
/// `~/.cache/agent-usage`.
pub fn cache_dir(env: Env) -> Result<PathBuf, String> {
    if let Some(dir) = env("AGENT_USAGE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(xdg) = env("XDG_CACHE_HOME") {
        return Ok(Path::new(&xdg).join("agent-usage"));
    }
    Ok(home(env)?.join(".cache").join("agent-usage"))
}

/// Claude Code's configuration directory: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir(env: Env) -> Result<PathBuf, String> {
    if let Some(dir) = env("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(home(env)?.join(".claude"))
}

/// Codex's home directory: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_dir(env: Env) -> Result<PathBuf, String> {
    if let Some(dir) = env("CODEX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    Ok(home(env)?.join(".codex"))
}

/// When `AGENT_USAGE_RAW_DIR` is set, write a provider's raw reply there as `<kind>-<ts>.json`,
/// for recording real replies as test fixtures. Replies carry usage figures, not credentials.
/// Failures are ignored: recording must never break a reading.
pub fn save_raw(env: Env, kind: &str, ts: i64, body: &str) {
    if let Some(dir) = env("AGENT_USAGE_RAW_DIR") {
        let dir = PathBuf::from(dir);
        if ensure_dir(&dir).is_ok() {
            let _ = std::fs::write(dir.join(format!("{kind}-{ts}.json")), body);
        }
    }
}

/// Create a private directory (mode 0700) and its parents if missing.
pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// Files inside the cache directory.
#[derive(Debug, Clone)]
pub struct CachePaths {
    /// The cache directory itself.
    pub dir: PathBuf,
}

impl CachePaths {
    /// Paths rooted at `dir`.
    pub fn new(dir: PathBuf) -> Self {
        CachePaths { dir }
    }
    /// Append-only JSONL of samples.
    pub fn history(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }
    /// Lock serialising history appends and transcript-index updates.
    pub fn lock(&self) -> PathBuf {
        self.dir.join("lock")
    }
    /// Incremental index of Claude Code transcripts (file offsets and per-minute token buckets).
    pub fn claude_index(&self) -> PathBuf {
        self.dir.join("claude-transcripts.json")
    }
    /// Incremental index of Codex's retried requests (from its log database).
    pub fn codex_logs(&self) -> PathBuf {
        self.dir.join("codex-logs.json")
    }
    /// Lock held by a running daemon for its whole life.
    pub fn daemon_lock(&self) -> PathBuf {
        self.dir.join("daemon.lock")
    }
    /// Process id of the running daemon.
    pub fn daemon_pid(&self) -> PathBuf {
        self.dir.join("daemon.pid")
    }
    /// Log of a detached daemon.
    pub fn daemon_log(&self) -> PathBuf {
        self.dir.join("daemon.log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn cache_dir_precedence() {
        let e = env_of(&[("HOME", "/h")]);
        assert_eq!(
            cache_dir(&e).unwrap(),
            PathBuf::from("/h/.cache/agent-usage")
        );
        let e = env_of(&[("HOME", "/h"), ("XDG_CACHE_HOME", "/x")]);
        assert_eq!(cache_dir(&e).unwrap(), PathBuf::from("/x/agent-usage"));
        let e = env_of(&[
            ("HOME", "/h"),
            ("XDG_CACHE_HOME", "/x"),
            ("AGENT_USAGE_DIR", "/a"),
        ]);
        assert_eq!(cache_dir(&e).unwrap(), PathBuf::from("/a"));
        let e = env_of(&[]);
        assert!(cache_dir(&e).is_err());
    }

    #[test]
    fn harness_dirs() {
        let e = env_of(&[("HOME", "/h")]);
        assert_eq!(claude_dir(&e).unwrap(), PathBuf::from("/h/.claude"));
        assert_eq!(codex_dir(&e).unwrap(), PathBuf::from("/h/.codex"));
        let e = env_of(&[
            ("HOME", "/h"),
            ("CLAUDE_CONFIG_DIR", "/c"),
            ("CODEX_HOME", "/d"),
        ]);
        assert_eq!(claude_dir(&e).unwrap(), PathBuf::from("/c"));
        assert_eq!(codex_dir(&e).unwrap(), PathBuf::from("/d"));
    }
}
