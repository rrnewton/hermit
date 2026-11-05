---
title: 'slack-push: deliver Slack messages without polling'
status: closed
priority: 2
issue_type: task
labels:
- vibe-talk
- slack
created_at: 2026-10-02T18:34:15.007227352+00:00
updated_at: 2026-10-02T21:37:45.408829213+00:00
closed_at: 2026-10-02T21:37:45.408829213+00:00
---

# Description

[opus 5.5] The devbig014 Slack instance polls each channel every 30 s through meta slack.message list. meta slack.message tail (with --catchup oldest) delivered no events for the owner's user token, so push through the ingest route is not possible yet. Find out what subscription the user-token tail needs, then push events (or change-hints) instead of polling.
