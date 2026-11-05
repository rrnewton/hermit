---
title: 'scrollback-jump: a jump-to-newest button at the lower left whenever the list is scrolled back'
status: closed
priority: 2
issue_type: feature
created_at: 2026-10-05T06:24:46.984981865+00:00
updated_at: 2026-10-05T08:18:37.903672554+00:00
closed_at: 2026-10-05T08:18:37.903671743+00:00
---

# Description

[opus 5.5] Owner, 2026-10-05 02:24: "I saw a button in the lower right to jump to newest messages. But it only appeared I think when a new message arrived perhaps. I would like to generalize this so that whenever we're in the scroll back there's a jump to bottom button in the lower left. In that same row with the summarys that expand collapse"

Today #jump-newest (web/voice.html, the chip row with Summaries / Expand all / Collapse all) is raised only by an arrival while the reader is scrolled back (setJumpNewest / jumpNewestWanted) and sits at the right end of the row. Wanted: one jump-to-newest control at the LEFT end of that row, present whenever the list on screen is scrolled away from its newest message, still saying when something new arrived below.
