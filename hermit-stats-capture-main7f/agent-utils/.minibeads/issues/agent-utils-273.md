---
title: 'send-confirm-thread: a delivered thread reply is shown as Delivery unconfirmed'
status: closed
priority: 0
issue_type: bug
labels:
- vibe-talk
created_at: 2026-10-13T16:05:16.225195872+00:00
updated_at: 2026-10-13T16:05:24.266332368+00:00
closed_at: 2026-10-13T16:05:24.266332247+00:00
---

# Description

[opus 5.5] Owner 2026-10-13 12:41Z (screenshot): 'I'm having regular message send failures.' A Reply into a thread from Main showed 'Delivery unconfirmed. Retrying cannot post it twice. Google Chat could not answer: Google Chat response could not be understood: timeline response included a different thread', though the message was delivered and answered. Cause: the bridge answered with the copy its live tail had stored first, which carries no thread; vibe-talk ran the timeline validator on a post answer.
