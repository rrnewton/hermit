---
title: 'offline-message-cache: show downloaded messages before refresh'
status: closed
priority: 0
issue_type: feature
assignee: opus-5.5
labels:
- vibe-talk
- pwa
- performance
- offline
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T04:23:34.824211145+00:00
updated_at: 2026-09-25T06:44:38.963137648+00:00
closed_at: 2026-09-25T06:44:38.963136907+00:00
claimed_at: 2026-09-25T05:19:54.982579786+00:00
claimed_until: 2026-09-27T05:19:54.982428038+00:00
---

# Description

Persist a bounded recent-message snapshot in browser storage so an installed PWA can render the last downloaded channel view immediately on launch, then refresh in the background and merge provider changes. Scope cache keys to the authenticated app identity and channel, never put private API responses in CacheStorage/service-worker caches, clear incompatible identity data on token change or sign-out, and preserve local pending-send reconciliation.

# Acceptance Criteria

A reload or PWA cold start renders the latest bounded cached snapshot before network completion; a successful background refresh merges newer and corrected rows without duplicates; offline/failing refresh leaves cached rows visible with honest stale/error state; storage is bounded and scoped by app identity plus channel; sign-out and token replacement clear inaccessible message data; service-worker caches never contain private API responses; browser tests cover reload, offline, merge, eviction, and identity separation.
