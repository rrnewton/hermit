---
title: 'freshness-pill-overlap: keep the saved-messages pill off the list header'
status: closed
priority: 3
issue_type: bug
labels:
- vibe-talk
- pwa
- offline
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T11:28:54.453675745+00:00
updated_at: 2026-09-25T11:45:44.943730203+00:00
closed_at: 2026-09-25T11:45:44.943728972+00:00
---

# Description

The saved-messages freshness pill on /voice (`#channel-freshness`, added by #18 offline-message-cache) floats over the top of the message list. It sits below the Main/Threads/All tabs, and it does not cover them. But it does sit on top of the first line of the list: the section header ("1 MESSAGE FROM … THREAD") or the day divider. At phone width it hides the middle of that label.

This was seen in the live QA of the cached cold start at 412x915, on a one-conversation source, which has no view tabs. While the page says "Saved HH:MM · refreshing…" the pill covers part of the header. It goes away when the refresh lands, so the overlap is transient in the normal case. In the "Offline · showing messages saved …" and "Refresh failed · …" states, the pill stays and so does the overlap.

Constraints from #18: the pill must not resize or jitter `#scroll-area`, must not cover the view tabs, and must stay announced (role=status). A fix could reserve the pill's height at the top of the list only while it is shown, or lay it out in flow above the header without moving the scroller's box.

# Acceptance Criteria

At 412x915 and at desktop width, in the refreshing, offline and failed states, with and without view tabs, the freshness pill covers neither the view tabs nor the list's header or first divider label. #scroll-area does not change size when the pill appears or clears. tests/offline_cache_browser.py asserts the no-overlap geometry against the header.
