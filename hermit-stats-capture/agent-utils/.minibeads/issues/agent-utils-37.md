---
title: 'read-aloud-cut-truncation: a completed read after an interrupt must play in full'
status: closed
priority: 1
issue_type: bug
labels:
- vibe-talk
- audio
created_at: 2026-09-25T20:04:57.111438436+00:00
updated_at: 2026-09-26T09:35:28.957363313+00:00
closed_at: 2026-09-26T09:35:28.957363213+00:00
---

# Description

[opus 5.5] Split out of #7 read-aloud-latency when that issue closed on its latency acceptance. This is the one acceptance item that did not pass.

After a read is interrupted, the next read that is allowed to run ends cleanly (not interrupted) after a few seconds. The voice service speaks the first 3-6 words of the message verbatim and then ends its response normally. Reads that do not follow an interrupt play in full.

Live evidence, 2026-09-25 19:49-20:02Z, voice service with a fresh, history-preserving backend session after every cut. Public app at 52d66ab, direct HTTP and real-browser page, read-only credential.
- 4 of 4 reads that followed a cut and were allowed past 2.5 s ended cleanly at 2.6-4.1 s. They were HTTP reads after a 3 s-delayed switch, a page completed read after an interrupt plus 80 s idle, and an HTTP read immediately after an interrupt.
- The same messages played 87-115 s when not preceded by a cut.
- Every cut opened a fresh backend session seeded with 2-13 earlier conversation items. There were no seed failures, no unseeded fallbacks and no warnings.
- A bridge-level continuity probe passed: after a cut, "what was that message about?" was answered about the cut message, and a fresh-session control was not. So history survives. The short replies happen inside correctly seeded sessions, not because of stale events from the old session.
- The earlier deployment showed the same defect (5.1 s and 4.2-4.4 s clean ends after interrupts). There it was attributed to stale end-of-turn events, which the fresh-session change has now ruled out.

Why this repository cares: the page treats a clean end as a finished read and archives the message, so a reader who interrupted one read can lose the next message after hearing a few words of it. The root cause is in the voice service's handling of seeded history and is owned there.

Possible app-side guard, not yet designed: do not archive when the audio played is far shorter than the message's expected spoken length.

Close when a live rerun shows reads after a cut (immediately, and after about 30 s and 80 s idle) playing in full with prompt first audio, while conversation history across the cut is still kept.
