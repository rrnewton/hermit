---
title: 'voice-connect-latency: make call startup measurable and fast'
status: closed
priority: 0
issue_type: bug
assignee: opus-5.5
labels:
- vibe-talk
- voice
- performance
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T03:55:15.301955780+00:00
updated_at: 2026-09-25T23:54:35.158742000+00:00
closed_at: 2026-09-25T23:54:35.158742000+00:00
claimed_at: 2026-09-25T05:09:54.970463188+00:00
claimed_until: 2026-09-27T05:09:54.970305732+00:00
---

# Description

Measure and reduce the delay from pressing Start a new call through session acquisition, browser WebSocket connection, provider readiness, first greeting transcript, and first audible greeting. Emit only content-free phase timing. Test the live deployment as well as deterministic local cases.

# Acceptance Criteria

A live call records each phase, the dominant delay is identified and reduced or isolated, and the greeting appears and is audible.
