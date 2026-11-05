---
title: 'persistent-cache: a Settings option to keep server caches on disk, time-slice indexed, plus connection profiling'
status: open
priority: 3
issue_type: feature
created_at: 2026-10-05T06:28:49.482548122+00:00
updated_at: 2026-10-05T06:28:49.482548122+00:00
---

# Description

[opus 5.5] Owner, 2026-10-05 02:26: write the plan, do not implement yet. A Settings option to persist vibe-talk server caches (e.g. SQLite) of what was already downloaded, with operations mostly time-slice indexed, so a new day's first open fetches about the last day rather than the whole history. Latency depends on each provider's CLI/API (Google Chat vs Slack vs Discord). Subproposal: the server keeps a performance model of its connections (profiling), so it can say how much a persistent cache would help; the ~11 s Google Chat whole-space read is too slow.

Plan: ~/work/vibe-talk-reports/2026-10-05-persistent-cache-plan.md (being written and reviewed).
