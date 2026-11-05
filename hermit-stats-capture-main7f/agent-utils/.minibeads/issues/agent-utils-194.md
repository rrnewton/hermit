---
title: 'thread-picker-polish: one thread name everywhere, d/h ages, older threads on their own page'
status: closed
priority: 1
issue_type: feature
created_at: 2026-10-04T11:21:19.140607645+00:00
updated_at: 2026-10-04T12:59:22.252680156+00:00
closed_at: 2026-10-04T12:59:22.252679996+00:00
---

# Description

[opus 5.5] Owner feedback 2026-10-04 07:18, from a phone screenshot of a thread view: "I would like that thread identity at the top to be consistent with the text we use in the drop down. Also the drop down shows times like 42h but that should be 1d22h for consistency. It also shows too many threads with a scrollable list. It should show only about one screen worth with an '... Older threads' option that pops up a dedicated paginated selector page." The screenshot also shows the floating chip strip (Summaries / Undo / Expand all / Collapse all / Newest) sitting on top of the thread view's "Reply in this thread" composer. The "Live · updated" pill at the top is loved and stays as it is.

Plan:
1. One naming function for a thread everywhere (picker option, thread header, the Older-threads screen, the fallback option for an open thread that is not in the directory): display_name, then provider title, then first-message prefix. The header follows the same computation and updates when a better name arrives later (a directory refresh or a thread summary), showing the same name less truncated plus the same "age · replies" facts.
2. One compact age scale used wherever a thread age appears: now, Nm, Nh, NdMh (dropping 0h), plain Nd from ten days, then Nw. Never "42h".
3. The picker lists about one phone screen of threads (a named constant), newest activity first, then "… Older threads" when more exist locally or on the server. That opens a dedicated screen listing every thread as "age · replies · name" plus the server's one-sentence summary, newest first, with Load older (paged by next_before), loading/empty/error+retry states and a back control. Tapping a row opens the thread and returns to the channel. The picker never stays on "Older threads".
4. The floating chip strip no longer covers the thread reply composer when the thread view is scrolled to the bottom.
