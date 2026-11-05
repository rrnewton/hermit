//! Codex: plan usage from the app-server's `account/rateLimits/read` request, which is what the
//! Codex TUI's `/status` reads. Free: the app-server answers it without a model call.
//!
//! Plan windows exist only for ChatGPT logins. With an API key or a gateway provider the server
//! answers "authentication required", reported as `unavailable`. To avoid starting a ~400 MB
//! process for nothing, the probe is skipped when `$CODEX_HOME/auth.json` holds no ChatGPT
//! tokens and the configuration does not keep credentials in a keyring.
//!
//! Independently of plan windows, the per-thread token totals in Codex's state database
//! (`threads.tokens_used`) give a cumulative token counter whose differences are local burn.

use crate::model::{valid_pct, Meter, Sample, Status, Tokens};
use crate::paths::{codex_dir, Env};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long to wait for the app-server's reply.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a stored login can have plan windows, and why not.
pub fn login_kind(dir: &Path) -> Result<&'static str, String> {
    if let Ok(text) = std::fs::read_to_string(dir.join("auth.json")) {
        let value: Value =
            serde_json::from_str(&text).map_err(|_| "auth.json is not JSON".to_string())?;
        let has_tokens = value
            .get("tokens")
            .is_some_and(|t| t.is_object() && t.get("access_token").is_some());
        if has_tokens {
            return Ok("chatgpt");
        }
        if value
            .get("OPENAI_API_KEY")
            .is_some_and(|k| k.as_str().is_some_and(|s| !s.is_empty()))
        {
            return Err("codex is logged in with an API key, which has no plan limits".into());
        }
    }
    let config = std::fs::read_to_string(dir.join("config.toml")).unwrap_or_default();
    let keyring = config.lines().any(|l| {
        let l = l.trim();
        l.starts_with("cli_auth_credentials_store")
            && (l.contains("\"keyring\"") || l.contains("\"auto\""))
    });
    if keyring {
        return Ok("keyring");
    }
    Err(format!(
        "no ChatGPT login in {}/auth.json (an API key or gateway provider has no plan limits)",
        dir.display()
    ))
}

fn window_name(mins: Option<i64>) -> String {
    match mins {
        Some(300) => "5h".into(),
        Some(10_080) => "weekly".into(),
        Some(m) if m % 1_440 == 0 => format!("{}d", m / 1_440),
        Some(m) if m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{m}m"),
        None => "window".into(),
    }
}

fn window_label(mins: Option<i64>) -> String {
    match mins {
        Some(300) => "5-hour window".into(),
        Some(10_080) => "weekly window".into(),
        _ => format!("{} window", window_name(mins)),
    }
}

fn snapshot_meters(snapshot: &Value, out: &mut Vec<Meter>) -> Result<(), String> {
    let limit_id = snapshot
        .get("limitId")
        .and_then(Value::as_str)
        .unwrap_or("codex");
    let limit_name = snapshot
        .get("limitName")
        .and_then(Value::as_str)
        .unwrap_or(limit_id);
    for slot in ["primary", "secondary"] {
        let Some(window) = snapshot.get(slot).filter(|w| !w.is_null()) else {
            continue;
        };
        let used = window
            .get("usedPercent")
            .and_then(Value::as_f64)
            .filter(|v| valid_pct(*v))
            .ok_or_else(|| format!("{limit_id} {slot} window has no valid usedPercent"))?;
        let mins = window.get("windowDurationMins").and_then(Value::as_i64);
        let label = if limit_id == "codex" {
            format!("Codex {}", window_label(mins))
        } else {
            format!("{limit_name} {}", window_label(mins))
        };
        let id = format!("{limit_id}:{}", window_name(mins));
        if out.iter().any(|m| m.id == id) {
            continue;
        }
        out.push(Meter {
            id,
            label,
            used_pct: used,
            resets_at: window.get("resetsAt").and_then(Value::as_i64),
            window_mins: mins,
        });
    }
    Ok(())
}

/// Meters, plan type and whether ordinary usage is allowed, from an `account/rateLimits/read`
/// result object.
pub fn parse_rate_limits(result: &Value) -> Result<(Vec<Meter>, Option<String>), String> {
    let main = result
        .get("rateLimits")
        .filter(|v| v.is_object())
        .ok_or("reply has no rateLimits")?;
    let mut meters = Vec::new();
    snapshot_meters(main, &mut meters)?;
    if let Some(by_id) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        let mut keys: Vec<_> = by_id.keys().collect();
        keys.sort();
        for key in keys {
            snapshot_meters(&by_id[key], &mut meters)?;
        }
    }
    let plan = main
        .get("planType")
        .and_then(Value::as_str)
        .map(str::to_string);
    if meters.is_empty() {
        return Err("rateLimits carried no windows".into());
    }
    Ok((meters, plan))
}

/// Run `codex app-server` and ask for the rate limits. Returns the `result` object.
pub fn probe_app_server(binary: &str, timeout: Duration) -> Result<Value, String> {
    let mut child = Command::new(binary)
        .args(["app-server"])
        .current_dir("/")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {binary} app-server: {e}"))?;
    let stdout = child.stdout.take().ok_or("no app-server stdout")?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let messages = [
        json!({"method": "initialize", "id": 1,
               "params": {"clientInfo": {"name": "agent-usage", "version": env!("CARGO_PKG_VERSION")}}}),
        json!({"method": "initialized"}),
        json!({"method": "account/rateLimits/read", "id": 2,
               "params": {"excludeResetCreditDetails": true}}),
    ];
    let outcome = (|| {
        let mut stdin = child.stdin.take().ok_or("no app-server stdin")?;
        for message in &messages {
            writeln!(stdin, "{message}")
                .map_err(|e| format!("app-server closed its input: {e}"))?;
        }
        stdin
            .flush()
            .map_err(|e| format!("app-server flush: {e}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "no reply to account/rateLimits/read within {}s",
                    timeout.as_secs()
                ));
            }
            let line = match rx.recv_timeout(left) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("app-server exited without answering".into())
                }
            };
            let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if reply.get("id").and_then(Value::as_i64) != Some(2) {
                continue;
            }
            if let Some(err) = reply.get("error") {
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(format!("app-server: {message}"));
            }
            return reply
                .get("result")
                .cloned()
                .ok_or_else(|| "reply has no result".to_string());
        }
    })();
    stop_group(&mut child);
    outcome
}

/// Stop the app-server and anything it started: it runs in its own process group, and a launcher
/// in front of the real binary would otherwise leave the server orphaned. Its stdin is already
/// closed, which makes a well-behaved server exit by itself; SIGTERM after 2 s, SIGKILL after 3 s.
fn stop_group(child: &mut std::process::Child) {
    let pgid = child.id() as libc::pid_t;
    let waited = |child: &mut std::process::Child, ms: u64| {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    };
    if !waited(child, 2_000) {
        // SAFETY: kill(2) on the process group this function's caller created.
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        if !waited(child, 1_000) {
            // SAFETY: as above.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
    let _ = child.wait();
    // Reap stragglers in the group that outlived the leader.
    // SAFETY: as above; ESRCH when the group is already empty is fine.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

/// The newest `state_<n>.sqlite` in the Codex home.
pub fn state_db(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(n) = name
            .strip_prefix("state_")
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

/// Cumulative token counter: the sum of every thread's `tokens_used`, read read-only through the
/// `sqlite3` command (`AGENT_USAGE_SQLITE3` overrides). `None` when there is no database or tool.
pub fn cumulative_tokens(dir: &Path) -> Option<Tokens> {
    let db = state_db(dir)?;
    let sqlite = std::env::var("AGENT_USAGE_SQLITE3").unwrap_or_else(|_| "sqlite3".to_string());
    let out = Command::new(sqlite)
        .args(["-readonly", "-batch", "-noheader", "-separator", " "])
        .arg(&db)
        .arg("select coalesce(sum(tokens_used),0), count(*) from threads;")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let total = parts.next()?.parse::<u64>().ok()?;
    Some(Tokens {
        total,
        ..Tokens::default()
    })
}

/// Read Codex's plan usage and token counter now.
pub fn read(env: Env, now: i64) -> Sample {
    let started = Instant::now();
    let mut sample = match codex_dir(env) {
        Err(e) => {
            let mut s = Sample::new("codex", now, Status::Error);
            s.detail = Some(e);
            s
        }
        Ok(dir) => {
            let mut s = read_plan(env, &dir, now);
            s.tokens_cumulative = cumulative_tokens(&dir);
            s
        }
    };
    sample.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    sample
}

fn read_plan(env: Env, dir: &Path, now: i64) -> Sample {
    let forced = env("AGENT_USAGE_CODEX_PROBE").is_some_and(|v| v == "always");
    if !forced {
        if let Err(why) = login_kind(dir) {
            let mut s = Sample::new("codex", now, Status::Unavailable);
            s.source = Some("none".into());
            s.detail = Some(why);
            return s;
        }
    }
    let binary = env("AGENT_USAGE_CODEX_BIN").unwrap_or_else(|| "codex".to_string());
    let probed = probe_app_server(&binary, PROBE_TIMEOUT);
    if let Ok(result) = &probed {
        crate::paths::save_raw(env, "codex-ratelimits", now, &result.to_string());
    }
    match probed {
        Ok(result) => match parse_rate_limits(&result) {
            Ok((meters, plan)) => {
                let mut s = Sample::new("codex", now, Status::Ok);
                s.source = Some("app-server".into());
                s.plan = plan;
                s.meters = meters;
                s
            }
            Err(e) => {
                let mut s = Sample::new("codex", now, Status::Error);
                s.source = Some("app-server".into());
                s.detail = Some(e);
                s
            }
        },
        Err(e) => {
            let unavailable = e.contains("authentication required");
            let mut s = Sample::new(
                "codex",
                now,
                if unavailable {
                    Status::Unavailable
                } else {
                    Status::Error
                },
            );
            s.source = Some("app-server".into());
            s.detail = Some(e);
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_result() -> Value {
        let text = include_str!("../tests/fixtures/codex-ratelimits.jsonl");
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("id").and_then(Value::as_i64) == Some(2))
            .and_then(|v| v.get("result").cloned())
            .unwrap()
    }

    #[test]
    fn parses_primary_secondary_and_extra_buckets() {
        let (meters, plan) = parse_rate_limits(&fixture_result()).unwrap();
        assert_eq!(plan.as_deref(), Some("plus"));
        let ids: Vec<_> = meters.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["codex:5h", "codex:weekly", "codex_other:weekly"]);
        assert_eq!(meters[0].label, "Codex 5-hour window");
        assert_eq!(meters[0].used_pct, 12.0);
        assert_eq!(meters[0].resets_at, Some(1_791_640_000));
        assert_eq!(meters[2].label, "GPT-5.6-Sol weekly window");
    }

    #[test]
    fn rejects_missing_windows() {
        assert!(parse_rate_limits(&json!({})).is_err());
        assert!(
            parse_rate_limits(&json!({"rateLimits": {"primary": null, "secondary": null}}))
                .is_err()
        );
        assert!(
            parse_rate_limits(&json!({"rateLimits": {"primary": {"usedPercent": -3}}})).is_err()
        );
    }

    #[test]
    fn login_kinds() {
        let dir = std::env::temp_dir().join(format!("agent-usage-codex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(login_kind(&dir).is_err());
        std::fs::write(
            dir.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-x","tokens":null}"#,
        )
        .unwrap();
        assert!(login_kind(&dir).unwrap_err().contains("API key"));
        std::fs::write(
            dir.join("auth.json"),
            r#"{"tokens":{"access_token":"a","account_id":"b"}}"#,
        )
        .unwrap();
        assert_eq!(login_kind(&dir), Ok("chatgpt"));
        std::fs::remove_file(dir.join("auth.json")).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .unwrap();
        assert_eq!(login_kind(&dir), Ok("keyring"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn window_names() {
        assert_eq!(window_name(Some(300)), "5h");
        assert_eq!(window_name(Some(10_080)), "weekly");
        assert_eq!(window_name(Some(2_880)), "2d");
        assert_eq!(window_name(Some(120)), "2h");
        assert_eq!(window_name(Some(45)), "45m");
        assert_eq!(window_name(None), "window");
    }
}
