---
title: 'reply-coalesce: an N replies button that gathers a message''s replies under it, and Thread(N) on the thread chip'
status: closed
priority: 2
issue_type: feature
created_at: 2026-10-08T14:24:01.907697401+00:00
updated_at: 2026-10-08T19:06:08.695168877+00:00
closed_at: 2026-10-08T19:06:08.695168766+00:00
---

# Description

[opus 5.5] Owner, 2026-10-08 16:23 CEST. The 'N replies' button no longer opens the thread view (redundant with the thread chip, which should read 'Thread(12)' instead of 'Thread 12'). Instead it COALESCES: every reply moves up to join its parent in a dense stack underneath it; nothing disappears, it is a reorder. The stacked replies drop their arrows for one connected bridge in the arrow colour: a vertical spine out of the parent with a horizontal connector into the left side of each child, no arrow heads. No scroll: the parent stays where it was on screen. Double-clicking the arrow head on any reply jumps to its parent's coalesced view, scrolled so that reply stays on screen. An X floating left of the bridge dissolves it and sends the children back to their chronological places in the All view.
