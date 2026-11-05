---
title: 'delegated-cpuset: run hard pinning inside a parent-owned cgroup'
status: open
priority: 1
issue_type: task
labels:
- dagrun
- validation
- cgroups
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T12:56:03.209320471+00:00
updated_at: 2026-09-25T12:56:03.209320471+00:00
---

# Description

[gpt-5.6-sol] Add a delegated-root backend for cpuset-alloc and pin-run so live hard-pinning tests can execute inside the aggregate validation DAG without systemd moving workloads into sibling user scopes outside the owning gate.

# Acceptance Criteria

When DAGRUN_DELEGATED_CGROUP is valid, cpuset-alloc and pin-run create, verify, use, and clean a private child cgroup beneath it; no child migrates outside the parent-owned validation subtree; aggregate differential and pytest coverage runs every live apply/release, interop, help passthrough, selftest, and signal leg; standalone behavior remains compatible.
