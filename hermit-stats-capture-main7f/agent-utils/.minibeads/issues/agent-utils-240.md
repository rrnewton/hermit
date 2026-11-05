---
title: 'reading-bookmark: an explicit bookmark line for catching up on a backlog, with keys to set and jump to it'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-12T13:54:23.871318049+00:00
updated_at: 2026-10-12T16:47:50.098686612+00:00
closed_at: 2026-10-12T16:47:50.098686462+00:00
---

# Description

Owner 2026-10-12: when catching up on a long backlog, mark the place, reply, jump back, mark done, and advance the bookmark. A clearly coloured horizontal separator like the collapsed-run line; Shift+B places it before the current message, b jumps to it (analogous to e / Shift+E); a jump-to-bookmark button like Newest; marking the message just after the bookmark done bumps the bookmark past it. Setting it marks nothing done. Also make done-through-here efficient: an O(N) server sweep is fine, and a cached all-done frontier only behind a small interface that keeps it exact.
