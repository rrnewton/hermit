# Fixtures

## Published recordings of the Claude usage endpoint

These two are copied verbatim from replies that third parties recorded from the real
`GET /api/oauth/usage` endpoint and published. They are the closest available stand-in for a
first-hand recording: the machine this tool was developed on has no claude.ai login. To record
one yourself, run `AGENT_USAGE_RAW_DIR=/some/dir agent-usage poll --provider claude` on a machine
with a claude.ai login; the raw reply (usage figures, no credentials) lands in that directory.

- `claude-usage-published-a.json`: from `docs/oauth-usage-endpoint.md` in
  https://github.com/fullfran/claudeops-tui (a Max-plan reply with the per-model weekly windows and
  extra usage).
- `claude-usage-published-b.json`: from `claude/learnings/anthropic-oauth-usage.md` in
  https://github.com/anothersava/claude-code-common (minimal reply; fractional percentages and the
  `.000+00:00` timestamp form; the author notes the values are percentages, `0.37` meaning 0.37 %).

## Synthetic replies

These replies are synthetic. Their field names and shapes follow the response schemas that the
Claude Code and Codex CLIs bundle for the same endpoints (the claude.ai usage endpoint's legacy
named windows and its `limits[]` rows; Codex app-server's `account/rateLimits/read` result), with
made-up values. They contain no account data and no credentials.

- `claude-usage-limits.json`: a reply carrying server `limits[]` rows, including a per-model
  weekly row.
- `claude-usage-legacy.json`: a reply with only the legacy named windows and extra usage.
- `codex-ratelimits.jsonl`: a fake app-server conversation (one JSON-RPC message per line).
- `claude-transcript.jsonl`: Claude Code transcript lines: one message split over two lines, an
  API error without a status, and an exhausted-retries HTTP 429 in a gateway's per-user wording.
