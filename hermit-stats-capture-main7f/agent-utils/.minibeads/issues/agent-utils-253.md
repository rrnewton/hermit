---
title: 'upstream-clear-polish: a Mark read now button instead of the relabelled checkbox, and fewer read-state quota calls'
status: closed
priority: 2
issue_type: task
created_at: 2026-10-12T19:42:39.424940790+00:00
updated_at: 2026-10-13T03:44:39.861531726+00:00
closed_at: 2026-10-13T03:44:39.861531597+00:00
---

# Description

Owner 2026-10-12: turning Clear unread On shows a checkbox, and after Save the same checkbox comes back relabelled, which reads as a second phase. Replace it with a one-shot Mark read now button. Also: Google allows about 2 read-state reads per user per minute and the adapter reads the cursor with --skip-cache before every write, so automatic clears can fail on quota; read the cursor less often. Settings should say plainly that thread replies stay unread upstream (no supported write API; investigation 2026-10-12).
