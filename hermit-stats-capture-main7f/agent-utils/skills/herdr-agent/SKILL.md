---
name: herdr-agent
description: Durably queue, submit, inspect, and read prompts for an already-running interactive agent in a Herdr pane. Use when a coordinator, cron job, or adapter must deliver a prompt without losing it or automatically duplicating an ambiguous submission.
---

# herdr-agent

Bind each queue to one exact pane or stable agent session, and assert every identity fact available
to the caller. A mismatch is a refusal. Do not manually retry an ambiguous submission until its
failed artifact and visible agent state have been inspected.

The installed CLI is the source of truth:

- `herdr-agent --help`
- `herdr-agent status --pane PANE --agent AGENT --workspace WORKSPACE --cwd CWD`
- `herdr-agent send --pane PANE --queue QUEUE --file PROMPT`
- `herdr-agent drain --pane PANE --queue QUEUE`
- `herdr-agent read --pane PANE --lines 500`
- `herdr-agent userguide`

Exit 75 with `outcome: pending` means nothing was injected and the durable inbox artifact is safe
for a later drain. Exit 76 with `outcome: possibly_submitted` means the prompt crossed the durable
inflight barrier and must not be automatically submitted again.
