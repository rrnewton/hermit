---
title: 'voice-chat-write-confirm: let a voice agent post only after a confirmed read-back'
status: closed
priority: 1
issue_type: feature
assignee: opus-5.5/voice-chat-write-confirm
labels:
- vibe-talk
- voice
- chat
depends_on:
  agent-utils-13: discovered-from
  agent-utils-1: parent-child
created_at: 2026-09-25T18:27:30+00:00
updated_at: 2026-09-26T01:33:56.179572691+00:00
closed_at: 2026-09-26T01:33:56.179572561+00:00
claimed_at: 2026-09-26T00:32:25.878995986+00:00
claimed_until: 2026-09-27T00:32:25.878867824+00:00
---

# Description

Split out of #13 voice-chat-tools, whose delivered scope is read-only.

`post_reply` is marked `RequiresApproval` in the tool manifest, and its description tells the model to read the exact text back and get a spoken yes. Nothing outside the model enforces that. Over MCP, a write-scope caller's `post_reply` posts immediately. A provider with a per-tool approval mode can enforce the confirmation, but a bridge without one would leave the decision to the model alone. That is why the internal voice bridge is kept on a read-scope credential.

Wanted: a confirmation gate in the application, so every provider inherits it:

1. Proposing a post does not post. It returns the exact text, the target channel and a short-lived, single-use confirmation handle.
2. Committing requires that handle, plus a confirmation that the model cannot produce on its own, such as an explicit confirm action from the voice UI or a separate user turn the bridge attributes to the speaker.
3. A handle is bound to the exact text, channel and reply target. Any change, expiry or reuse refuses the commit.
4. Refusals and commits are logged without message text at the default level.

# Acceptance Criteria

- Automated: without a confirmation, a write-scope caller cannot post. A confirmed handle posts exactly once. An edited, expired or reused handle is refused. The read-scope tool list is unchanged.
- A voice bridge is given write scope only after the gate lands.
- One live explicit send, run only when the owner explicitly asks for that external write, posts once to a designated writable channel. The same session shows that an unconfirmed request posts nothing.
