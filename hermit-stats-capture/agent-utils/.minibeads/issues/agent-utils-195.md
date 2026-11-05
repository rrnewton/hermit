---
title: 'send-resilience: survive a slow backend without losing sends or raising raw errors'
status: closed
priority: 1
issue_type: bug
created_at: 2026-10-04T10:35:59.105646092+00:00
updated_at: 2026-10-04T11:26:03.697198038+00:00
closed_at: 2026-10-04T11:26:03.697197707+00:00
---

# Description

[opus 5.5] Live incident 2026-10-04 ~10:17-10:25Z: the Google Chat adapter answered timeline reads in 10-12 s and twice failed them after 50 s (502), and failed two thread replies on upstream timeouts. The page (1) raised a persistent red banner carrying the raw 'HTTP 502 chat_error: ... {json}' text for a background timeline refresh the reader never asked for, although the freshness pill already said 'Refresh failed'; (2) left the failed send in the outbox for a manual Retry even though the same message went through from the official app moments later; (3) waited on a timeline fetch longer than the proxy keeps the connection. Wanted: background refresh failures stay in the freshness pill (human wording, no raw JSON); a failed send is retried automatically with backoff exactly when the server can prove it will not double-post (a stable per-entry idempotency key honoured by the provider, or a failure that provably happened before anything was posted), resuming on 'online' and on becoming visible, never for a 4xx refusal, and the row says what is happening; a timeline fetch gives up client-side within a bounded time so 'refresh failed' appears promptly.
