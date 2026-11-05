---
title: 'voice-catchup-speed: keep recent_activity well inside the voice client''s 15 s tool limit'
status: closed
priority: 2
issue_type: bug
created_at: 2026-10-07T09:51:50.417611815+00:00
updated_at: 2026-10-13T02:44:47.068823367+00:00
closed_at: 2026-10-13T02:44:47.068823257+00:00
---

# Description

[opus 5.5] From the #190 voice-agent-tools live re-run (2026-10-07): recent_activity took 5-9 s (average 6.0 s) per call, about 2 s per channel, because each channel read goes through the Google Chat adapter's single CLI slot. During a ~58 s upstream stall three calls hit the voice client's 15 s tool timeout and that session got no answer. Make the catch-up fast and bounded: serve it from the adapter's in-memory snapshot (milliseconds when warm) or a server-side cache, answer with what is ready by a deadline and name the channels that were slow, and/or report per-channel timing.
