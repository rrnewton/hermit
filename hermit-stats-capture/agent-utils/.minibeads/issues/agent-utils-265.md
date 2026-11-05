---
title: 'window-channel-memory: a reload keeps each window on the channel it was showing'
status: closed
priority: 1
issue_type: bug
created_at: 2026-10-13T02:03:54.831197748+00:00
updated_at: 2026-10-13T05:29:09.769090001+00:00
closed_at: 2026-10-13T05:29:09.769089841+00:00
---

# Description

[opus 5.5] Owner, 2026-10-13 02:02Z: with two windows of the installed desktop PWA open, one on one channel and one on another, Cmd-R in the first window switches it to the other window's channel (not the first in the list either). Expected: a reload keeps each window on its own channel. Likely cause: the chosen channel is remembered in storage shared by all windows, so the last window to choose wins. Fix: remember the channel per window (the URL and/or per-window session storage), with the shared value only as the default for a new window.
