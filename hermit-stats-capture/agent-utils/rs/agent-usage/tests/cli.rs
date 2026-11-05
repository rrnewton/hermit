//! End-to-end tests of the `agent-usage` binary with fake providers: a fake `curl` for the
//! claude.ai usage endpoint, a fake `codex app-server`, a fake `sqlite3`, and isolated homes.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_agent-usage");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const FAKE_TOKEN: &str = "fake-access-token-0123456789";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        let root =
            std::env::temp_dir().join(format!("agent-usage-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "claude", "codex", "cache", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Sandbox { root }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.root.join(p)
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.path("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn claude_login(&self, expires_ms: i64, scopes: &str) {
        std::fs::write(
            self.path("claude/.credentials.json"),
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"{FAKE_TOKEN}","refreshToken":"r","expiresAt":{expires_ms},"scopes":[{scopes}],"subscriptionType":"max"}}}}"#
            ),
        )
        .unwrap();
    }

    fn fake_curl(&self, fixture: &str, code: u16) {
        self.fake_curl_with_headers(fixture, code, "");
    }

    /// A fake curl answering with `fixture`; `headers` (CRLF-separated, without the status line)
    /// are emitted first, as `--dump-header -` would.
    fn fake_curl_with_headers(&self, fixture: &str, code: u16, headers: &str) {
        let log = self.path("curl-argv.txt");
        let stdin = self.path("curl-stdin.txt");
        let body = format!(
            "printf '%s\\n' \"$@\" > {log}\ncat > {stdin}\n{head}cat {FIXTURES}/{fixture}\nprintf '\\n{code}'",
            head = if headers.is_empty() {
                String::new()
            } else {
                format!("printf 'HTTP/2 {code}\\r\\n{headers}\\r\\n\\r\\n'\n")
            },
            log = log.display(),
            stdin = stdin.display()
        );
        self.script("curl", &body);
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> (i32, String, String) {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path("home"))
            .env("CLAUDE_CONFIG_DIR", self.path("claude"))
            .env("CODEX_HOME", self.path("codex"))
            .env("AGENT_USAGE_DIR", self.path("cache"))
            .env("AGENT_USAGE_CURL", self.path("bin/curl"))
            .env("AGENT_USAGE_SQLITE3", self.path("bin/sqlite3"))
            .env("TZ", "UTC");
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn status_json(&self, args: &[&str], extra: &[(&str, &str)]) -> Value {
        let mut all = vec!["status", "--json"];
        all.extend_from_slice(args);
        let (code, out, err) = self.run(&all, extra);
        assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn provider<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == name)
        .unwrap_or_else(|| panic!("no {name} in {report}"))
}

#[test]
fn claude_subscription_reads_meters_and_keeps_token_off_argv() {
    let sb = Sandbox::new("claude-ok");
    sb.claude_login(
        (now_secs() + 3_600) * 1000,
        r#""user:inference","user:profile""#,
    );
    sb.fake_curl("claude-usage-limits.json", 200);
    let report = sb.status_json(&["--provider", "claude", "--no-tokens"], &[]);
    let p = provider(&report, "claude");
    assert_eq!(p["status"], "ok");
    assert_eq!(p["plan"], "max");
    assert_eq!(p["fresh"], true);
    let labels: Vec<_> = p["meters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "Current session",
            "Current week (all models)",
            "Current week (Fable)"
        ]
    );
    assert_eq!(p["meters"][2]["used_pct"], 5.0);

    let argv = std::fs::read_to_string(sb.path("curl-argv.txt")).unwrap();
    let stdin = std::fs::read_to_string(sb.path("curl-stdin.txt")).unwrap();
    assert!(!argv.contains(FAKE_TOKEN), "token on argv: {argv}");
    assert!(
        argv.contains("https://api.anthropic.com/api/oauth/usage"),
        "{argv}"
    );
    assert!(
        stdin.contains(&format!("Authorization: Bearer {FAKE_TOKEN}")),
        "{stdin}"
    );
    assert!(
        stdin.contains("anthropic-beta: oauth-2025-04-20"),
        "{stdin}"
    );

    // The token never reaches the cache directory.
    for entry in std::fs::read_dir(sb.path("cache")).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default();
        assert!(!text.contains(FAKE_TOKEN));
    }

    // A second call within --max-age is served from the history without calling curl.
    std::fs::remove_file(sb.path("curl-argv.txt")).unwrap();
    let report = sb.status_json(&["--provider", "claude", "--no-tokens"], &[]);
    assert_eq!(provider(&report, "claude")["fresh"], false);
    assert!(!sb.path("curl-argv.txt").exists());
}

#[test]
fn claude_failures_are_reported_not_hidden() {
    let sb = Sandbox::new("claude-bad");
    // No login, cloud provider selected: unavailable, curl never runs.
    sb.fake_curl("claude-usage-limits.json", 200);
    let r = sb.status_json(
        &["--provider", "claude", "--no-tokens"],
        &[("CLAUDE_CODE_USE_VERTEX", "1")],
    );
    let p = provider(&r, "claude");
    assert_eq!(p["status"], "unavailable");
    assert!(p["detail"].as_str().unwrap().contains("vertex"));
    assert!(!sb.path("curl-argv.txt").exists());

    // Expired login: error, no request.
    sb.claude_login((now_secs() - 600) * 1000, r#""user:profile""#);
    let r = sb.status_json(
        &["--provider", "claude", "--no-tokens", "--max-age", "0"],
        &[],
    );
    let p = provider(&r, "claude");
    assert_eq!(p["status"], "error");
    assert!(p["detail"].as_str().unwrap().contains("expired"), "{p}");
    assert!(!sb.path("curl-argv.txt").exists());

    // Inference-only token: unavailable.
    sb.claude_login((now_secs() + 600) * 1000, r#""user:inference""#);
    let r = sb.status_json(
        &["--provider", "claude", "--no-tokens", "--max-age", "0"],
        &[],
    );
    assert_eq!(provider(&r, "claude")["status"], "unavailable");

    // HTTP error: error with the status code.
    sb.claude_login((now_secs() + 600) * 1000, r#""user:profile""#);
    sb.fake_curl("claude-usage-legacy.json", 429);
    let r = sb.status_json(
        &["--provider", "claude", "--no-tokens", "--max-age", "0"],
        &[],
    );
    let p = provider(&r, "claude");
    assert_eq!(p["status"], "error");
    assert!(p["detail"].as_str().unwrap().contains("HTTP 429"), "{p}");
}

#[test]
fn codex_app_server_and_thread_totals() {
    let sb = Sandbox::new("codex");
    std::fs::write(
        sb.path("codex/auth.json"),
        r#"{"tokens":{"access_token":"x","account_id":"y"}}"#,
    )
    .unwrap();
    std::fs::write(sb.path("codex/state_5.sqlite"), "").unwrap();
    // The fake app-server answers only after reading the three requests, then exits at end of
    // input as the real one does.
    let fake = sb.script(
        "codex",
        &format!(
            "[ \"$1\" = app-server ] || exit 2\nhead -n 3 > /dev/null\ncat {FIXTURES}/codex-ratelimits.jsonl\ncat > /dev/null"
        ),
    );
    sb.script("sqlite3", "echo '123456 7'");
    let fake = fake.to_str().unwrap();
    let r = sb.status_json(&["--provider", "codex"], &[("AGENT_USAGE_CODEX_BIN", fake)]);
    let p = provider(&r, "codex");
    assert_eq!(p["status"], "ok", "{p}");
    assert_eq!(p["plan"], "plus");
    let ids: Vec<_> = p["meters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["codex:5h", "codex:weekly", "codex_other:weekly"]);
    let (_, hist, _) = sb.run(&["history", "--json", "--provider", "codex"], &[]);
    let sample: Value = serde_json::from_str(hist.lines().next().unwrap()).unwrap();
    assert_eq!(sample["tokens_cumulative"]["total"], 123_456);
    assert!(
        sample["elapsed_ms"].as_u64().unwrap() < 1_500,
        "app-server did not stop at end of input"
    );

    // No ChatGPT login: the app-server is not started at all.
    std::fs::remove_file(sb.path("codex/auth.json")).unwrap();
    sb.script("codex", "touch \"$0.ran\"");
    let r = sb.status_json(
        &["--provider", "codex", "--max-age", "0"],
        &[("AGENT_USAGE_CODEX_BIN", fake)],
    );
    assert_eq!(provider(&r, "codex")["status"], "unavailable");
    assert!(!Path::new(&format!("{fake}.ran")).exists());
}

#[test]
fn burn_rates_from_seeded_history() {
    let sb = Sandbox::new("burn");
    let now = now_secs();
    let reset = now + 7_200;
    let mut lines = String::new();
    // Every 15 minutes for 3 hours: session +2 points each step; codex counter +1M tokens.
    for i in 0..=12i64 {
        let ts = now - (12 - i) * 900;
        lines.push_str(&format!(
            r#"{{"v":1,"ts":{ts},"provider":"claude","status":"ok","source":"oauth-usage","meters":[{{"id":"session","label":"Current session","used_pct":{used},"resets_at":{reset},"window_mins":300}}]}}"#,
            used = 10 + 2 * i
        ));
        lines.push('\n');
        lines.push_str(&format!(
            r#"{{"v":1,"ts":{ts},"provider":"codex","status":"unavailable","tokens_cumulative":{{"total":{t}}}}}"#,
            t = 1_000_000 * i
        ));
        lines.push('\n');
    }
    std::fs::write(sb.path("cache/history.jsonl"), lines).unwrap();
    let r = sb.status_json(&["--cached", "--no-tokens"], &[]);
    let m = &provider(&r, "claude")["meters"][0];
    assert_eq!(m["used_pct"], 34.0);
    let burn = |w: &str| {
        m["burn"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["window"] == w)
            .cloned()
    };
    assert_eq!(burn("15m").unwrap()["used"], 2.0);
    assert_eq!(burn("1h").unwrap()["used"], 8.0);
    assert_eq!(burn("3h").unwrap()["used"], 24.0);
    assert_eq!(burn("24h").unwrap()["covered_secs"], 10_800);
    assert_eq!(m["projection"]["basis"], "1h");
    assert_eq!(m["projection"]["per_hour"], 8.0);
    // 66 points left at 8/h = 8.25 h, after the reset in 2 h.
    assert_eq!(m["projection"]["full_before_reset"], false);
    let tokens = &provider(&r, "codex")["tokens"];
    assert_eq!(tokens["source"], "thread-totals");
    assert_eq!(tokens["windows"][1]["window"], "1h");
    assert_eq!(tokens["windows"][1]["tokens"]["total"], 4_000_000);

    let (code, text, _) = sb.run(&["--cached", "--no-tokens"], &[]);
    assert_eq!(code, 0);
    assert!(text.contains("Current session"), "{text}");
    assert!(text.contains("15m +2.0, 1h +8.0, 3h +24.0"), "{text}");
}

#[test]
fn daemon_once_status_and_usage_errors() {
    let sb = Sandbox::new("daemon");
    let (code, _, err) = sb.run(&["daemon", "--once", "--interval", "60"], &[]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("poll"), "{err}");
    let (_, out, _) = sb.run(&["daemon", "status", "--json"], &[]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["running"], false);
    assert_eq!(v["samples"], 2);
    let (code, _, _) = sb.run(&["--provider", "nope"], &[]);
    assert_eq!(code, 64);
    let (code, out, _) = sb.run(&["--help"], &[]);
    assert_eq!(code, 0);
    assert!(out.contains("USAGE"));
    let (code, out, _) = sb.run(&["quickstart"], &[]);
    assert_eq!(code, 0);
    assert!(out.contains("agent-usage"));
}

#[test]
fn claude_429_backs_off_and_keeps_the_last_good_reading() {
    let sb = Sandbox::new("claude-429");
    sb.claude_login((now_secs() + 3_600) * 1000, r#""user:profile""#);
    sb.fake_curl("claude-usage-published-a.json", 200);
    let floor = [("AGENT_USAGE_CLAUDE_MIN_INTERVAL", "0")];
    let r = sb.status_json(&["--provider", "claude", "--no-tokens"], &floor);
    assert_eq!(provider(&r, "claude")["status"], "ok");

    sb.fake_curl_with_headers("claude-usage-legacy.json", 429, "retry-after: 832");
    let r = sb.status_json(
        &["--provider", "claude", "--no-tokens", "--max-age", "0"],
        &floor,
    );
    let p = provider(&r, "claude");
    assert_eq!(p["status"], "error");
    assert!(p["detail"].as_str().unwrap().contains("13m"), "{p}");
    // The figures stay visible, from the last good reading.
    assert_eq!(p["meters"][0]["used_pct"], 6.0);
    assert!(p["meters_sampled_at"].is_i64());
    let until = p["backoff_until"].as_i64().unwrap();
    assert!((until - now_secs() - 832).abs() < 30, "{until}");

    // During the back-off nothing is sent, even when asked to poll.
    std::fs::remove_file(sb.path("curl-argv.txt")).unwrap();
    let r = sb.status_json(
        &["--provider", "claude", "--no-tokens", "--max-age", "0"],
        &floor,
    );
    assert_eq!(provider(&r, "claude")["fresh"], false);
    assert!(!sb.path("curl-argv.txt").exists());
    let (_, line, _) = sb.run(
        &["--line", "--provider", "claude", "--no-tokens", "--cached"],
        &[],
    );
    assert!(line.starts_with("claude[max] STALE("), "{line}");
}

#[test]
fn claude_endpoint_floor_holds_even_with_max_age_zero() {
    let sb = Sandbox::new("claude-floor");
    sb.claude_login((now_secs() + 3_600) * 1000, r#""user:profile""#);
    sb.fake_curl("claude-usage-limits.json", 200);
    let raw = sb.path("raw");
    sb.status_json(
        &["--provider", "claude", "--no-tokens"],
        &[("AGENT_USAGE_RAW_DIR", raw.to_str().unwrap())],
    );
    // The raw reply was recorded for fixtures, and holds no credential.
    let recorded: Vec<_> = std::fs::read_dir(&raw)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(recorded.len(), 1);
    let text = std::fs::read_to_string(&recorded[0]).unwrap();
    assert!(text.contains("weekly_scoped") && !text.contains(FAKE_TOKEN));
    std::fs::remove_file(sb.path("curl-argv.txt")).unwrap();
    let r = sb.status_json(
        &["--provider", "claude", "--no-tokens", "--max-age", "0"],
        &[],
    );
    assert_eq!(provider(&r, "claude")["fresh"], false);
    assert!(!sb.path("curl-argv.txt").exists());
}

#[test]
fn rate_limit_evidence_from_transcripts_and_codex_logs() {
    let sb = Sandbox::new("limits");
    let now = now_secs();
    let iso = |t: i64| {
        let out = Command::new("date")
            .args(["-u", "-d", &format!("@{t}"), "+%Y-%m-%dT%H:%M:%SZ"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    // Claude: two requests and one exhausted-retries 429 from a gateway, 5 minutes ago.
    let proj = sb.path("claude/projects/p");
    std::fs::create_dir_all(&proj).unwrap();
    let mut lines = String::new();
    for (i, id) in ["m1", "m2"].iter().enumerate() {
        lines.push_str(&format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"{id}","model":"m","usage":{{"input_tokens":1,"output_tokens":2}}}}}}"#,
            iso(now - 500 + i as i64 * 60)
        ));
        lines.push('\n');
    }
    lines.push_str(&format!(
        r#"{{"type":"assistant","timestamp":"{}","isApiErrorMessage":true,"apiErrorStatus":429,"message":{{"id":"x","model":"<synthetic>","content":[{{"type":"text","text":"API Error: Request rejected (429) · rate limit exceeded for u-1::w-60s: 436/425 in 60s"}}],"usage":{{"input_tokens":0,"output_tokens":0}}}}}}"#,
        iso(now - 300)
    ));
    lines.push('\n');
    std::fs::write(proj.join("s.jsonl"), lines).unwrap();
    // Codex: a retried 429 and a retried 404 in its log database.
    std::fs::write(sb.path("codex/logs_2.sqlite"), "").unwrap();
    std::fs::write(sb.path("codex/state_5.sqlite"), "").unwrap();
    let rows = format!(
        r#"[{{"id":10,"ts":-1,"b":""}},{{"id":8,"ts":{},"b":"retrying sampling request (1/5 in 200ms)... sampling_error=unexpected status 429 Too Many Requests: rate limit exceeded for u-1::w-600s: 901/900 in 600s"}},{{"id":9,"ts":{},"b":"sampling_error=unexpected status 404 Not Found: no such model"}}]"#,
        now - 120,
        now - 60
    );
    std::fs::write(sb.path("rows.json"), rows).unwrap();
    sb.script(
        "sqlite3",
        &format!(
            "case \"$*\" in *-json*) cat {} ;; *) echo '10 1' ;; esac",
            sb.path("rows.json").display()
        ),
    );
    let r = sb.status_json(&[], &[]);
    let cl = &provider(&r, "claude")["limits"];
    assert_eq!(cl["source"], "transcripts");
    assert_eq!(cl["includes_retried"], false);
    assert_eq!(cl["windows"][0]["window"], "15m");
    assert_eq!(cl["windows"][0]["rate_limited"], 1);
    assert_eq!(cl["last_rate_limit"]["kind"], "gateway-user");
    assert_eq!(
        cl["last_rate_limit"]["count"],
        serde_json::json!([436, 425, 60])
    );
    assert_eq!(cl["requests_10m"], 2);
    assert_eq!(cl["peak_requests_per_minute_1h"], 1);
    let cx = &provider(&r, "codex")["limits"];
    assert_eq!(cx["source"], "codex-logs");
    assert_eq!(cx["windows"][0]["rate_limited"], 1);
    assert_eq!(cx["windows"][0]["other_errors"], 1);
    assert_eq!(
        cx["last_rate_limit"]["count"],
        serde_json::json!([901, 900, 600])
    );
    let (_, text, _) = sb.run(&["--cached"], &[]);
    assert!(
        text.contains("gateway per-user limit, 436/425 requests in 60s"),
        "{text}"
    );
    let (_, line, _) = sb.run(&["--line", "--cached"], &[]);
    assert!(
        line.trim_end().ends_with("429s 24h: claude 1, codex 1"),
        "{line}"
    );
}
