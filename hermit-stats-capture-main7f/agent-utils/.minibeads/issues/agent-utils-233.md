---
title: 'llm-summarizer: a direct Messages-API summariser (Haiku, no tools) for message one-liners and thread slug plus sentence'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-11T15:29:55.943586177+00:00
updated_at: 2026-10-11T17:41:46.779142807+00:00
closed_at: 2026-10-11T17:41:46.779142456+00:00
---

# Description

Owner 2026-10-11: summaries today only come from an ElevenLabs agent, and this deployment has no ElevenLabs credentials, so every summary fails. Add a backend that makes one plain HTTP call to the Messages API (anthropic dialect for the public API, vertex dialect for Google Vertex or an mTLS gateway speaking that path), never sends tools, rate-limited, configured per host through a config overlay.
