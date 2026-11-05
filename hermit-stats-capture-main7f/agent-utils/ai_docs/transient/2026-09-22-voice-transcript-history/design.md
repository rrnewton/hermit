# A browsable voice transcript, and a way to search it

*2026-09-22. Working notes, not settled documentation.*

## What was asked for

> I would like to store our transcript history for the voice chat. We already have a stateful web
> app. So I figured we could store that lightweight text message history in simple compressed json
> or whatever. This means when I reopen the app I should be able to see my prior conversations. Of
> course we want to show only a bounded suffix of that transcript and demand load if we scroll up.
> This UI feature is orthogonal to any feature that would actually send history to the 11 Labs
> agent to catch it up. I don't think we've implemented anything like that yet. You can confirm.
> I don't want to add it yet in this wave. I just want this group to be persistent so I can browse
> it.
>
> I'd also like to put a little search / magnifying glass icon in the upper right of the app. It
> should filter all the messages on screen for the search terms allowing double quoting to group
> them into one string. It should work both for raw discord messages and for a transcript of the
> voice agent discussion

## What already exists — the confirmation that was asked for

**Storage: already built.** `#48 transcript-storage` put the voice transcript in the server's
SQLite store, not in the DOM. Every turn is POSTed to
`/api/v1/conversations/{id}/turns` as it is spoken, one at a time and in order, and the server
stamps `seq` and `at_ms` on arrival. `storage.max_conversations` (50) and
`storage.max_turns_per_conversation` (1000) bound it.

Not compressed JSON, and deliberately so given what is wanted here: a suffix of a gzip blob cannot
be read without decompressing the whole thing, which is the opposite of demand-loading. A SQLite
table indexed by time answers "the newest 40 turns" without touching the rest.

**Replay to the vendor: ALSO already built, contrary to the assumption in the request.**
`#46 conversation-replay` renders an earlier transcript and hands it to a new call, either as a
`contextual_update` or as a `user_message`. It is **off by default** (`replay.enabled = false`),
because it re-sends earlier conversation content — including third-party channel text the agent
read out — to the voice vendor on every new call. The Settings screen has a switch and a help entry
that says so. Nothing in this wave turns it on or changes it; it is named here only because the
request asked for it to be confirmed and the answer is the opposite of the one expected.

**Restore on reopen: partly built, and not in the shape asked for.** At sign-in the page fetches
`/api/v1/conversations`, takes `conversations[0]` — the single most recent — and loads **all** of
its turns. So:

- Prior conversations, plural, are not shown. Only the latest one is.
- There is no bounded suffix. A 1000-turn conversation is loaded entire.
- There is no demand-loading, because there is nothing left to demand.

## The gap, stated as work

1. **A transcript stream, not a conversation.** What the reader wants to scroll back through is one
   continuous record of everything said, spanning conversations, newest at the bottom. The
   conversation is a boundary inside it, not the unit of browsing.

2. **A paginated read.** `GET /api/v1/conversations/{id}` returns every turn and has no cursor. A
   new route is needed that walks BACKWARD across the whole store.

3. **Demand-load on scroll up**, with the loaded window visibly finite — a reader who reaches the
   top must be told whether that is the beginning of the record or the beginning of what is loaded.

4. **Search**, over what is on screen, both kinds of message.

## Shape of the change

### The order turns are paginated in

Pagination needs a total order that no two rows share and that agrees with per-conversation order.
`(at_ms, conversation_id, seq)` is one: `at_ms` is non-decreasing within a conversation because
turns are recorded one at a time in order, `seq` breaks a millisecond collision inside a
conversation, and `conversation_id` breaks one across two. The cursor is that triple.

This needs an index — the current schema has `PRIMARY KEY (conversation_id, seq)` and nothing
ordered by time — so a migration step is part of it.

### `GET /api/v1/transcript?limit=&before=`

Newest first, so the server can `LIMIT` rather than the page discarding. Each turn carries its
`conversation_id`, so the page can draw a boundary. Answers a `next` cursor and whether there is
more. Write scope, like every other conversation route, and for the same reason: what comes back is
the transcript.

### The page

Replace "load the newest conversation in full" with "load the newest N turns", prepend on scroll to
top, and draw a seam at each conversation boundary rather than one seam at the top. Keep
`resumeConversationId` pointing at the newest conversation: resuming is a separate feature and must
not change behaviour because browsing did.

### Search

A magnifying glass, upper right, filtering what is rendered. Terms split on whitespace,
double quotes group. Applies to the Discord log and the voice transcript alike.

## Open questions

- **Does search filter, or highlight-and-jump?** Filtering is what was asked for ("filter all the
  messages on screen"). Proceeding on filtering.
- **Does search reach unloaded history?** It cannot, without a server-side search route. The honest
  answer on screen is that it searches what is loaded and says so — a filter that silently
  misses matches in unloaded turns would be worse than one that states its range.
