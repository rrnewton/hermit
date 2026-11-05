---
title: 'voice-call-drops: voice calls end on their own, and a spoken yes is not acted on'
status: closed
priority: 0
issue_type: bug
created_at: 2026-10-13T11:49:51.275172468+00:00
updated_at: 2026-10-13T17:10:11.353484411+00:00
closed_at: 2026-10-13T17:10:11.353484251+00:00
---

# Description

[opus 5.5] Owner, 2026-10-13 11:31Z: 'I started voice sessions but on multiple occasions they died abruptly without me pressing the hang up button.' and 11:34Z: in approval mode the card appeared but 'it did not respond to me verbally saying yes when it asked me' (the agent read back exactly and said 'One moment.'; the owner then tapped Send at 11:29:59). Bridge log: 11:27:15 'Connection reset without closing handshake' (22 s into a call); 11:30:18 and 11:30:44 the page sent audio_end + quit (the second 9 s after the call opened); 11:30:49 'interrupt boundary timed out' then disconnect.
