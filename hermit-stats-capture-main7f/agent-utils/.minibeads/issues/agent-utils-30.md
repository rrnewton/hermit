---
title: 'pidfd-process-ownership: make agent cleanup generation-safe'
status: open
priority: 0
issue_type: bug
labels:
- agentctl
- safety
- process-lifecycle
created_at: 2026-09-25T14:07:45.137142234+00:00
updated_at: 2026-09-25T14:07:45.137142234+00:00
---

# Description

[gpt-5.6-sol] Replace PID/PGID-check-then-signal cleanup in the legacy agentctl backend with a durable kernel-bound ownership model. The design must cover runner and whole harness process trees without signaling a PID or group reused after validation.

# Acceptance Criteria

Persist same-boot identity including boot ID; use pidfds or a per-turn owned cgroup for signaling; safely handle leader-dead/child-live groups, fork-during-census, D-state/timeouts, unsupported pidfds, and every migration rollback path; no cleanup error masks the original migration failure; legacy state has an explicit migration policy; adversarial tests prove PID/PGID reuse cannot target an unrelated process.
