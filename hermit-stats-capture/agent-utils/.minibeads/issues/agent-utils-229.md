---
title: 'desktop-two-column: a desktop layout toggle with Main on the left and the selected thread or a thread list on the right'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-09T20:58:56.953180064+00:00
updated_at: 2026-10-10T02:13:38.578908411+00:00
closed_at: 2026-10-10T02:13:38.578908271+00:00
---

# Description

Owner 2026-10-09: like the Google Chat desktop app's two-column view. A layout button in the desktop dock toggles single/double column. In double column the view selector greys out: Main fills the left column; the right column shows the selected thread's messages, or when none is selected a list of threads with fixed ~3-line previews (display_name: summary when summaries exist, else the first message's text). The existing go-to-thread button on a message selects the thread on the right; a small X on the right column deselects it and returns to the list.
