---
name: wrkviz
description: Build an idempotent, zoomable coordinator/subagent timeline with cached phrase, paragraph, daily, weekly, monthly, and quarterly summaries. Use when a user wants to understand or archive what an agent team did over real time.
---

# wrkviz

Turn coordinator and subagent rollouts into durable JSON/Markdown and a self-contained local SVG
website. The model-spending summary stage is separated from token-free formatting, and
content-addressed caches make refreshes incremental.

The CLI is the source of truth:

- `wrkviz quickstart`
- `wrkviz --help`
- `wrkviz --userguide`
