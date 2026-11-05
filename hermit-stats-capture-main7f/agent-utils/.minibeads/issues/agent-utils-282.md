---
title: 'slack-flat-skip: Slack Flat reads replies only for threads that moved since the last read'
status: closed
priority: 1
issue_type: task
labels:
- vibe-talk
created_at: 2026-10-13T19:57:59.379982269+00:00
updated_at: 2026-10-14T00:14:51.187387561+00:00
closed_at: 2026-10-14T00:14:51.187387290+00:00
---

# Description

Remote-ops audit W8 (2026-10-13 remote-ops-audit, section 2 row 13). A Slack Flat read walks the replies of every thread root on the page, up to 10 roots, each from its first reply, at 20 rows per shim call: at least 3 + sum(min(ceil((R_i+1)/20), 10)) upstream calls, 23 for 10 roots of 30 replies, on every poll of the page's default view. Remember each root's last-seen latest_reply and reply_count with the replies read for it; on the next Flat read skip roots that did not move (0 calls), read only the newer replies of roots that did (1 conversations.replies call with oldest=<last seen>), and fall back to a whole read when the count does not add up (a deletion) or the remembered replies are older than the 15 minute reconcile. Proof: slack_fake call counts per Flat read, two reads with no new reply make 0 replies calls the second time, one changed root makes exactly 1 with oldest set.
