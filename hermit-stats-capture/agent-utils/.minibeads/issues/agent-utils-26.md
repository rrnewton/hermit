---
title: 'validation-flakes: eliminate environment and deadline failures'
status: closed
priority: 0
issue_type: task
labels:
- validation
- reliability
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T04:10:41.859585672+00:00
updated_at: 2026-09-25T15:31:36.491255619+00:00
closed_at: 2026-09-25T15:31:36.491255619+00:00
---

# Description

[gpt-5.6-sol] Make validation deterministic across supported local and CI environments, including known deadline, PATH, and process-cleanup failures.

# Acceptance Criteria

Known deadline and environment flakes are reproduced or classified; tests use explicit prerequisites and robust synchronization; repeated targeted and full runs have no unexplained failures.

# Outcome

[gpt-5.6-sol] Closed after replacing PATH-sensitive setup, fixed readiness assumptions, shared temp-root collisions, in-place fixture publication, terminal-result publication races, and load-sensitive timing windows with explicit prerequisites or synchronization. Dagrun, agentctl, and wrkslots now parse opaque procfs records as bytes, reject forged status fields, distinguish definite disappearance from unreadable/malformed identity, and avoid treating incomplete scans as a clean result. The final 91-gate run and independent 5,882-case census are green with no unexplained failure. Aggregate-only live cgroup omissions are loud, intentional safety skips tracked by #28 delegated-cpuset and #29 delegated-scope-smokes; durable signaling and the late-fork census boundary remain #30 pidfd-process-ownership and #31 late-fork-census.
