---
title: 'summary-shot-race: wait for the probed row''s own summary, not any row''s'
status: closed
priority: 1
issue_type: bug
assignee: devbig014
labels:
- vibe-talk
- screenshots
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T23:59:19.341973745+00:00
updated_at: 2026-09-26T00:11:43.213228765+00:00
closed_at: 2026-09-26T00:11:43.213228635+00:00
claimed_at: 2026-09-25T23:59:26.751218235+00:00
claimed_until: 2026-09-26T03:59:26.751027028+00:00
---

# Description

[opus 5.5] Scene 32-channel-summarised in vibe-talk/scripts/screenshots.py waits for ANY `#discord-log .summary-text` that is non-empty and not the `summarising…` placeholder, and its expectation checks the same any-row predicate. A different row's cached or already-finished summary therefore satisfies the wait while the measured `#summary-probe` row still shows the placeholder, so `__summarisedHeight` is taken early and equals the clamped height (107px against 107px). The shots.screenshots job then fails intermittently, and a clean retry passes.

Fix: wait for, and expect, a real summary on the `#summary-probe` row itself. Keep the strict `0 < summarised < folded` height assertion as it is. Add an offline self-test control that fails on the any-row predicate.

# Acceptance Criteria

The summarised scene's wait and expectation both name #summary-probe; the screenshot self-test rejects the any-row predicate; shots.screenshots passes; height assertion unchanged.
