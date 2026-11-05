---
title: 'stream-trailing-byte: do not silently drop a partial final sample in read-aloud streaming'
status: closed
priority: 2
issue_type: bug
assignee: opus-5.5/stream-trailing-byte
labels:
- vibe-talk
- audio
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-26T10:29:04.019315230+00:00
updated_at: 2026-09-26T10:55:02.646459210+00:00
closed_at: 2026-09-26T10:55:02.646459210+00:00
---

# Description

[opus 5.5] Found in the final doneness audit for #1 mobile-chat-reader.

The streamed read-aloud player in vibe-talk/web/voice.js decodes whole PCM frames and keeps any remainder in `carry` for the next chunk. When the stream ends, a non-empty `carry` is discarded without a word. An odd final byte, or on stereo any partial frame, is dropped, and the read still counts as finished. The page archives a finished read as heard. So a body cut short by one byte, or a WAV whose data length disagrees with its sample size, passes as a clean end.

# Acceptance Criteria

At end of stream, a non-empty remainder is handled explicitly. Either the read is treated as not having arrived whole (the same path as a response cut off part-way, so the message is not archived), or the remainder is logged and the choice is documented where `carry` is kept. Regression tests cover an even-length body (plays, ends, archives), an odd trailing byte, and a partial stereo frame, and they fail without the fix.

# Notes

[opus 5.5] Delivered in cf21ef9 on main. A non-empty `carry` at the end of a streamed body now takes the same path as a response cut off part-way: the audio stops, the page says it could not be played, and nothing is archived. Page tests cover whole stereo frames (plays, ends, archives), an odd trailing byte, half a stereo frame, and a stereo frame short by one byte, and they fail without the fix. make validate passed. An Opus 5.5 worker reviewed it read-only and approved it. Follow-up for the next deploy (not done here; no deploy was in scope): make one real streamed read to confirm the live provider's PCM total is always a whole number of samples. If it is not, every streamed read would now fail loudly where it used to pass.
