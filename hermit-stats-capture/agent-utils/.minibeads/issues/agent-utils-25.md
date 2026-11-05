---
title: 'test-cost-ranking: justify every regression test by cost'
status: closed
priority: 0
issue_type: task
labels:
- validation
- testing
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T04:10:41.205691928+00:00
updated_at: 2026-09-25T15:31:36.491255619+00:00
closed_at: 2026-09-25T15:31:36.491255619+00:00
---

# Description

[gpt-5.6-sol] Capture machine-readable timing for every executable gate and every `py/` pytest case, attribute other runner cases to measured gates, rank the suite by cost, and record a retain, optimize, relocate, or remove decision for expensive coverage.

# Acceptance Criteria

The report covers every executable gate and every `py/` pytest case with duration and ownership; Rust/libtest, JavaScript/browser, cross-language, package, example, and application cases without a common per-case timing schema are enumerated under measured owning gates and tracked for unified telemetry follow-up; expensive tests have an evidence-backed coverage rationale; fixed waits and redundant coverage are optimized; retained integration tests run only in relevant lanes.

# Follow-up

[gpt-5.6-sol] Cross-runner per-case telemetry beyond the complete gate-level ranking is tracked by #27 cross-runner-test-timing.

# Outcome

[gpt-5.6-sol] Closed with two exact v2 inventories. The source-stable Python census contains 5,882/5,882 passing cases: 4,744 general, 856 mapped-user/PID-namespace lifecycle, and 282 host lifecycle, with 1,261.883 summed testcase seconds and no skips. Its 5,882-row TSV assigns every case ownership, a coverage contract, and a recommendation. The full-graph report contains all 91 green gates from run `18d89844a629049200077887`, including non-pytest runners, their measured wall/CPU/memory cost, rationale, and recommendation. Unified non-pytest per-case telemetry remains the non-blocking follow-up #27 cross-runner-test-timing.
