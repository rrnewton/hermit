---
title: 'gchat-thread-selector: make the active conversation easy to choose'
status: closed
priority: 1
issue_type: task
labels:
- vibe-talk
- threads
- ui
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T01:32:03.265269974+00:00
updated_at: 2026-09-25T18:26:00.918304000+00:00
closed_at: 2026-09-25T18:26:00.918304000+00:00
---

# Description

[gpt-5.6-sol] Redesign thread selection so a reader can recognize and choose the thread corresponding to the current conversation. Incorporate the pending owner UX specification before implementation.

# Acceptance Criteria

The current conversation is recognizable without opening unrelated threads; selection works on a narrow mobile viewport; state and scroll behavior remain stable; browser tests cover the interaction.

# Notes

Owner UX details were pending when the handoff was requested. Preserve this as a separate task from thread-list correctness.
