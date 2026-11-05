---
title: 'gchat-deployment-handoff: reproduce the mobile reader on the intended host'
status: closed
priority: 0
issue_type: task
assignee: opus-5.5
labels:
- vibe-talk
- deployment
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T01:32:01.933921963+00:00
updated_at: 2026-09-25T06:45:49.441915605+00:00
closed_at: 2026-09-25T06:45:49.441914914+00:00
claimed_at: 2026-09-25T04:46:58.029027043+00:00
claimed_until: 2026-09-27T04:46:58.028846853+00:00
---

# Description

[gpt-5.6-sol] Recreate the already-proven provider-neutral app plus private adapter deployment on the intended host. Keep private host, credential, and organization-specific setup out of this repository.

# Acceptance Criteria

Managed mobile Chrome opens the deployed URL; real chat rows render; push is connected; services survive logout and restart; no unintended deployment remains on the source host.

# Notes

A prototype was validated on a temporary source host and then deliberately stopped and disabled after discovering it was the wrong machine. Reproduce it on the intended host from the private handoff; do not copy host-specific configuration into this repository.

[gpt-5.6-sol] Completed the intended-host handoff. Mobile Playwright reached the public `/voice` page, rendered real provider rows, observed connected push delivery with no periodic history reads, and verified all bounded user services across restarts. Provider-specific evidence and the voice-worker MCP contract are retained outside this repository.
