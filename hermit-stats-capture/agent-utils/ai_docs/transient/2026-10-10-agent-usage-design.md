# agent-usage: design note (2026-10-10)

## Request

The owner wanted agents to know their own token budgets the way a person sees them in Claude
Code's `/status` → Usage tab ("Current session 8% used, resets 8:10am; Current week (all models)
22% used; Current week (Fable) 5% used"), plus burn rates over 15 minutes, 1 hour, 3 hours and
24 hours. Polling had to be cheap: no harness started in a temporary directory to type
`/status`, no tokens spent. It should use structured files or API calls where they exist, fall
back to TUI scraping only if it must, offer a 15-minute background daemon with history in a
standard cache directory, and come with measurements of wall time, CPU, memory and disk, plus
the cost of keeping a dummy session open compared with starting one per poll. Rust first, with a
Python port to follow once the tool has been battle tested.

## Findings

**Prior art.** One existing approach reads Claude by running `claude -p --model haiku` with a
one-word prompt and capturing the `rate_limit_event` the CLI streams after the first API
response. That costs one Haiku call per reading. It reads Codex through `codex app-server`'s
`account/rateLimits/read`, which is free. It keeps no history and computes no burn rate.

**Claude Code** (2.1.292). The Usage tab calls `GET /api/oauth/usage` on the Anthropic API host
with the claude.ai OAuth token and `anthropic-beta: oauth-2025-04-20`. It needs the
`user:profile` scope. The reply has legacy named windows (`five_hour`, `seven_day`,
`seven_day_sonnet`, `seven_day_opus`, `seven_day_oauth_apps`, each `{utilization 0-100,
resets_at ISO}`), server-defined `limits[]` rows (`kind`, `group`, `percent`, `resets_at`,
`scope.model.display_name`, `severity`, `is_active`), and `extra_usage`. These shapes come from
the zod schemas bundled in the CLI. The login is in `.credentials.json` (`claudeAiOauth`) or the
macOS keychain. The bundle documents `rate_limits_available: false` for API-key, Bedrock and
Vertex sessions. On the measurement host, whose Claude Code runs on a cloud provider through a gateway,
the Usage tab showed only session cost (checked in a throwaway tmux session). Plan windows exist
only for claude.ai logins. The same data reaches the status-line command's JSON and the SDK
`get_usage` control request, but both need a live session.

**Codex** (0.159.3). `account/rateLimits/read` returns `rateLimits` (`primary` and `secondary`,
each `{usedPercent, windowDurationMins, resetsAt}`, plus `planType` and `limitId`),
`rateLimitsByLimitId` for extra buckets, and `ordinaryUsageAllowed`. The shapes are from the
open-source app-server protocol. For ChatGPT logins the server calls the backend's
`/wham/usage`. On the measurement host (gateway login) the reply was "codex account
authentication required to read rate limits", and session rollouts carry `rate_limits.primary =
null`.

**Local tokens.** Claude transcripts carry per-message usage. On the measurement host, 52
transcript files were touched in 24 hours, 641 MB in total, holding 10.3-10.8 k requests and
about 4.4 G tokens (almost all cache reads). A cold Python scan of all of it took 1.5 s, so the
tool indexes transcripts incrementally. Codex keeps per-thread totals in `state_<n>.sqlite`
(`threads.tokens_used`); summing them takes about 3 ms.

**TUI scraping** is not needed: both CLIs have a structured source that needs no tokens. It is
also expensive; see the crate README for the measured cost of a dummy session.

## Decisions

- Name `agent-usage`, crate `rs/agent-usage`, a hand-parsed CLI in the style of `gh-paced`, and
  no HTTP or TLS dependency: requests go through `curl`, with headers passed on stdin as
  `--config -` so the bearer token never appears on a command line.
- Never refresh OAuth tokens. Refreshing rotates the refresh token Claude Code holds, and could
  log the user's own sessions out. An expired token is reported as `error`.
- Report "no plan limits here" (`unavailable`) separately from "could not read" (`error`). A
  partially parseable reply is an error: unknown headroom must not read as zero.
- Skip the Codex app-server, a 410 MB process, when no ChatGPT login exists.
- History: append-only JSONL, one line per provider per reading, under an advisory lock that
  also stops concurrent callers from polling twice. `status` reuses a sample younger than 120 s.
- Burn: the sum of rises between consecutive samples, with reset detection (a sample taken past
  the previous reset time, or a reset time that moved forward by more than 2 minutes), linear
  proration of a segment that straddles the window start, and the covered span reported beside
  every figure.
- Daemon: single instance per cache directory (`flock`), a 15-minute default interval,
  `--detach` (setsid, log file), `--once` for cron.

## Departures from the request

- The owner's example (session, weekly and per-model percentages) cannot be produced on hosts
  whose harnesses use a gateway or cloud-provider login, because those providers have no plan
  windows. The tool says so, and shows local token windows instead.
- The Claude network request could not be timed: the measurement host has no claude.ai login,
  and an unauthenticated probe of the endpoint was declined. The end-to-end tests cover that path
  with a fake `curl`.
- "Keep a dummy session open" was measured as a cost comparison only. The tool does not need it.
