---
title: 'view-switch-instant: switching All to Main (or back) redraws from what is loaded, with no blank screen and no wait'
status: closed
priority: 1
issue_type: bug
created_at: 2026-10-08T16:06:41.699275905+00:00
updated_at: 2026-10-08T22:45:59.454582146+00:00
closed_at: 2026-10-08T22:45:59.454582026+00:00
---

# Description

[opus 5.5] Owner, 2026-10-08 18:03 CEST: switching the channel view from All to Main flashed a blank screen and ran the fetch/loading animation, although All had already loaded every message and Main only hides thread replies. Wanted: a view switch paints immediately from what the page already holds (All -> Main filters locally; Main -> All shows what is loaded and only fetches what is genuinely missing, without blanking), and never re-fetches cached data or makes the reader wait.
