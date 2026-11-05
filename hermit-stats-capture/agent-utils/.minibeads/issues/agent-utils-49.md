---
title: 'post-confirmation-card-hold: hold Send when a confirmation card appears where none was shown'
status: closed
priority: 2
issue_type: bug
assignee: opus-5.5/post-confirmation-card-hold
labels:
- vibe-talk
- ui
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-26T10:29:04.019315230+00:00
updated_at: 2026-09-26T10:55:02.646459210+00:00
closed_at: 2026-09-26T10:55:02.646459210+00:00
---

# Description

[opus 5.5] Found in the final doneness audit for #1 mobile-chat-reader, in the #34 voice-chat-write-confirm post gate.

`showPostProposal` in vibe-talk/web/voice.js holds Send for `POST_CHANGE_HOLD_MS` only when a proposal REPLACES one already on screen. When a card appears where none was shown, `previous` is null, no hold is set, and Send is enabled on the first frame the card is visible. A tap already on its way to whatever was under that spot, such as a message row or the transcript, can land on Send and post text the owner has not read.

# Acceptance Criteria

A card that appears where none was shown keeps Send disabled for the same hold as a replaced card, then enables it without further input. A replaced card keeps its hold and its "text changed" notice. Don't send stays usable throughout. A real-browser regression taps Send immediately after a first card appears and shows nothing is posted, and it fails without the fix.

# Notes

[opus 5.5] Delivered in cf21ef9 on main. `showPostProposal` now arms the `POST_CHANGE_HOLD_MS` (2 s) hold on every new card, not only on a replacement; only a replacement sets the "changed the text" notice. Don't send stays usable, and the server-side handle-bound commit is unchanged (no `src/` change). The page suite pins the first-card hold, and the new real-Chromium regression `tests/post_confirm_browser.py` (in `validate-boxed` and `make post-confirm-browser`) taps Send about 25 ms after a first card appears in a typed call. Nothing is committed, Send enables itself after the hold, and the next tap commits once as the UI. Against the previous page the same tap posts. make validate passed. An Opus 5.5 worker reviewed it read-only and approved it.
