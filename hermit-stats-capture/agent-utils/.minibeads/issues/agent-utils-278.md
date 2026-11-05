---
title: 'send-order: messages to one destination leave in the order written, through retries and reloads'
status: closed
priority: 0
issue_type: bug
labels:
- vibe-talk
created_at: 2026-10-13T20:15:23.231346664+00:00
updated_at: 2026-10-13T20:17:05.545156312+00:00
closed_at: 2026-10-13T20:17:05.545156151+00:00
---

# Description

[opus 5.5] Owner 2026-10-13 18:20Z: 'when it retries it ends up sending messages OUT OF ORDER. I.e. I write A then B but B lands first.' Fixed page-side (outbox order is send order; later messages wait behind an earlier one) and server-side (a per-destination lane held until the provider answers).
