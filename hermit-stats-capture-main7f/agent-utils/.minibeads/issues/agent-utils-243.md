---
title: 'filter-keeps-selection: changing or clearing a filter keeps the selected message on screen at the same height'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-12T14:22:03.498369046+00:00
updated_at: 2026-10-12T17:33:42.241393572+00:00
closed_at: 2026-10-12T17:33:42.241393462+00:00
---

# Description

Owner 2026-10-12: search, filter to Pinned, select the pinned message, clear the filter: the list jumps and the found message is gone. Rule: whenever a filter changes (search text, Pinned, Links, PRs/Commits/Actions toggles, read modes), the selected message must stay on screen, ideally at the same Y position with no jump. Losing the reader's place in a long history is the worst failure a chat client has.
