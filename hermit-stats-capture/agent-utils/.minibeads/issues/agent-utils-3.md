---
title: 'gchat-channel-enrollment: preserve runtime space and thread registration'
status: closed
priority: 0
issue_type: task
assignee: opus-5.5
labels:
- vibe-talk
- channels
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T01:32:02.090624506+00:00
updated_at: 2026-09-25T06:45:49.442125430+00:00
closed_at: 2026-09-25T06:45:49.442125360+00:00
claimed_at: 2026-09-25T04:46:58.029284209+00:00
claimed_until: 2026-09-27T04:46:58.029180845+00:00
---

# Description

[gpt-5.6-sol] Verify the runtime Add channel flow against the intended deployment for both whole-space and thread-level sources, including persistence and removal.

# Acceptance Criteria

A space URL and a thread or message URL can each be added from Settings, survive restart, render messages, and be removed without editing configuration or restarting manually.

# Notes

The provider-neutral application registration API and Settings UI already exist. Remaining work is intended-host integration proof for persisted whole-space and thread-level sources.

[gpt-5.6-sol] Completed against the intended deployment through the public mobile UI. A whole-space URL and a message/thread URL were added in Settings, rendered real rows, survived a service restart, were removed in Settings, and remained absent after another restart. The pre-test whole-space enrollment was then restored and verified after restart; content-free evidence remains private.
