---
title: 'mobile-chat-reader: finish the mobile chat client handoff'
status: open
priority: 0
issue_type: epic
labels:
- vibe-talk
- handoff
created_at: 2026-09-25T01:31:11.140627899+00:00
updated_at: 2026-09-25T02:07:55.156864999+00:00
---

# Description

[gpt-5.6-sol] Coordinate the remaining mobile chat reader work as separate, independently finishable tasks. The implementation must remain provider-neutral in this public repository; deployment-specific details belong in private handoff material.

# Acceptance Criteria

Every child task has a verified outcome or an explicit blocker; the intended-host deployment is reachable from managed mobile Chrome; tracked spaces and thread-level sources can be added without restarting.

# Notes

[gpt-5.6-sol] Validation handoff: Android evaluator self-test passed 9 controls; both browser page suites passed; git diff --check passed. The repository-wide validator passed hygiene, docs, format, build, typecheck, and the first 4,604-test suite, then was interrupted at 67 percent of the cross-language suite on explicit owner instruction to push speculatively and fix forward.
