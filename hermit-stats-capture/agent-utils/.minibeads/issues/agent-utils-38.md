---
title: 'read-token-conversation-probe: a read-scope /voice page asks for stored conversations and logs a 403'
status: closed
priority: 1
issue_type: bug
labels:
- vibe-talk
- auth
- ui
created_at: 2026-09-25T20:07:34.870604057+00:00
updated_at: 2026-09-25T20:18:38.669380000+00:00
closed_at: 2026-09-25T20:18:38.669380000+00:00
---

# Description

[opus 5.5] Signed in with a read-scope token, every load of /voice sends `GET /api/v1/conversations`, gets 403, and the browser logs "Failed to load resource" as a console error, although the rest of the page works. The endpoint's scope is right: the conversation and transcript routes are write-scope by design, and a test asserts it. The client is wrong: `loadStoredConversation()` asks unconditionally, because `/api/v1/client-config` accepts both scopes and does not say which one the caller holds, so the page cannot know the request is doomed. A browser logs a console error for any 4xx whatever the page does with it, so the only fix is not to ask.

# Design

`/api/v1/client-config` adds `token_scope` (`read` | `write`): the caller's own scope, which discloses nothing it did not already hold. The page skips the stored-conversation listing and Forget under `read`, says why in Settings, and does not spend its once-per-page restore flag, so saving the write token afterwards restores as before. A server that omits the field keeps the old behaviour. No route's scope changes.

# Acceptance Criteria

A read-scope sign-in makes no request answered 4xx and logs no console error, while the conversation routes still refuse a read token; client-config reports the caller's scope and a test ties it to what the conversation routes enforce; a real-Chromium step fails without the client change; the deployed page under the read credential shows neither the 403 nor the console error.
