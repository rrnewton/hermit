---
title: 'claude-linux-reply-bullet: accept Claude Code''s Linux reply bullet in chat reply capture'
status: closed
priority: 1
issue_type: bug
depends_on:
  agent-utils-10: discovered-from
created_at: 2026-09-25T05:11:14.829091266+00:00
updated_at: 2026-09-25T06:44:20.682855349+00:00
closed_at: 2026-09-25T06:44:20.682855238+00:00
---

# Description

[opus 5.5] Discovered while live-proving #10 agent-space-linking. Claude Code on Linux prefixes assistant output with U+25CF (●). The Python and Rust reply-marker scanners accepted only • (Codex) and ⏺, so a tagged reply from a Claude-hosted agent was never captured: the bridge reported 'closing marker is visible but no complete protocol-v3 reply block is retained'. Fix: accept ● wherever the other native bullets are accepted (py chat_replies/_closing_patterns, rs chat_service/chat_runtime) with parametrized tests.
