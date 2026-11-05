---
title: 'voice-agent-prompt: share the operational system prompt'
status: closed
priority: 0
issue_type: feature
assignee: opus-5.5
labels:
- vibe-talk
- voice
- prompt
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T03:55:15.310333640+00:00
updated_at: 2026-09-25T23:54:35.158742000+00:00
closed_at: 2026-09-25T23:54:35.158742000+00:00
claimed_at: 2026-09-25T05:09:54.970910729+00:00
claimed_until: 2026-09-27T05:09:54.970846703+00:00
---

# Description

Check in a provider-neutral voice-agent system prompt based on the owner-supplied ElevenLabs prompt. It should default to catching the user up, suppress refuted agent theories, abbreviate hashes and paths, use Eastern Time, retrieve detail when asked, send only on explicit request, address provider-specific mention syntax through supplied capability metadata, and wait without filler. Wire the internal provider to receive it and document how hosted providers consume the same prompt.

# Acceptance Criteria

The prompt is versioned in OSS without provider-specific deployment details, the internal agent receives it, hosted-provider setup references it, and tests prevent silent prompt omission.
