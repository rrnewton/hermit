---
title: 'gchat-thread-listing: return the expected linked conversations'
status: closed
priority: 0
issue_type: task
assignee: gpt-5.6-sol
labels:
- vibe-talk
- threads
- correctness
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T01:32:03.040463709+00:00
updated_at: 2026-09-25T04:21:39.313136668+00:00
closed_at: 2026-09-25T04:21:39.313136558+00:00
claimed_at: 2026-09-25T04:13:47.042846326+00:00
claimed_until: 2026-09-27T04:13:47.042688399+00:00
---

# Description

[gpt-5.6-sol] Diagnose why the thread menu can show a thread unrelated to the expected linked conversations. Keep the fix at the provider-neutral boundary where possible and private adapter behavior outside OSS.

# Acceptance Criteria

Given multiple linked conversations, the API and UI show the expected thread identities in deterministic order with no unrelated entry; tests reproduce the original mismatch.

# Notes

Reported symptom: the Threads menu exposes one unexpected entry instead of either expected linked conversation. Trace provider thread discovery, adapter normalization, and UI filtering before changing presentation.

[gpt-5.6-sol] Investigation correction: the live app has thread timelines disabled, so the observed menu cannot be the Threads projection. Verify the two registered channel identities and their thread-scoped adapter reads before changing either timeline semantics or UI filtering.
