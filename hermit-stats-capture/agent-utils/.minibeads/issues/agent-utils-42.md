---
title: 'api-scope-first: check scope before parsing on every /api route'
status: closed
priority: 3
issue_type: bug
assignee: opus-5.5/api-scope-first
labels:
- vibe-talk
depends_on:
  agent-utils-41: discovered-from
created_at: 2026-09-26T01:56:37.351739869+00:00
updated_at: 2026-09-26T02:57:13.850411608+00:00
closed_at: 2026-09-26T02:57:13.850411508+00:00
---

# Description

[opus 5.5] Discovered while landing #41 post-gate-scope-first, which moved the scope check ahead of
request parsing only on the routes that propose, confirm, withdraw, read a proposal or post: the four
`/api/v1/post-proposals*` routes, `/api/v1/channels/{id}/reply` and `/api/v1/channels/{id}/ask`.
Those now take the `WriteScope` extractor in `src/http/api.rs` before `Path`, `Query` or `Json`.

Every other `/api/` route still calls `require(&headers, &state, ...)` inside the handler, after axum
has already run its extractors. A caller without the right scope therefore gets the parser's 400, 413
or 422 instead of 403 (read token) or 401 (no token) whenever its request is malformed.

Still parse-first, write scope:
- `set_alias`, `append_turn`, `mark_read_upstream`, `mark_read`, `dismiss`, `restore` (Path + Json).
  `mark_read_upstream` is the closest to chat-facing: it calls the provider's mark-read.
- `add_channel` (Json); `transcript`, `channel_directory` (Query)
- `voice_timing`, `voice_health` (Bytes; `voice_health` has a small body limit, so an oversize body
  from a read token gets 413 before 403)
- path-only routes such as `clear_alias`, `conversation`, `replay`, `forget_conversation`,
  `forget_read_mark` (`Path<String>` answers 400 for an undecodable id like `%FF`)

Still parse-first, read scope (an anonymous caller gets 400/422, not 401, although `src/http/mod.rs`
says everything under `/api/` requires a bearer token): `speak`, `messages`, `page`, `timeline`,
`count`, `message_by_id`, `digest`, `resolve`, `todo`, `message_summary`, `prepare_speech`.

Wanted: one scope-check style for the whole API. Either move every write route to `WriteScope` and
add a matching `ReadScope`, or authenticate in a router-level layer that answers 401 before any
extractor runs and leave only the write/read distinction to the handler. Two styles in one file
invite the next route to copy the old one.

Acceptance: a regression test in the style of
`a_caller_without_write_scope_is_refused_before_its_request_is_parsed` (tests/post_gate.rs) that
covers every `/api/` route, comparing each malformed request's answer with the well-formed one's.
