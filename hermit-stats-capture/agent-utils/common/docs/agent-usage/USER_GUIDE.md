# agent-usage user guide

`agent-usage` tells an agent (or a person) how much of each coding-agent subscription's plan
windows is used, when each window resets, how fast it is being spent, and how many tokens this
host has used recently. It covers the Claude Code and Codex CLIs.

## Which hosts get percentages

Plan percentages and reset times exist only for subscription logins. Everything else gets local
burn only, and the report says why instead of showing an error.

| How the harness is logged in | Plan windows (% used, reset) | Local burn |
| --- | --- | --- |
| Claude Code, claude.ai Pro / Max / Team / Enterprise interactive login (`/login`) | yes: session, weekly, per-model weekly, extra usage | yes (transcripts) |
| Claude Code, `claude setup-token` long-lived token (inference-only scope) | no (`unavailable`) | yes |
| Claude Code on an API key, Bedrock, Vertex, Foundry or a gateway | no (`unavailable`; there are no plan limits) | yes |
| Codex, ChatGPT login | yes: 5-hour and weekly windows, extra buckets | yes (thread totals) |
| Codex on an API key or a gateway | no (`unavailable`) | yes |

In practice: personal machines logged in to claude.ai or ChatGPT get percentages; shared
development servers whose harnesses run through a provider gateway get local burn only.

## Commands

```text
agent-usage [status] [--json | --line] [--provider P] [--max-age DUR | --cached] [--no-tokens]
agent-usage poll [--json] [--provider P]
agent-usage daemon [--interval DUR] [--once] [--detach]
agent-usage daemon status [--json] | stop
agent-usage history [--provider P] [--since DUR] [--json]
agent-usage path
agent-usage quickstart | userguide | help | --version
```

- `status` (the default) prints the report. For each selected provider it reuses the newest
  sample in the history when that sample is younger than `--max-age` (default 120 s), and
  otherwise takes a new reading first. `--max-age 0` always reads; `--cached` never does.
  `--no-tokens` skips the Claude transcript scan. `--line` prints the gist on one line, for a
  status line or an agent that wants it in few tokens:
  `claude[max] 5h 34% +8.0/h reset 1h25m, wk 22% +1.0/h reset 1d03h, wk:Fable 5% +0.4/h reset 1d03h | codex no-plan | 1h tokens: claude 304M (977 req), codex 52M`.
  `5h` is the session window, `wk` the weekly one, `+N/h` the projection's rate in points per
  hour, and `FULL-BEFORE-RESET` marks a window that would fill before it resets; `no-plan` is
  `unavailable`, `UNKNOWN` is `error` with no earlier good reading, and `STALE(<age>)` precedes the
  last good figures when the newest reading failed.
- `poll` reads every selected provider now, appends the samples, and prints one line each.
- `daemon` polls every `--interval` (default 15 minutes, at least 60 s) in the foreground until
  SIGTERM, SIGINT or SIGHUP. Only one daemon runs per cache directory; a second exits with 75.
  `--once` polls a single time and exits, for cron or a systemd timer. `--detach` starts the
  daemon in a new session with its output in `daemon.log`, prints its pid, and returns.
  `daemon status` reports whether one runs and how fresh the history is; `daemon stop` sends it
  SIGTERM.
- `history` prints stored samples from the last `--since` (default 24 h).
- `path` prints the cache directory.

`--provider` takes `claude`, `codex` or `all` (default) and may be repeated. Durations are
seconds, or a number followed by `s`, `m`, `h` or `d`.

## Sources

### Claude Code

Plan windows come from the claude.ai usage endpoint, `GET https://api.anthropic.com/api/oauth/usage`,
which is what the `/status` Usage tab reads. agent-usage sends the stored OAuth access token and
the `anthropic-beta: oauth-2025-04-20` header through `curl`. The token is passed to curl on its
standard input (`--config -`), never on the command line, and is never written to the cache or
printed.

The login is read from `$CLAUDE_CONFIG_DIR/.credentials.json` (default `~/.claude`), key
`claudeAiOauth`; on macOS, from the login keychain item `Claude Code-credentials` when the file
holds no login. The reply's `limits[]` rows become meters with ids `session`, `weekly_all` and
`weekly:<model>` (for example `weekly:Fable`), labelled as the Usage tab labels them. Replies
without those rows fall back to the named windows `five_hour`, `seven_day`, `seven_day_sonnet`,
`seven_day_opus` and `seven_day_oauth_apps`. Enabled extra usage appears as `extra_usage`.

| Situation | Status | What happens |
| --- | --- | --- |
| claude.ai login with the `user:profile` scope | `ok` | One GET, about one network round trip. |
| No claude.ai login (API key, Bedrock, Vertex, Foundry, a gateway) | `unavailable` | Nothing is sent. These have no plan limits; the detail names the provider when `CLAUDE_CODE_USE_*` says which. |
| Login from `claude setup-token` (inference-only scope) | `unavailable` | The endpoint would refuse it. |
| Stored access token expired | `error` | Nothing is sent. agent-usage does not refresh tokens, because refreshing rotates the refresh token that Claude Code itself holds; any running Claude Code session refreshes it. |
| HTTP 429 from the usage endpoint | `error` | Nothing more is sent until the `Retry-After` time (delta seconds, capped at 6 h; 10 minutes when absent or unusable), and the report keeps showing the last good reading, marked as such (`STALE(...)` in `--line`). |
| HTTP error, malformed reply, or a window whose percentage is missing | `error` | The reply is not partially trusted: unknown headroom never reads as zero. The last good reading is shown, marked as such. |

The usage endpoint is itself rate-limited, per organisation, and Claude Code's own Usage tab
polls it too. Third-party reports show 429s with `Retry-After` values of 832 s and 2,449 s under
frequent polling and a 10-minute cadence as stable. So agent-usage never asks it more often than
every 300 s (`AGENT_USAGE_CLAUDE_MIN_INTERVAL`), whatever `--max-age` says, including
`--max-age 0` and the daemon. Readings that never reach the endpoint (no login, expired token) do
not count against that floor.

### Codex

Plan windows come from Codex's app-server: agent-usage starts `codex app-server`, sends
`initialize`, `initialized` and `account/rateLimits/read` (with `excludeResetCreditDetails`),
reads the reply, closes the server's input, and stops its process group (SIGTERM after 2 s,
SIGKILL after 3 s, for servers that do not exit at end of input). The primary and secondary
windows of each rate-limit bucket become meters such as `codex:5h` and `codex:weekly`; extra
buckets (`rateLimitsByLimitId`) add their own, labelled with the bucket's name.

The app-server is started only when `$CODEX_HOME/auth.json` (default `~/.codex`) holds ChatGPT
tokens or `config.toml` keeps credentials in a keyring. With an API key or a gateway provider
there are no plan windows, and the status is `unavailable` without starting a process. Set
`AGENT_USAGE_CODEX_PROBE=always` to start it anyway; the server's "authentication required"
reply is also reported as `unavailable`.

### Local tokens

**Claude Code transcripts.** Every assistant message in `$CLAUDE_CONFIG_DIR/projects/**/*.jsonl`
(subagent transcripts included) carries its token usage. A message written as several lines
(one per content block) repeats the same id and is counted once; synthetic error messages are
skipped. The index (`claude-transcripts.json`) keeps each file's byte offset and per-minute
buckets for 25 hours, so each call reads only bytes appended since the previous one. A file
seen for the first time is entered at its first line inside those 25 hours, found by binary
search on the line timestamps. Only complete lines are consumed, so a line still being written
is read next time. The report gives requests and tokens (output, cache read, cache write,
uncached input, and their total) for each window. This counts only this host's transcripts.

**Codex thread totals.** `threads.tokens_used` in the newest `$CODEX_HOME/state_<n>.sqlite`,
summed through the `sqlite3` command (read-only), is a cumulative counter stored with every
Codex sample. Its burn over each window comes from the history, like a plan meter's, so it needs
two samples at least a minute apart.

### Rate limits (HTTP 429)

Hosts whose harnesses run through a provider gateway have no plan windows; their real limits are
request rates: the gateway's per-user limit (requests per 60 s and per 600 s, counted per user
and upstream across every host) and the model provider's quota behind it. Neither can be read in
advance, so the report shows the evidence that is on disk:

- **Claude Code** writes an API error into the transcript only when its retries are exhausted
  (`isApiErrorMessage`, `apiErrorStatus`). A 429 that a retry got past leaves no local trace, so
  Claude's counts are a floor. The same index that counts tokens counts these per minute.
- **Codex** logs every retried request (`codex_core::responses_retry`, `unexpected status <code>`)
  to its log database, `$CODEX_HOME/logs_<n>.sqlite`, so its counts include 429s a retry got past.
  `codex-logs.json` in the cache keeps the last 24 hours and the highest row id read, so each call
  reads only rows added since (by primary key) after one indexed read of the last 24 hours.

Each 429's text is classified: `gateway-user` when it names a counted key and `count/max in
<window>s` (shown as, for example, `gateway per-user limit, 436/425 requests in 60s`);
`provider-quota` for `RESOURCE_EXHAUSTED` or `Quota exceeded` replies, which are shared by
everyone behind the same provider project; `other` otherwise. For Claude the report also gives
this host's requests in the last 10 minutes and in its busiest minute of the last hour, from the
per-minute buckets (minute granularity: "the last 10 minutes" covers the buckets that start
inside it). Other hosts' requests count against the same gateway limit and are not visible here.

Example from a host on a gateway:

```text
  rate limits (HTTP 429; only those that exhausted Claude Code's retries; retried ones leave no local trace):
    15m 0, 1h 0, 3h 0, 24h 2 (other API errors in 24h: 0)
    last 429: 11h48m ago, provider quota, shared beyond this user: API Error: Request rejected (429) · [{"error":{"code":429,"message":"Resource exhausted. ...
    requests from this host: 168 in the last 10 min, busiest minute of the last hour 33
```

`--line` ends with `429s 24h: claude 2, codex 0`.

## Burn rates

For every plan meter, and for the Codex token counter, the report gives the increase over the
last 15 minutes, 1 hour, 3 hours and 24 hours:

- The increase is the sum of rises between consecutive samples. A fall counts as zero.
- If a sample was taken after the previous sample's reset time, or the reset time moved forward
  by more than two minutes, the window reset in between. The rise is then the new value itself,
  which is what was used since the reset. Whatever was used between the older sample and the
  reset is not seen.
- A segment that starts before the window is prorated linearly by how much of it lies inside.
- `covered_secs` is how much of the window the samples actually span, and `per_hour` divides
  by that span, not by the nominal window. In text output a window covered less than 90 % says
  so, for example `+3.0 (40m seen)`.
- A window needs two usable samples covering at least a minute; otherwise it is absent (`n/a`).

The projection uses the 1 h rate (else 3 h, 15 m, 24 h). It gives the time until 100 % at that
rate, and whether that comes before the window's reset (`BEFORE the reset` in text,
`full_before_reset: true` in JSON). The rate is idle when it is zero.

With samples every 15 minutes, the 15 m window rests on a single interval. Poll more often
(`--interval 5m`) when that matters.

## JSON output

`status --json` prints one object:

```json
{
  "now": 1791631000, "now_iso": "2026-10-10T11:16:40Z",
  "providers": [
    {
      "provider": "claude", "status": "ok", "source": "oauth-usage", "plan": "max",
      "sampled_at": 1791631000, "age_secs": 0, "fresh": true, "history_samples": 13,
      "meters": [
        {
          "id": "session", "label": "Current session", "used_pct": 34.0, "remaining_pct": 66.0,
          "resets_at": 1791638200, "resets_at_iso": "2026-10-10T13:16:40Z", "resets_in_secs": 7200,
          "burn": [
            {"window": "15m", "window_secs": 900, "used": 2.0, "covered_secs": 900,
             "per_hour": 8.0, "samples": 1, "resets": 0}
          ],
          "projection": {"basis": "1h", "per_hour": 8.0, "secs_to_full": 29700,
                         "full_before_reset": false}
        }
      ],
      "limits": {"source": "transcripts", "includes_retried": false,
                 "windows": [{"window": "15m", "rate_limited": 0, "other_errors": 0}],
                 "last_rate_limit": {"ts": 1791591054, "kind": "provider-quota",
                                     "detail": "API Error: Request rejected (429) ..."},
                 "requests_10m": 168, "peak_requests_per_minute_1h": 33},
      "tokens": {"source": "transcripts", "windows": [
        {"window": "15m", "covered_secs": 900, "tokens": {"requests": 259, "input": 518,
         "output": 216000, "cache_read": 69000000, "cache_write": 1900000, "total": 71116518}}
      ]}
    }
  ]
}
```

`status` is one of `ok`, `unavailable` and `error`; `detail` explains the last two. When the newest
reading failed, `meters` come from the newest good reading and `meters_sampled_at` gives its time;
`backoff_until` is set while a `Retry-After` back-off runs. A provider
with no sample (`--cached` with an empty history) is left out.

`poll --json` and `history --json` print one sample per line, as stored in `history.jsonl`:
`v`, `ts`, `provider`, `status`, `source`, `detail`, `plan`, `meters` (id, label, `used_pct`,
`resets_at`, `window_mins`), `tokens_cumulative` (Codex) and `elapsed_ms`.

## Files and environment

| Path or variable | Meaning |
| --- | --- |
| `$AGENT_USAGE_DIR`, else `$XDG_CACHE_HOME/agent-usage`, else `~/.cache/agent-usage` | Cache directory (mode 0700). |
| `history.jsonl` | Append-only samples (mode 0600). Only the newest 32 MiB are read, about a year of 15-minute samples. |
| `claude-transcripts.json` | Transcript index (tokens and API errors per minute). Safe to delete; it is rebuilt from the last 25 hours. |
| `codex-logs.json` | Codex's retried requests of the last 24 hours and the highest log row read. Safe to delete. |
| `lock` | Serialises polls and index updates, so concurrent callers do not all poll. |
| `daemon.lock`, `daemon.pid`, `daemon.log` | The daemon's single-instance lock, pid and log. |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | Harness homes, as the CLIs themselves use them. |
| `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`, `ANTHROPIC_API_KEY` | Only used to explain why Claude has no plan limits. |
| `AGENT_USAGE_CLAUDE_USAGE_URL` | Override the usage endpoint (tests). |
| `AGENT_USAGE_CURL`, `AGENT_USAGE_SQLITE3`, `AGENT_USAGE_CODEX_BIN` | Override the `curl`, `sqlite3` and `codex` commands (tests, unusual installs). |
| `AGENT_USAGE_CODEX_PROBE=always` | Start the Codex app-server even without a ChatGPT login. |
| `AGENT_USAGE_CLAUDE_MIN_INTERVAL` | Seconds between requests to the Claude usage endpoint (default 300). |
| `AGENT_USAGE_RAW_DIR` | Write each raw provider reply there (`claude-usage-<ts>.json`, `codex-ratelimits-<ts>.json`), to record real replies as test fixtures. Replies carry usage figures, not credentials. |

## Exit status

`0` when a report was printed, including when a provider's status is `unavailable` or `error`
(read the `status` field); `64` for bad arguments; `70` when the cache directory cannot be used;
`75` when a daemon is already running.

## Cost

Measured on a Linux development host, 2026-10-10 (see the crate README for the method):

| Operation | Wall | CPU | Peak RSS |
| --- | --- | --- | --- |
| `status`, answer from the history (sample fresh, indexes warm) | ~14 ms | ~12 ms | ~2.5 MB |
| `status --cached --no-tokens` | ~1 ms | ~1 ms | ~2.5 MB |
| `status`, cold indexes (52 transcripts, 641 MB touched in 24 h; Codex logs) | ~0.17 s | ~0.17 s | ~2.5 MB |
| Claude reading | one `curl` process (~6 ms) plus one HTTPS request | | |
| Codex reading (app-server start, reply, stop) | ~0.6-0.8 s | ~1.2-1.4 s | ~410 MB (the app-server) |

The alternative, keeping a Claude Code session open to scrape `/status`, costs a process tree
of over twenty processes and several gigabytes of resident memory on a host with many MCP
servers configured; see the crate README.
