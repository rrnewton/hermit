---
title: 'voice-auto-reconnect: heal network-induced voice call drops instead of starting over'
status: open
priority: 1
issue_type: feature
labels:
- vibe-talk
created_at: 2026-10-13T16:05:16.231715607+00:00
updated_at: 2026-10-13T16:05:16.231715607+00:00
---

# Description

[opus 5.5] Owner 2026-10-13 12:25Z: 'For call drops that are Network induced. Does this particular voice preview API provide any operations that would enable us to build and auto reconnect feature to try to heal these kinds of errors rather than completely starting a new conversation??' Answer (scout, 2026-10-13): neither backend can resume a dropped socket. Options: (a) ElevenLabs redial plus recap from #46 replay; (b) private stack redial plus recap via a bridge resume= parameter that puts the recap in the system prompt; (c) a real resume in the private bridge (keep the session for a grace period, resume token). Recommended c + a. Redial only on 1006-after-error while visible and online; never after a hang-up, an error frame or an agent end. WAITING ON THE OWNER'S GO-AHEAD (c changes the private bridge; a adds billed reconnects).
