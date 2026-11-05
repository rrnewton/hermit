---
title: 'gchat-thread-selector: put a thread picker beside the channel picker, always shown'
status: closed
priority: 1
issue_type: feature
labels:
- vibe-talk
- threads
- ui
created_at: 2026-10-03T20:29:54.590362348+00:00
updated_at: 2026-10-03T20:30:03.126680812+00:00
closed_at: 2026-10-03T20:30:03.126680481+00:00
---

# Description

[opus 5.5] Owner specification, 2026-10-03, which #9 gchat-thread-selector closed while still waiting for: a thread dropdown immediately right of the channel picker, at the bottom, shown all the time, with Main and All as extra options. It replaces the Main / Threads / All tabs over the list, which appeared only when the provider reported threads.

# Acceptance Criteria

The channel view's control bar shows a thread picker beside the channel picker on every channel; it offers Main, All, and each known thread by name; picking an entry shows that view; a provider without thread timelines shows an inert picker with Main; the page and browser suites cover it.
