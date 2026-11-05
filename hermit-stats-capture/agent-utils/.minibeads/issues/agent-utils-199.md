---
title: 'removable-config-channels: every channel in the list can be removed, including ones from the config file'
status: closed
priority: 1
issue_type: feature
created_at: 2026-10-04T13:37:14.300261567+00:00
updated_at: 2026-10-04T16:03:06.901987323+00:00
closed_at: 2026-10-04T16:03:06.901987203+00:00
---

# Description

[opus 5.5] Owner report 2026-10-04 09:33: he cannot remove the 'agentcloud homebase thread' channel from his list. It is the only [[channels]] entry in the deployment's config file; DELETE /api/v1/channels/{id} refuses configured channels (channel_is_configured), the page hides Remove for them, and config validation requires at least one configured channel. Wanted: the owner can take any channel off his list from the app (a durable server-side hide for configured channels, or allowing an empty configured list when channel registration and a store exist), with a way to bring it back.
