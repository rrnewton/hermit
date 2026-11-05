---
title: 'nested-lists: chat-style indented sub-lists render flat in server-side markdown'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-11T18:27:05.265622544+00:00
updated_at: 2026-10-11T20:45:28.939473196+00:00
closed_at: 2026-10-11T20:45:28.939472846+00:00
---

# Description

Owner 2026-10-11 (screenshot): a '1.' item followed by sub-bullets renders the bullets at the same level, and a following line glues onto the last bullet. CommonMark nests only at the parent's content column (3 spaces under '1. '); chat text usually indents 2 spaces. Fix in the HTML render path with a list-indent normalisation pre-pass, and check nested-list CSS.
