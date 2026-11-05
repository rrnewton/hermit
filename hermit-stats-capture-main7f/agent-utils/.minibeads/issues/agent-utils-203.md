---
title: 'incremental-refresh: refresh fetches only what is new since the last read'
status: closed
priority: 0
issue_type: feature
created_at: 2026-10-04T12:42:05.442191565+00:00
updated_at: 2026-10-04T16:03:06.878580114+00:00
closed_at: 2026-10-04T16:03:06.878579994+00:00
---

# Description

[opus 5.5] Owner, 2026-10-04 08:19, answering whether, after a fetch at 7:00, a refresh at 7:15 could ask our backend trait and the bridge's provider for just that time range, and whether that would be one read for the channel or one per thread: "This is a really important overhaul to prioritize. This is the common case inner loop for how we use the application all the time. It is important for this to be efficient. Right now my refreshes feel very slow."

Today neither layer can read just what is new. `ChatClient::fetch_timeline` reads backward only (a newest page, then opaque `before` cursors), so every poll, pull-to-refresh, and live message on a threaded channel re-reads the newest page of the whole view, and a bridge that keeps one snapshot per channel re-scans the whole history whenever that snapshot is a few minutes old.

Design (the vibe-talk half; the bridge half is the bridge's own work):

1. Forward cursor. Every newest timeline read returns `next_after` for its view (channel + view + thread). A read with `after=<next_after>` returns only the entries new or changed in that view since then, oldest first, at most `limit`, with `delta: {more, complete, deleted, removed_threads}`; `has_more` is false and `next_before` null on a delta. `after` and `before` are mutually exclusive (400). A cursor for another channel, view or thread is 400 `cursor_mismatch`; a cursor the backend can no longer answer is 410 `cursor_expired`, and the caller does a full newest read. A page may carry `as_of` when the backend is serving data it could not bring up to date.
2. vibe-talk wraps every forward cursor in an envelope bound to channel, view and thread. A backend that issues its own `next_after` (the bridge, the fake) gets it back untouched; any other backend gets a generic cursor - provider timestamps with a 120 s overlap and a seen set - answered by a default `ChatClient::fetch_timeline_since` that reads newest pages and filters them: correct, with `complete: false`, and no cheaper than a newest read. The router forwards it to the channel's provider.
3. Bridge contract: a bridge advertises forward reads only by including `next_after`; vibe-talk passes `after` through, validates the delta shape, and maps 410 (and 400/405/501 after a bridge rollback) to `cursor_expired`.
4. HTTP route `GET /api/v1/channels/{id}/timeline` gains `after`; contract samples and the generated schema/validators cover `next_after`, `delta`, `as_of`.
5. Page: on a channel whose deltas are complete, polls, live messages, edits and deletes, pull-to-refresh, and returning to a page hidden for 15 s or more read a delta and fold it into the view's cover (upserts, deleted ids, removed threads; never deletion by absence; coverage unchanged); a full read still happens on view entry, stream reset, an expired or mismatched cursor, a delta that runs past 4 pages, and every 15 minutes. Channels whose deltas are incomplete keep their full 45 s poll, so edit/delete latency does not regress. The freshness pill keeps its meaning; an old `as_of` shows the existing "slow" sentence.

Acceptance: Rust tests for the envelope, generic fallback, bridge parsing and status mapping, fake native and generic paths through the route, the router with mixed providers, and the contract; page tests for the delta fold, no duplicates, cursor-error fallback, the reconcile timer, and view switches; full page suite green.
