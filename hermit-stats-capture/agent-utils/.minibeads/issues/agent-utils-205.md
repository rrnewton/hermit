---
title: 'channel-view-memory: remember the view the owner picked in each channel, across restarts'
status: closed
priority: 1
issue_type: feature
created_at: 2026-10-04T20:18:40.761308810+00:00
updated_at: 2026-10-05T06:10:23.860091356+00:00
closed_at: 2026-10-05T06:10:23.860091216+00:00
---

# Description

[opus 5.5] Owner, 2026-10-04 16:18, answering a #189 restore-ui-state follow-up: 'All should be the default but if changed the selection should be saved for that channel. Ideally it should survive restarts. Stored in local client state ideally but server side if needed.' Today an explicit Main (or thread) choice resets to All on a channel switch, and only the last channel's view survives a restart.
