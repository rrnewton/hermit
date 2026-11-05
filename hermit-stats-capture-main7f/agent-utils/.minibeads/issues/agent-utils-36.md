---
title: 'thread-view-phone-overflow: keep a long thread title from widening the phone layout'
status: closed
priority: 0
issue_type: bug
labels:
- vibe-talk
- threads
- ui
- regression
created_at: 2026-09-25T19:23:43.223208716+00:00
updated_at: 2026-09-25T19:41:51.552485000+00:00
closed_at: 2026-09-25T19:41:51.552485000+00:00
---

# Description

[opus 5.5] On an Android-phone viewport, opening a thread whose title is longer than the screen makes every message row run off the right edge. At 412 CSS px the document is 732 px wide: the heading's title is `white-space: nowrap`, so its min-content width is the whole title, and `#screen-main`'s implicit grid column is `auto`, which grows to that minimum instead of letting the title ellipsise. The list, the composer and the view tabs all inherit the widened column. Hiding the heading alone restores 412 px; hiding message metadata, bodies, the composer or the freshness row changes nothing.

# Acceptance Criteria

At 412 and 360 CSS px, a thread with a long title (words and a long unbroken token) and a body with a long unbroken token and inline code opens with the document no wider than the viewport, no shown element of the main screen past the right edge, and the title ellipsised; a real-Chromium regression fails without the fix; the deployed page is verified at phone width.
