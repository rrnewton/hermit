---
title: 'reply-context: the reply screen names its thread and shows the thread''s earlier messages'
status: closed
priority: 1
issue_type: feature
created_at: 2026-10-04T12:35:47.079586581+00:00
updated_at: 2026-10-04T16:03:06.917172212+00:00
closed_at: 2026-10-04T16:03:06.917171882+00:00
---

# Description

[opus 5.5] The owner, 2026-10-04 08:17, from a phone screenshot of the Reply screen (the title bar said only "‹ Reply"; under it the author and message id, a large box with the message being answered, then "Your reply", Cancel and Send reply):

> We should certainly show the thread identity at the top when we're doing a reply to a message. In fact, even while we feature the message we're replying to prominently like this, I would like the prior messages in the thread to be rendered above so that if we scroll up, we see them when we're doing this reply.

Plan (page only):

1. Thread identity in the reply screen's title bar, named exactly as the thread picker and the heading over an open thread name it (`threadName` / `threadFacts` on the thread the reply posts into, as `replyRoute` decides): "Reply · 13h · 16 · <thread name>", or "Reply · Main · <channel name>" for a reply on the main channel. It follows the destination: the "Start a new thread from this message" box, and a read that places a message the live stream left unplaced.
2. The earlier messages of the place the message being answered is in (its thread, or the main channel before it) above the featured message, oldest at the top, drawn by the channel list's own row renderer as read-only rows (no Reply, Done or more-options controls). The featured message stays prominent, and the screen opens with it and the composer in view; the context is reached by scrolling up. What the page already holds is shown first, capped at a named count; "Load earlier messages" reveals more, reading the thread's (or Main's) timeline with a `before` cursor when the page holds no more. GET only, so it works with a read token. Drafts, the outbox, the new-thread box and Cancel/Back returning to the exact line are unchanged.
3. Page tests for each of the above.

Also visible in the same screenshot, an inline markdown bug: an underscore inside a word opened and closed emphasis. A SCREAMING_SNAKE_CASE identifier with digits in it lost two underscores and had its middle word italicised, and a Rust attribute naming a snake_case feature lost its underscores and italicised the middle word. CommonMark's rule for `_` is that it opens emphasis only where it is not preceded by a letter or digit and closes only where it is not followed by one; `*` may be intraword and keeps its behaviour. Check whether the speech preparation drops these underscores too.
