---
title: 'late-fork-census: close the post-snapshot inheritance race'
status: open
priority: 1
issue_type: bug
labels:
- wrkslots
- safety
- process-lifecycle
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T14:23:59.002187886+00:00
updated_at: 2026-09-25T14:23:59.002187886+00:00
---

# Description

[gpt-5.6-sol] The same-UID validation-path census snapshots PIDs once, then probes only those generations. A process can fork after enumeration, hand an already-open fenced cwd/fd/mapping/mount to the child, and exit; the successor is absent from every observer input and the census can incorrectly return clear. This is distinct from ordinary unrelated PID disappearance, which must not force global retries.

# Acceptance Criteria

Use exact (pid,start_ticks) identities and a bounded, operation-wide closing inventory or a stronger kernel primitive so every generation live at the acceptance boundary received a complete negative scan. Accumulate authenticated positive matches; never erase them on retry. A matched link, map, mount, or descriptor whose generation becomes ambiguous must hard-refuse. Unrelated ENOENT/ESRCH churn and harmless PID replacement must not restart the full host scan. Add deterministic tests for fork handoff, same-number PID replacement, candidate disappearance after positive evidence, convergent unrelated births, and bounded non-convergence. State the cooperative same-UID/fd-passing threat model explicitly.
