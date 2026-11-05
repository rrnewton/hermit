# agent-usage: related work

Public open-source tools that address the same need, and how agent-usage differs.

| Tool | What it reads | Plan percentage and reset | Burn rate | Notes |
| --- | --- | --- | --- | --- |
| **The CLIs' own `/status` (Claude Code Usage tab, Codex `/status`)** | The provider's usage endpoint | Yes, authoritative | No | Interactive only; an agent cannot read it without driving a terminal. agent-usage reads the same endpoints directly. |
| **ccusage** (`ryoppippi/ccusage`) | Claude Code transcript JSONL on the local disk | No: it estimates against token counts, it does not ask the server | Yes, for the current 5-hour block, from local tokens | Rich daily, monthly, session and block reports and cost estimates. Sees only this machine's transcripts, so usage from other machines or claude.ai is invisible to it. |
| **Claude Code Usage Monitor** (`Maciek-roboblog/Claude-Code-Usage-Monitor`) | Claude Code transcript JSONL | No: plan limits are configured or inferred from past usage | Yes, with a predicted exhaustion time | A live terminal dashboard rather than a one-shot command. |
| **Status-line scripts** (for example `ccstatusline`) | The JSON Claude Code passes to its status-line command | Recent Claude Code versions include the plan windows in that JSON | No history | Only runs while an interactive session renders its status line. |

What agent-usage adds:

- **The provider's own numbers.** Plan percentages and reset times come from the endpoints the
  `/status` screens use, so usage from other machines and from claude.ai or ChatGPT is included,
  and per-model windows (for example a separate weekly limit for one model) appear as the server
  defines them.
- **Burn rates from a history of those numbers**, over fixed windows (15 minutes, 1 hour,
  3 hours, 24 hours), with reset detection and an honest "covered" span when the history is
  short.
- **Both Claude Code and Codex** in one command and one JSON shape.
- **Zero-token polling**, cheap enough to call before every unit of agent work: a cached answer
  costs about 10 ms, and a fresh one is one HTTPS GET (Claude) or one short-lived app-server
  process (Codex).
- **Local token windows as well**, from Claude transcripts (incrementally indexed, so repeat
  calls read only appended bytes) and Codex thread totals. These keep working where there are no
  plan limits to read.

What it does not do: cost estimation in currency, per-project or per-session breakdowns (ccusage
does these well), or a live dashboard.
