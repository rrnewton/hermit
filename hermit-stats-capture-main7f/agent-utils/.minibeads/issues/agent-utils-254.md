---
title: 'keymap-layout-fix: a punctuation key is read by its character, not by its QWERTY position'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-12T19:57:06.490183931+00:00
updated_at: 2026-10-12T19:57:16.959192844+00:00
closed_at: 2026-10-12T19:57:16.959192704+00:00
---

# Description

Owner 2026-10-12: ':' collapses all but ';' marked a read message unread. On a Dvorak-style layout ';' is typed on the key QWERTY calls Z, and the non-Latin-layout fallback (read a letter shortcut by its key position when the character is not a Latin letter) turned ';' into z = Undo last Done.
