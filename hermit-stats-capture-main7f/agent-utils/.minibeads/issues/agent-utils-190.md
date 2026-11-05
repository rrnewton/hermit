---
title: 'voice-agent-tools: confirm the voice agent can read channels now that it connects'
status: closed
priority: 2
issue_type: task
labels:
- vibe-talk
created_at: 2026-10-04T10:09:55.533695979+00:00
updated_at: 2026-10-07T09:51:50.404823391+00:00
closed_at: 2026-10-07T09:51:50.404823090+00:00
---

# Description

[opus 5.5] On 2026-09-26 the voice agent connected but answered 'I can't retrieve that message': its chat tool call failed. The connection is fixed (e4ecde0e, same-origin socket); verify a tool call works end to end and fix it if not.
