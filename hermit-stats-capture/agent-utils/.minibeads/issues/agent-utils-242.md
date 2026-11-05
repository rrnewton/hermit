---
title: 'rich-headers: lift an agent''s [who-am-i] tag, its stated time and a title from a message''s first line into the header row'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-12T14:16:16.075142499+00:00
updated_at: 2026-10-12T17:00:37.794356643+00:00
closed_at: 2026-10-12T17:00:37.794356522+00:00
---

# Description

Owner 2026-10-12: agent messages start with a who-am-i tag ([name role] or a recurring (tag)), often a stated time (11:58 AM ET, 15:05Z) and a short title, and the desktop header row has room to spare. Best-effort server-side extraction before markdown to HTML: optional tag, optional time, title, remainder body. The page promotes what fits into the header (timestamp, tag in subdued italics, bold title, subdued @time, then the buttons) and shows the rest in the body; nothing is ever dropped. A (paren) tag counts only when the same tag starts more than one message in the channel. Settings toggle, on by default; with no extraction the message renders as today.
