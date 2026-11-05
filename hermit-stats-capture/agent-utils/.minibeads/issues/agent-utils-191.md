---
title: 'read-aloud-first-lag: the first agent read starts about 5 s after the tap'
status: closed
priority: 2
issue_type: bug
labels:
- vibe-talk
created_at: 2026-10-04T10:09:55.545988303+00:00
updated_at: 2026-10-13T02:44:47.059168131+00:00
closed_at: 2026-10-13T02:44:47.059167981+00:00
---

# Description

[opus 5.5] Owner report 2026-10-03, measured: message preparation 1.9 s plus 2.9 s to the first audio for the first read of a session; later reads start in about 0.3 s. Reduce the first-read cost (prepare earlier, keep the session warm, or start speaking before preparation finishes).
