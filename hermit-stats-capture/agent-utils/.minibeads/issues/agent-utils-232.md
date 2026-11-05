---
title: 'upstream-mark-read: optionally mark ingested messages read in the upstream client, keeping vibe-talk''s own read state'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-11T15:23:26.534794992+00:00
updated_at: 2026-10-11T17:10:51.878665751+00:00
closed_at: 2026-10-11T17:10:51.878665060+00:00
---

# Description

Owner 2026-10-11: agent updates flood the native client's unread feed and bury human messages. vibe-talk maps only a subset of spaces and keeps its own read marks. Add a Settings option (per channel) that marks every message vibe-talk ingests from that space as read upstream (Google Chat first), so agent traffic is triaged only in vibe-talk and the native client's unread feed holds only human conversations. Prototype, deploy, let the owner try it.
