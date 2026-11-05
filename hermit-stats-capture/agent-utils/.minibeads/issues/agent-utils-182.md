---
title: 'thread-display-names: name and summarise threads with an inexpensive model'
status: closed
priority: 2
issue_type: feature
labels:
- vibe-talk
- threads
- summaries
created_at: 2026-10-04T09:37:34.191747041+00:00
updated_at: 2026-10-12T19:31:37.432138825+00:00
closed_at: 2026-10-12T19:31:37.432138715+00:00
---

# Description

[opus 5.5] Owner request, 2026-10-04: each thread gets a short hyphenated display name (chosen once, kept even if the thread drifts) and a one-sentence summary, produced through the same code path as message summaries by an inexpensive model. The summary is refreshed on a backed-off schedule: at 1, 2, 4 and 8 messages, then every 10. Delivered so far: ThreadSummary.display_name and ThreadSummary.summary on the wire, thread_summary_due() implementing the schedule, and the thread picker preferring display_name. Remaining: a summariser backend this deployment can call (the shipped one needs ElevenLabs credentials, which this deployment lacks), a durable per-thread record (name, sentence, count summarised at), and the server populating both fields on timeline reads.

# Acceptance Criteria

On a deployment with a configured summariser, the thread picker and thread heading show each thread's stable display name, and its sentence is refreshed per the schedule; without one, the picker falls back to the first-message prefix and nothing fails visibly.
