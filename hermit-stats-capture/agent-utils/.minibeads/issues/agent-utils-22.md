---
title: 'validation-selection: route each change to affected tests'
status: closed
priority: 0
issue_type: task
labels:
- validation
- selection
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T04:10:39.074004671+00:00
updated_at: 2026-09-25T15:31:36.491255619+00:00
closed_at: 2026-09-25T15:31:36.491255619+00:00
---

# Description

[gpt-5.6-sol] Replace coarse directory buckets with dependency-aware ownership and close known selection gaps without weakening unknown-path fail-safe behavior.

# Acceptance Criteria

Documentation-only and single-tool changes select only relevant checks plus hygiene; multi-area changes select the union and reverse dependencies; every tracked path and test has an owner; planner output explains every selection; local and CI consume the same plan.

# Outcome

[gpt-5.6-sol] Closed with `validation/components.json` owning 185 ordinary Python test files across nine components plus seven separately partitioned suite files. Selector self-tests cover unknown paths, untracked paths, both sides of renames, union semantics, reverse dependencies, manifest omissions, and duplicate ownership. Both selected and full execution load `validation.dag.yaml`; CI no longer maintains a parallel command list. Historical end-to-end proofs measured a documentation-only selection at seven gates and 1.5 seconds total, and a tick-hub component selection at 17 gates and 14.76 seconds total.
