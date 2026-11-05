---
title: 'read-aloud-latency: measure and reduce time to first audible audio'
status: closed
priority: 0
issue_type: task
assignee: opus-5.5
labels:
- vibe-talk
- audio
- performance
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T01:32:02.842014199+00:00
updated_at: 2026-09-25T20:05:10.305575804+00:00
closed_at: 2026-09-25T20:05:10.305575694+00:00
claimed_at: 2026-09-25T04:17:41.500542020+00:00
claimed_until: 2026-09-27T04:17:41.500399477+00:00
---

# Description

[gpt-5.6-sol] Add enough content-free timing evidence to separate message preparation, provider WebSocket connect, session readiness, first PCM, transfer, and browser playback. Existing logs cannot reconstruct a completed request and the current conversation backend warm-up is a no-op.

# Acceptance Criteria

One real read produces durable phase timings without message content or credentials; the dominant stage is identified; a regression test covers timing instrumentation; a justified latency improvement is implemented or a blocker is documented.

# Notes

The completed request could not be reconstructed: retained server logs contain no phase timing and the browser records none. The streaming path opens a new provider WebSocket per read, waits for session readiness, and does not measure first PCM or audible playback. Add content-free phase metrics before optimizing.

[gpt-5.6-sol] 2026-09-24 evidence: an isolated build on the intended host completed a real provider read of synthetic, non-private text and returned 3,577,004 audio bytes, with the first byte at 3.729 seconds. That probe exposed 112 ms preparation, 80 ms connection, 28 ms readiness, and 3,727 ms first-audio values, but it predated the correction that stops message preparation before provider warm-up; those values overlap and prove the real read but cannot identify the dominant stage. The implementation now records non-overlapping message preparation, provider connection, session readiness, first audio, transfer, browser playback, and tap-to-audible time under an opaque observation id. It also holds a bounded-age ready session during preparation, rejects queued-close sessions, and reconnects if prompt send fails.

[gpt-5.6-sol] Exact remaining blocker: the intended host has no Chrome, Chromium, or Playwright installation and no other runnable browser. The real provider read therefore could not produce a genuine browser loadeddata/playing event, physically audible confirmation, or an end-to-end dominant-stage result. Keep this task open until one instrumented read is initiated in a real browser and its final read-aloud reached audible playback record is retained.

[gpt-5.6-sol] The persistent-session design now serializes prompt/stream ownership and returns a socket to the ready pool only after an explicit turn_complete. Focused regression two_sequential_reads_share_one_session_and_keep_their_audio_separate passed: two sequential speak_stream calls used exactly one accepted WebSocket/session and returned distinct PCM for each message. Cancellation, queued-close, and cached prompt-send failure regressions also passed and force a safe reconnect rather than recycling ambiguous state. Focused results: exact reuse test 1/1; read_aloud integration suite 10/10; API integration suite 88/88; browser-page Node suite 1/1; cargo fmt and clippy passed. Keep #agent-utils-7 read-aloud-latency open: the finalized code has not yet been deployed for a live two-selection measurement, so reuse on the second live selection and a complete browser audible timing record remain unproven.
