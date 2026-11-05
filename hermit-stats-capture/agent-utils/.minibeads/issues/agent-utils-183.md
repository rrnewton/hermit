---
title: 'gchat-thread-read-speed: thread timeline reads take 9-11 s and the phone reports Offline'
status: closed
priority: 0
issue_type: bug
labels:
- vibe-talk
created_at: 2026-10-04T10:09:55.444349668+00:00
updated_at: 2026-10-04T16:03:06.887051007+00:00
closed_at: 2026-10-04T16:03:06.887050667+00:00
---

# Description

[opus 5.5] Owner report 2026-10-04 06:05: the page says 'Offline · showing messages saved'. Since thread timelines were turned on for Google Chat, timeline reads take 9-11 s at the app (log), and the phone never receives them: the Nest dev proxy appears to drop responses past about 10 s, so the browser's fetch fails and the page labels itself offline. Cause: the private Google Chat adapter reuses a timeline snapshot for only 3 s (TIMELINE_FRESH_SECONDS) and otherwise rebuilds it with a full history scan through the meta CLI. Fix needs reads well under a second in the common case (snapshot reuse invalidated by the adapter's own live tail, or incremental update), without dropping a just-arrived message.
