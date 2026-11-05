---
title: 'thread-scope-main-view: a route bound to one thread shows only its root'
status: closed
priority: 2
issue_type: bug
assignee: opus-5.5/thread-scope-main-view
labels:
- vibe-talk
- correctness
depends_on:
  agent-utils-42: discovered-from
created_at: 2026-09-26T03:34:56.981071572+00:00
updated_at: 2026-09-26T04:20:40.962507278+00:00
closed_at: 2026-09-26T04:20:40.962507148+00:00
claimed_at: 2026-09-26T03:34:56.981071572+00:00
claimed_until: 2026-09-28T03:34:56.983895250+00:00
---

# Description

[opus 5.5] Found in read-only live acceptance of #42 api-scope-first. A tracked route registered
as one thread of a space draws 1 message of the 15 the server returns, with no tabs and no other
way to reach the remaining 14. Both the loopback page and the owner's phone route show it, and it
predates #42.

The server is right and the page throws the rows away. For a registration narrowed to one
conversation, `parse_bridge_page` (`src/discord/threads.rs`, ~1033 at ac5bbce) keeps the scope in
the page's `thread`, sets `has_threads` to false and removes the scope from `threads`. That fix is
what stops Threads from listing the channel as its own child. The Main page then holds the whole
conversation: the root, plus its replies, each still carrying thread membership with
`is_root: false`.

The page filters that Main page again with `inView` (`web/voice.js`, ~5339), which keeps only
unthreaded messages and thread roots. Its comment says this is "the same rule the server's timeline
applies". For a scoped route it is not, so all 14 replies are dropped. The same rule appears
in:

- `foldTimelinePage` (~5403), which merges the page into the store through `inView`;
- `projectView` (~5433), so the saved/offline paint drops them too;
- `appendChannelRow` (~9859), so a new live reply on Main is stored but never drawn.

`channelHasThreads` is false, so the tabs and the channel navigation are hidden (~5798), and there
is no view left that shows the replies.

The contract compounds this. `TimelinePage.thread` (`src/threads.rs`) is documented as "the
selected thread, for the thread view", yet the bridge also fills it outside the thread view with
the registration's scope. The page cannot learn that meaning from the contract.

Measured live at ac5bbce with the read token (content-free: counts only):

| route | Main returned | unthreaded / roots / replies | `thread` present | `has_threads` | Threads listed | drawn |
|---|---|---|---|---|---|---|
| thread-bound | 15 | 0 / 1 / 14, all in the scope | yes | false | 0 | 1 row, no tabs, no navigation |
| whole space (control) | 8 | 0 / 8 / 0 | no | true | 5 | 8 rows, tabs shown |

**Reproduction (read-only).**

1. `GET /api/v1/channels` with a read token, and pick the route whose Main page has `thread` set.
2. `GET /api/v1/channels/{id}/timeline?view=main&limit=50` and count the messages that are
   unthreaded, roots and replies. Count how many the Main filter keeps: unthreaded, or
   `thread.is_root === true`.
3. In a throwaway mobile browser profile with the read token, and every non-GET aborted, open
   `/voice` and select that route. Count the distinct ids in `#discord-log > li[data-ids]`, and
   check whether `#channel-view-tabs` and `#channel-navigation` are visible.

A unit or browser test can reproduce this without a provider: serve a Main page whose `thread` is
a scope summary, with `has_threads: false`, one root and N replies in that scope. Today it draws 1
row.

**Fix direction (not prescriptive).** Key the behaviour on the scope (`thread` present outside the
thread view), not on `has_threads === false`. A genuinely unthreaded channel also reports
`has_threads: false`, but its messages carry no thread membership. A threaded channel must keep
today's root-only Main. Persist the scope with the saved channel so the offline paint agrees.
Correct the contract doc for `TimelinePage.thread`, and regenerate the TypeScript contract.

# Acceptance Criteria

- On a route whose Main page carries a scope (`thread` present, `has_threads: false`), the page
  draws every message the server returned, root and replies, in time order. With the live data
  above that is 15 of 15, and the reproduction measures drawn ids equal to the server's count.
- No tabs and no Threads entry appear for that route, and the scope is never listed as its own
  child (the `a_registration_scoped_to_one_thread_offers_no_child_threads` behaviour holds).
- The same route paints all held rows from the device store on an offline or cached reload, with
  0 timeline attempts offline. A live reply in the scope appears on Main once, without a reload.
  Walking back with `before` keeps replies.
- **A genuinely unthreaded channel is unchanged:** no `thread`, `has_threads: false`, and messages
  without thread membership. Main draws every message, with no tabs, no navigation and no Threads
  read.
- A threaded channel is unchanged: without a scope, Main still draws only unthreaded messages and
  roots, and Threads and All keep today's counts. On the live whole-space route that is Main 8,
  Threads 5.
- Tests: page and browser tests cover a scoped route (initial, cached/offline and live reply), an
  unthreaded channel, and a threaded channel, and each asserts the drawn ids against the served
  page. The `TimelinePage.thread` contract doc describes the scope meaning, and the generated
  contract matches.
- Live re-check after deploy: the read-only reproduction draws 15 of 15 on the thread-bound route,
  and the whole-space route still draws Main 8 with tabs.
