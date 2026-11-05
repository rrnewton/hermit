---
title: 'transcript-dedup: reconcile partial and final speech text'
status: closed
priority: 0
issue_type: bug
assignee: opus-5.5
labels:
- vibe-talk
- voice
- transcript
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T03:55:15.313114142+00:00
updated_at: 2026-09-25T23:54:35.158742000+00:00
closed_at: 2026-09-25T23:54:35.158742000+00:00
claimed_at: 2026-09-25T05:09:54.971014916+00:00
claimed_until: 2026-09-27T05:09:54.970953774+00:00
---

# Description

Reproduce duplicated incremental/final speech-to-text text, identify the provider event identity and lifecycle, and reconcile updates into one utterance per speaker. Cover repeated partials, corrected finals, reconnects, and distinct identical utterances so valid repetition is not lost.

# Acceptance Criteria

Each spoken utterance appears once after finalization, partial text updates in place, legitimate repeated utterances remain distinct, and the reported examples are covered by tests.
