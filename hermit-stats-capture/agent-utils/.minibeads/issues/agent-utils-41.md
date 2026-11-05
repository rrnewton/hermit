---
title: 'post-gate-scope-first: refuse a read token with 403 before parsing a post-proposal body'
status: closed
priority: 3
issue_type: bug
assignee: opus-5.5/post-gate-scope-first
labels:
- vibe-talk
depends_on:
  agent-utils-34: discovered-from
created_at: 2026-09-26T01:43:29.872090323+00:00
updated_at: 2026-09-26T02:05:58.175307278+00:00
closed_at: 2026-09-26T02:05:58.175307138+00:00
---

# Description

Found after #34 voice-chat-write-confirm was deployed.

A read-scope token that POSTs an invalid body (for example `{}`) to
`/api/v1/post-proposals/commit` gets 422 from the JSON extractor, because the
body is parsed before the handler's scope check runs. A valid body from the
same token gets 403. Nothing leaks and nothing posts, but the refusal should be
uniform: a caller without write scope should get 403 whatever it sends.

Wanted: check scope before the body is parsed on the four post-proposal routes
(and ideally on every write route), and add a test that a read token gets 403
for a malformed body on each.
