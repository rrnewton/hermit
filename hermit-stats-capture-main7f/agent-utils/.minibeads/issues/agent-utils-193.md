---
title: 'compose-draft-loss: never lose a half-typed message to a screen change or a killed app'
status: closed
priority: 1
issue_type: bug
created_at: 2026-10-04T10:17:42.349350643+00:00
updated_at: 2026-10-12T23:36:19.912110250+00:00
closed_at: 2026-10-12T23:36:19.912110120+00:00
---

# Description

[opus 5.5] Owner audit request 2026-10-04: where can typing be lost? Channel composer (vibe-talk.channel-drafts) and reply composer (vibe-talk.voice.drafts) already save on every keystroke and survive reloads and sign-in bounces. Gaps found: (1) the call composer #compose-text saved nothing and folded away on leaving the main screen — fixed: saved per keystroke under vibe-talk.voice.compose-draft, restored on load, field stays open when it holds text; (2) Settings prompt fields saved only on 'change' (blur) — now also on input. Remaining: (3) a failed outbox entry's Dismiss discards its text with one tap and no undo, and there is no Edit that moves it back to the composer; (4) a saved reply draft has no marker on its message row, so after a sign-in bounce the reader must remember which message they were answering; (5) drafts are per-device only — no server-side or cross-device sync.
