---
title: 'summary-truncation: never cut a summary or thread sentence the model wrote; keep a generous hard cap'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-12T20:27:27.959043548+00:00
updated_at: 2026-10-12T23:49:08.492965782+00:00
closed_at: 2026-10-12T23:49:08.492965422+00:00
---

# Description

Owner 2026-10-12: thread slugs + summaries show now, but some sentences end cut off with '...'. The summariser is asked for target_chars and plain()/condense cuts at the same number, so a slightly long answer is truncated. Keep asking for the same length but cap far above it, so model nondeterminism never truncates; regenerate already-cut summaries.
