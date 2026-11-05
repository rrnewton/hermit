---
title: 'cross-runner-test-timing: add common per-case timing telemetry'
status: open
priority: 1
issue_type: task
labels:
- validation
- observability
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T11:45:20.424065054+00:00
updated_at: 2026-09-25T11:45:20.424065054+00:00
---

# Description

[gpt-5.6-sol] Extend the current gate-level timing evidence with one normalized per-case schema for Rust/libtest, JavaScript/browser, cross-language, package, example, and application runners.

# Acceptance Criteria

Every supported runner emits stable case identity, outcome, duration, and owning gate; a single report verifies exact inventory equality without double-counting nested alternate-environment runs; portable validation cost and report size remain bounded.
