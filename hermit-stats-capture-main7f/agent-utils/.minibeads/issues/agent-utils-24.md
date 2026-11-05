---
title: 'validation-parallelism: execute the full contract efficiently'
status: closed
priority: 0
issue_type: task
labels:
- validation
- performance
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T04:10:40.477016914+00:00
updated_at: 2026-09-25T15:31:36.491255619+00:00
closed_at: 2026-09-25T15:31:36.491255619+00:00
---

# Description

[gpt-5.6-sol] Turn the complete portable validation contract into a profiled dagrun graph and tune representative outer plus inner concurrency from measured sweeps.

# Acceptance Criteria

A clean full validation completes under 10 minutes on the target host; plans expose the critical path and resource choices; independent work overlaps safely; a measured lifecycle outer-width sweep and cold Cargo inner-width sweep identify stable knees without excessive CPU; the final full-graph profile shows remaining utilization and critical-path limits; selective runs remain substantially shorter.

# Outcome

[gpt-5.6-sol] Closed from green run `18d89844a629049200077887`: 91/91 gates, 407.988 seconds graph wall, 3,280.088 aggregate step-wall seconds, 2,995.912 aggregate CPU-seconds, and zero positive memory-limit events. The measured lifecycle sweep selected outer width five; the cold Cargo sweep selected eight jobs as the smallest width within 5% of the fastest sample. The final profile identifies `cross.dagrun.differential` as the dominant critical path at 404.467 seconds and 482.479 CPU-seconds, so further full-run improvement belongs inside that gate rather than in more outer concurrency.
