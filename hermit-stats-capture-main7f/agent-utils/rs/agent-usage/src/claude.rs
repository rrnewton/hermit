//! Claude Code: plan usage from the claude.ai usage endpoint, the same one the `/status` Usage tab
//! reads. Free: no model call, no session.
//!
//! The endpoint needs a claude.ai subscription login (OAuth token with the `user:profile` scope).
//! Sessions on an API key, a cloud provider (Bedrock, Vertex, Foundry) or a gateway have no plan
//! limits, and are reported as `unavailable` rather than as an error.

use crate::http;
use crate::model::{valid_pct, Meter, Sample, Status};
use crate::paths::{claude_dir, Env};
use crate::timefmt::parse_iso8601;
use serde_json::Value;
use std::path::Path;

/// Default usage endpoint (override with `AGENT_USAGE_CLAUDE_USAGE_URL`).
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Beta header the OAuth endpoints require.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Shortest time between two requests to the usage endpoint, whatever `--max-age` says. The
/// endpoint answers frequent polling with HTTP 429 and a long `Retry-After` (third-party reports
/// show 832 s and 2,449 s); a 10-minute cadence is reported stable. Override with
/// `AGENT_USAGE_CLAUDE_MIN_INTERVAL` (seconds).
pub const MIN_INTERVAL: i64 = 300;
/// Back-off after a 429 that carries no usable `Retry-After`.
pub const BACKOFF_DEFAULT: i64 = 600;
/// Longest back-off honoured, so a garbage `Retry-After` cannot stop polling indefinitely.
pub const BACKOFF_MAX: i64 = 6 * 3_600;

/// The parts of a stored claude.ai login this tool needs. Never logged or written anywhere.
pub struct Login {
    /// OAuth access token.
    pub access_token: String,
    /// Expiry in Unix milliseconds, when stored.
    pub expires_at_ms: Option<i64>,
    /// Granted scopes, when stored.
    pub scopes: Option<Vec<String>>,
    /// Subscription type (`pro`, `max`, `team`, `enterprise`), when stored.
    pub subscription: Option<String>,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("access_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("scopes", &self.scopes)
            .field("subscription", &self.subscription)
            .finish()
    }
}

/// Parse the credentials JSON Claude Code stores (`.credentials.json`, or the macOS keychain
/// item `Claude Code-credentials`). `None` when it holds no claude.ai login.
pub fn parse_credentials(text: &str) -> Option<Login> {
    let value: Value = serde_json::from_str(text).ok()?;
    let oauth = value.get("claudeAiOauth")?;
    let access_token = oauth.get("accessToken")?.as_str()?.to_string();
    if access_token.is_empty() {
        return None;
    }
    Some(Login {
        access_token,
        expires_at_ms: oauth.get("expiresAt").and_then(Value::as_i64),
        scopes: oauth.get("scopes").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        }),
        subscription: oauth
            .get("subscriptionType")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn read_keychain() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Which non-subscription provider the environment selects, if any.
pub fn third_party_provider(env: Env) -> Option<&'static str> {
    let on = |name: &str| env(name).is_some_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"));
    if on("CLAUDE_CODE_USE_BEDROCK") {
        Some("bedrock")
    } else if on("CLAUDE_CODE_USE_VERTEX") {
        Some("vertex")
    } else if on("CLAUDE_CODE_USE_FOUNDRY") {
        Some("foundry")
    } else if env("ANTHROPIC_API_KEY").is_some() {
        Some("api-key")
    } else {
        None
    }
}

/// Find the stored login: `.credentials.json` in the config directory, then the macOS keychain.
pub fn find_login(dir: &Path) -> Option<Login> {
    let file = dir.join(".credentials.json");
    if let Ok(text) = std::fs::read_to_string(&file) {
        if let Some(login) = parse_credentials(&text) {
            return Some(login);
        }
    }
    read_keychain().and_then(|t| parse_credentials(&t))
}

fn meter_for_kind(kind: &str, scope: Option<&str>) -> (String, String, Option<i64>) {
    match (kind, scope) {
        ("session", _) => ("session".into(), "Current session".into(), Some(300)),
        ("weekly_all", _) => (
            "weekly_all".into(),
            "Current week (all models)".into(),
            Some(10_080),
        ),
        ("weekly_scoped", Some(name)) => (
            format!("weekly:{name}"),
            format!("Current week ({name})"),
            Some(10_080),
        ),
        (other, Some(name)) => (format!("{other}:{name}"), format!("{other} ({name})"), None),
        (other, None) => (other.to_string(), other.to_string(), None),
    }
}

const LEGACY: [(&str, &str, &str, i64); 5] = [
    ("five_hour", "session", "Current session", 300),
    (
        "seven_day",
        "weekly_all",
        "Current week (all models)",
        10_080,
    ),
    (
        "seven_day_sonnet",
        "weekly:Sonnet",
        "Current week (Sonnet)",
        10_080,
    ),
    (
        "seven_day_opus",
        "weekly:Opus",
        "Current week (Opus)",
        10_080,
    ),
    (
        "seven_day_oauth_apps",
        "weekly:oauth_apps",
        "Current week (OAuth apps)",
        10_080,
    ),
];

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|v| valid_pct(*v))
}

fn reset_time(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(s) => parse_iso8601(s),
        Value::Number(n) => n
            .as_i64()
            .map(|v| if v > 100_000_000_000 { v / 1000 } else { v }),
        _ => None,
    }
}

/// Meters from a usage-endpoint reply. The server's `limits[]` rows are used when present (they
/// carry per-model weekly windows such as Fable); otherwise the legacy named windows. A row whose
/// percentage is missing or out of range makes the whole reply an error: unknown headroom must not
/// read as zero.
pub fn parse_usage(body: &str) -> Result<Vec<Meter>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("usage reply is not JSON: {e}"))?;
    if let Some(err) = value.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("usage endpoint error: {message}"));
    }
    if !value.is_object() {
        return Err("usage reply is not an object".into());
    }
    let mut meters = Vec::new();
    if let Some(rows) = value
        .get("limits")
        .and_then(Value::as_array)
        .filter(|r| !r.is_empty())
    {
        for row in rows {
            let kind = row
                .get("kind")
                .and_then(Value::as_str)
                .ok_or("limits row without kind")?;
            let scope = row
                .get("scope")
                .and_then(|s| s.get("model").or_else(|| s.get("surface")))
                .and_then(|m| m.get("display_name"))
                .and_then(Value::as_str);
            let used = number(row.get("percent"))
                .ok_or_else(|| format!("limits row {kind} has no valid percent"))?;
            let (id, label, window) = meter_for_kind(kind, scope);
            meters.push(Meter {
                id,
                label,
                used_pct: used,
                resets_at: reset_time(row.get("resets_at")),
                window_mins: window,
            });
        }
    } else {
        for (key, id, label, window) in LEGACY {
            let Some(window_value) = value.get(key).filter(|v| !v.is_null()) else {
                continue;
            };
            let used = number(window_value.get("utilization"))
                .ok_or_else(|| format!("{key} has no valid utilization"))?;
            meters.push(Meter {
                id: id.to_string(),
                label: label.to_string(),
                used_pct: used,
                resets_at: reset_time(window_value.get("resets_at")),
                window_mins: Some(window),
            });
        }
    }
    if let Some(extra) = value.get("extra_usage").filter(|v| !v.is_null()) {
        if extra.get("is_enabled").and_then(Value::as_bool) == Some(true) {
            if let Some(used) = number(extra.get("utilization")) {
                meters.push(Meter {
                    id: "extra_usage".into(),
                    label: "Extra usage (billing period)".into(),
                    used_pct: used,
                    resets_at: None,
                    window_mins: None,
                });
            }
        }
    }
    if meters.is_empty() {
        return Err("usage reply carried no usage windows".into());
    }
    Ok(meters)
}

/// Read Claude's plan usage now.
pub fn read(env: Env, now: i64) -> Sample {
    let started = std::time::Instant::now();
    let mut sample = read_inner(env, now);
    sample.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    sample
}

fn read_inner(env: Env, now: i64) -> Sample {
    let dir = match claude_dir(env) {
        Ok(d) => d,
        Err(e) => return error(now, &e),
    };
    let Some(login) = find_login(&dir) else {
        let mut s = Sample::new("claude", now, Status::Unavailable);
        s.source = Some("none".into());
        s.detail = Some(match third_party_provider(env) {
            Some(p) => format!(
                "no claude.ai login; this environment uses {p}, which has no plan limits"
            ),
            None => format!(
                "no claude.ai login in {}/.credentials.json (an API key, cloud provider or gateway has no plan limits)",
                dir.display()
            ),
        });
        return s;
    };
    if let Some(scopes) = &login.scopes {
        if !scopes.iter().any(|s| s == "user:profile") {
            let mut s = Sample::new("claude", now, Status::Unavailable);
            s.source = Some("none".into());
            s.plan = login.subscription.clone();
            s.detail = Some(
                "the stored login lacks the user:profile scope (a setup-token login), so usage cannot be read"
                    .into(),
            );
            return s;
        }
    }
    if let Some(exp) = login.expires_at_ms {
        // Claude Code writes milliseconds; some third-party tools have written seconds.
        let exp = if exp >= 100_000_000_000 {
            exp / 1000
        } else {
            exp
        };
        if exp <= now {
            let mut s = error(
                now,
                &format!(
                    "stored claude.ai login expired {} ago; any Claude Code session refreshes it",
                    crate::timefmt::human_duration(now - exp)
                ),
            );
            // Nothing was sent, so this does not count against the endpoint's polling floor.
            s.source = Some("none".into());
            s.plan = login.subscription.clone();
            return s;
        }
    }
    let url = env("AGENT_USAGE_CLAUDE_USAGE_URL").unwrap_or_else(|| USAGE_URL.to_string());
    let auth = format!("Bearer {}", login.access_token);
    let agent = format!("agent-usage/{}", env!("CARGO_PKG_VERSION"));
    let headers = [
        ("Authorization", auth.as_str()),
        ("anthropic-beta", OAUTH_BETA),
        ("Content-Type", "application/json"),
        ("User-Agent", agent.as_str()),
    ];
    let resp = match http::get(&url, &headers, 10) {
        Ok(r) => r,
        Err(e) => return error(now, &e),
    };
    crate::paths::save_raw(env, "claude-usage", now, &resp.body);
    if resp.status == 429 {
        // The usage endpoint is itself rate-limited, per organisation and shared with Claude
        // Code's own polling. Honour Retry-After (capped) so no poll runs before it expires.
        let wait = http::retry_after_secs(resp.header("retry-after"))
            .unwrap_or(BACKOFF_DEFAULT)
            .clamp(60, BACKOFF_MAX);
        let mut s = error(
            now,
            &format!(
                "usage endpoint rate-limited (HTTP 429); not asking again for {}",
                crate::timefmt::human_duration(wait)
            ),
        );
        s.backoff_until = Some(now + wait);
        s.plan = login.subscription;
        return s;
    }
    if resp.status != 200 && resp.status != 0 {
        let snippet: String = resp.body.chars().take(160).collect();
        return error(
            now,
            &format!("usage endpoint HTTP {}: {snippet}", resp.status),
        );
    }
    match parse_usage(&resp.body) {
        Ok(meters) => {
            let mut s = Sample::new("claude", now, Status::Ok);
            s.source = Some("oauth-usage".into());
            s.plan = login.subscription;
            s.meters = meters;
            s
        }
        Err(e) => error(now, &e),
    }
}

fn error(now: i64, detail: &str) -> Sample {
    let mut s = Sample::new("claude", now, Status::Error);
    s.source = Some("oauth-usage".into());
    s.detail = Some(detail.to_string());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: &str = include_str!("../tests/fixtures/claude-usage-limits.json");
    const LEGACY_BODY: &str = include_str!("../tests/fixtures/claude-usage-legacy.json");

    #[test]
    fn parses_server_rows_with_model_scope() {
        let meters = parse_usage(LIMITS).unwrap();
        let ids: Vec<_> = meters.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["session", "weekly_all", "weekly:Fable"]);
        assert_eq!(meters[0].label, "Current session");
        assert_eq!(meters[0].used_pct, 8.0);
        assert_eq!(meters[0].resets_at, Some(1_791_731_400));
        assert_eq!(meters[2].label, "Current week (Fable)");
        assert_eq!(meters[2].used_pct, 5.0);
        assert_eq!(meters[2].resets_at, Some(1_791_746_400));
    }

    #[test]
    fn parses_legacy_windows_and_skips_nulls() {
        let meters = parse_usage(LEGACY_BODY).unwrap();
        let ids: Vec<_> = meters.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["session", "weekly_all", "weekly:Sonnet", "extra_usage"]
        );
        assert_eq!(meters[1].used_pct, 22.0);
        assert_eq!(meters[1].window_mins, Some(10_080));
    }

    #[test]
    fn parses_published_real_replies() {
        let a = parse_usage(include_str!(
            "../tests/fixtures/claude-usage-published-a.json"
        ))
        .unwrap();
        let got: Vec<_> = a.iter().map(|m| (m.id.as_str(), m.used_pct)).collect();
        assert_eq!(
            got,
            [
                ("session", 6.0),
                ("weekly_all", 35.0),
                ("weekly:Sonnet", 21.0),
                ("weekly:Opus", 12.0),
                ("extra_usage", 12.5)
            ]
        );
        assert_eq!(a[0].resets_at, parse_iso8601("2026-04-08T18:59:59Z"));
        let b = parse_usage(include_str!(
            "../tests/fixtures/claude-usage-published-b.json"
        ))
        .unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].used_pct, 0.37);
        assert_eq!(b[0].resets_at, parse_iso8601("2026-04-21T03:00:00Z"));
        assert_eq!(b[1].used_pct, 0.67);
    }

    #[test]
    fn rejects_errors_and_malformed_rows() {
        assert!(
            parse_usage("{\"type\":\"error\",\"error\":{\"message\":\"nope\"}}")
                .unwrap_err()
                .contains("nope")
        );
        assert!(parse_usage("[]").is_err());
        assert!(parse_usage("{}").is_err());
        assert!(parse_usage("{\"limits\":[{\"kind\":\"session\",\"percent\":\"x\"}]}").is_err());
        assert!(parse_usage("{\"five_hour\":{\"utilization\":null}}").is_err());
        assert!(parse_usage("{\"five_hour\":{\"utilization\":1e9}}").is_err());
    }

    #[test]
    fn credentials_parse_and_redact() {
        let login = parse_credentials(
            r#"{"claudeAiOauth":{"accessToken":"SECRET-123","expiresAt":1791745200000,
                "scopes":["user:inference","user:profile"],"subscriptionType":"max"}}"#,
        )
        .unwrap();
        assert_eq!(login.expires_at_ms, Some(1_791_745_200_000));
        assert_eq!(login.subscription.as_deref(), Some("max"));
        let shown = format!("{login:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(parse_credentials("{}").is_none());
        assert!(parse_credentials(r#"{"claudeAiOauth":{"accessToken":""}}"#).is_none());
    }

    #[test]
    fn third_party_detection() {
        let vertex = |k: &str| (k == "CLAUDE_CODE_USE_VERTEX").then(|| "1".to_string());
        assert_eq!(third_party_provider(&vertex), Some("vertex"));
        let off = |k: &str| (k == "CLAUDE_CODE_USE_VERTEX").then(|| "0".to_string());
        assert_eq!(third_party_provider(&off), None);
        let none = |_: &str| None;
        assert_eq!(third_party_provider(&none), None);
    }
}
