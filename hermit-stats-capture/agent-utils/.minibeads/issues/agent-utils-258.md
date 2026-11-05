---
title: 'slack-clear-unread: mark ingested Slack messages read upstream, including thread replies if any API allows it'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-12T20:27:27.961862593+00:00
updated_at: 2026-10-12T23:04:36.320985218+00:00
closed_at: 2026-10-12T23:04:36.320985127+00:00
---

# Description

Owner 2026-10-12: Google Chat cannot clear thread unreads (no supported API); the company is moving to Slack, so dig deeper on Slack: channel read marker (conversations.mark), thread read state, what the meta slack CLI and the vibe-talk Slack shim expose, and whether thread replies can be marked read.
