---
title: 'voice-bridge-interrupt-ends-session: a barge-in ends the bridge''s whole session and leaves the socket open'
status: closed
priority: 1
issue_type: bug
labels:
- vibe-talk
created_at: 2026-10-04T10:09:55.457422484+00:00
updated_at: 2026-10-13T09:48:50.033826107+00:00
closed_at: 2026-10-13T09:48:50.033825997+00:00
---

# Description

[opus 5.5] Owner report 2026-10-04 05:54. Bridge log: the backend session was cancelled with reason user_interrupt at 09:53:16Z, after which every audio frame failed with 'Session ended: session already ended' (1,703 in 2.5 minutes) while the WebSocket stayed open until the owner hung up. The page side is fixed in 2856b42c (an error frame now ends the call). Remaining, in the private bridge (local fbsource only, not published without owner review): an interrupt should cut the turn, not end the session; and an ended session should close the socket or send one error frame and stop.
