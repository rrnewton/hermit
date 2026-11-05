---
title: 'replay-burst-double-read: one reload issues two identical newest-page reads'
status: closed
priority: 3
issue_type: bug
assignee: opus-5.5/replay-burst-double-read
labels:
- vibe-talk
- performance
depends_on:
  agent-utils-18: discovered-from
created_at: 2026-09-26T02:46:20.582224194+00:00
updated_at: 2026-09-26T04:17:56.962152793+00:00
closed_at: 2026-09-26T04:17:56.962152673+00:00
claimed_at: 2026-09-26T02:46:20.582224194+00:00
claimed_until: 2026-09-28T02:46:20.585820967+00:00
---

# Description

[opus 5.5] Found in read-only live QA of a threaded channel after #18 offline-message-cache. A warm
page that reloads online sends `GET /api/v1/channels/{id}/timeline?view=main&limit=50` twice, with
the same parameters and no `before`. The second read returns exactly the same message ids as the
first.

Both reads come from the replay of the live stream, not from the page's own hydration. After a
reload the stream attaches with no `Last-Event-ID`, so the server replays its whole tail. Every
frame arrives with `replayed: true`. In `web/voice.js` (line numbers at 1e5dfd7), the
`threadingSupported` branch of `receiveLiveMessage` (~11053) calls
`loadDiscord({ keepPosition: true })` for every message frame, whether or not it was replayed.

- The first replayed frame starts read 1:
  `signIn → applyClientConfig → startChannelStream → followChannel → readChannelStream →
  onStreamFrame → receiveLiveMessage → loadDiscord → loadTimeline`.
- The remaining frames arrive while read 1 is in flight. `loadTimeline` (~6109) queues them into
  one `discordQueuedLoad`, and `finishDiscordLoad` (~10313) drains that queue into read 2 as soon
  as read 1 finishes.

Measured on the live deployment (a channel with 8 Main rows, 5 threads and 2 frames in the tail):

- 2 message frames, both replayed, 0 live. Each frame's id was already rendered in the log and
  already in the device cache when it arrived.
- Read 2 returned the same id set and the same newest id as read 1.
- All frames arrived before read 1 finished.
- Same result whether the reload lands on the voice pane (no click) or on the channel pane.

A channel with an empty tail makes 0 timeline reads on the same reload.

**Why read 2 is redundant.** The replay is a snapshot of messages the server had already published
when the stream attached. Read 1 is sent after that attach, so its page already accounts for every
replayed frame, and nothing newer than read 1 triggered read 2.

**Why read 1 is questionable too.** On the voice pane, read 1 runs while the channel pane is hidden.
That breaks the rule in `scheduleDiscordPoll`: a voice call never shares its network with a
refresh nobody is looking at. On the channel pane, the view-switch handler already issues the entry
read, and read 1 merely coalesces with it.

**Reproduction.** Warm a throwaway browser profile on a threaded channel whose stream tail is not
empty, then reload.

- Count `/timeline` requests with CDP `Network.requestWillBeSent`, taking stacks from
  `Debugger.setAsyncCallStackDepth`.
- Tee the `/stream` fetch body in an init script to count replayed and live frames.

A unit test can do the same without a server: feed `onStreamFrame` N replayed message frames whose
ids are already in the store, then count the timeline requests.

# Acceptance Criteria

- A reload, sign-in or channel change whose stream replay carries only messages the page already
  holds causes no timeline read beyond the one the entry path itself makes (0 on the voice pane, 1
  on the channel pane).
- A replay burst never produces a queued follow-up read of the same newest page.
- A replayed frame the page does not hold yet still reaches the list, with correct thread
  membership. For example, a message that landed while the tab was shut is merged by one read.
- Live (non-replayed) frames keep today's behaviour. The follow-up read after an in-flight snapshot,
  documented at `refreshAfterLiveMutation`, is still issued for them.
- A browser test covers replay-only, replay-with-unknown-id and live-during-read cases by counting
  requests.
