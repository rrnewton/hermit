---
title: 'error-banner-scroll-shift: keep the reader in place when the error panel comes and goes'
status: closed
priority: 2
issue_type: bug
labels:
- vibe-talk
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T12:14:01.506688484+00:00
updated_at: 2026-09-25T12:18:13.774466013+00:00
closed_at: 2026-09-25T12:18:13.774465663+00:00
---

# Description

When a channel refresh fails on /voice, the page shows the error panel (`#error-wrap`). The panel is a grid row above the message list, and nothing kept the reader in place when it appeared or cleared. Found in the #32 freshness-pill-overlap review. It predates #32.

Measured on 6d09127 in real Chromium, with the reader scrolled mid-list or parked at the newest line:

- **Panel appears:** the list keeps its scroll offset while its window moves down by the panel's height. The line being read moves down the screen by that height, and the newest line falls 74–90px below the fold.
- **Panel clears:** the line stays where it is on the screen instead of moving back.

The net effect of one failed refresh and its recovery: the line being read drifts 74–89px, and the newest line ends up 74–90px out of view. The sizes are 412x915 (89px), 360x800 (88px) and 1280x800 (74px).

Wanted:

- Showing or clearing the panel leaves a mid-list reader's line where it is on the screen.
- A reader at the newest line stays at the newest line.
- A reader at the very top stays at the top, with the header visible.
- The error must not be hidden or delayed, and the list may give up room only at its head.
