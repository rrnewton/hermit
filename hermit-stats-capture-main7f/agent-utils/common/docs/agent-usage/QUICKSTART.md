# agent-usage quickstart

**Problem.** Claude Code and Codex agents cannot see their own budgets. A person types
`/status` and pages to the Usage tab ("Current session 8% used, resets 8:10am; Current week
22% used, resets Oct 11"), but an agent has no cheap way to read that, and no idea how fast it
is spending. Starting a throwaway harness to type `/status` costs a full start-up, and probing
with a model call costs tokens.

**What agent-usage does.** One command prints, for each harness:

- every plan window the provider reports (Claude: current session, current week, per-model
  weeks such as Fable; Codex: the 5-hour and weekly windows), its percentage used and its reset
  time;
- the burn over the last 15 minutes, 1 hour, 3 hours and 24 hours, and whether the window
  would fill before it resets at the current rate;
- local token use from Claude Code transcripts and Codex's thread totals, which also works where
  there are no plan limits (API keys, cloud providers, gateways);
- rate-limit evidence for gateway hosts, whose real limit is a request rate: HTTP 429s from
  Claude transcripts and Codex's logs, classified as a gateway's per-user limit or a provider
  quota, and this host's recent request rate.

It reads the same structured sources the CLIs' own `/status` screens use: Claude's claude.ai
usage endpoint (one HTTPS GET with the stored login) and Codex's app-server
`account/rateLimits/read` request. No model call, no session, no tokens.

**Dependencies.** Linux or macOS, `curl`, `sqlite3` (optional, for Codex token totals and retry logs), and a
Rust toolchain to build. Plan windows need a subscription login (claude.ai for Claude, ChatGPT
for Codex); without one the tool says so and still reports local tokens.

## Install

```bash
cargo build --release --manifest-path rs/Cargo.toml -p agent-usage
install -m 0755 rs/target/release/agent-usage ~/bin/agent-usage
```

## Use

```bash
agent-usage                 # report; polls a provider whose newest reading is over 2 minutes old
agent-usage --json          # the same, for scripts and agents
agent-usage --line          # one line: meters, burn per hour, resets, 1 h tokens
agent-usage --cached        # never poll; report from the history only
agent-usage daemon --detach # poll every 15 minutes in the background so burn rates have data
agent-usage daemon status   # is it running, how fresh is the history
agent-usage daemon stop
```

Example: a claude.ai Max login with three hours of 15-minute history, followed by a host whose
Claude Code runs on a cloud provider (no plan limits; local tokens only). The numbers are made up
for the first part and real for the second.

```text
claude [max]: plan usage (oauth-usage, read 35s ago)
  Current session             34% used, 66% left, resets 6:20am PDT (in 1h59m)
    burn (points): 15m +1.9, 1h +7.9, 3h +23.9, 24h +24.0 (3h00m seen)
    at 8.0 points/h (1h rate): 100% in 8h15m, after the reset
  Current week (all models)   22% used, 78% left, resets Oct 11 8:20am PDT (in 1d03h)
    burn (points): 15m +0.2, 1h +1.0, 3h +3.0, 24h +3.0 (3h00m seen)
    at 1.0 points/h (1h rate): 100% in 3d05h, after the reset
  Current week (Fable)         5% used, 95% left, resets Oct 11 8:20am PDT (in 1d03h)
    burn (points): 15m +0.1, 1h +0.5, 3h +1.5, 24h +1.5 (3h00m seen)
    at 0.5 points/h (1h rate): 100% in 7d21h, after the reset

claude: no plan limits to read (read now): no claude.ai login; this environment uses vertex, which has no plan limits
  local tokens (transcripts on this host):
    15m:   452 requests,   125M tokens (output 196k, cache read 122M, cache write 2.7M, input 904)
     1h:   823 requests,   244M tokens (output 449k, cache read 240M, cache write 3.6M, input 1.7k)
     3h:  1431 requests,   532M tokens (output 841k, cache read 526M, cache write 5.6M, input 2.9k)
    24h: 10769 requests,   4.5G tokens (output 6.0M, cache read 4.4G, cache write 38M, input 22k)
```

Burn rates need at least two readings, so start the daemon once per host (or run
`agent-usage daemon --once` from cron). History lives in `~/.cache/agent-usage/history.jsonl`.

Run `agent-usage userguide` for the full reference: sources, the burn arithmetic, files,
environment variables, costs and failure modes.
