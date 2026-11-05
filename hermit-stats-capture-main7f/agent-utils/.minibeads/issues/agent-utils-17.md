---
title: 'graceful-stream-shutdown: bound service restart downtime'
status: closed
priority: 0
issue_type: bug
labels:
- vibe-talk
- deployment
- reliability
depends_on:
  agent-utils-2: parent-child
created_at: 2026-09-25T04:07:40.434800064+00:00
updated_at: 2026-09-25T04:16:00.000000000+00:00
---

# Description

The Axum graceful-shutdown path waits for long-lived channel SSE responses, so a routine systemd restart remains in stop-sigterm until the 90-second service timeout and the public endpoint returns 502. Make shutdown actively terminate or drain live streams and bound restart downtime without losing persistent state.

# Acceptance Criteria

SIGTERM closes active SSE streams and the process exits within five seconds; a restart keeps the public endpoint outage bounded; an automated test covers the open-stream case.

[gpt-5.6-sol] Added a five-second application-level graceful-drain deadline with an automated paused-clock open-stream test. A deployed restart while an SSE stream was open completed in 5.07 seconds including service startup, instead of waiting for the 90-second systemd stop timeout.
