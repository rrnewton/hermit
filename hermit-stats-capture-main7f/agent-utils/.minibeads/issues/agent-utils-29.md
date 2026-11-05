---
title: 'delegated-scope-smokes: preserve top-level boxing coverage outside delegated validation'
status: open
priority: 1
issue_type: task
labels:
- dagrun
- validation
- cgroups
depends_on:
  agent-utils-21: parent-child
created_at: 2026-09-25T13:01:19.795276722+00:00
updated_at: 2026-09-25T13:01:19.795276722+00:00
---

# Description

[gpt-5.6-sol] Preserve live fresh-top-level systemd scope coverage without letting aggregate validation tests migrate into sibling units outside their parent-owned delegated gate.

# Acceptance Criteria

The complete validation contract runs the forced-box, observed-live-pid, boxed-journal, Python symlink re-exec, Python stdin re-exec, and Rust `boxed_stdin_smoke` legs in a containment-safe lane; aggregate validation never creates a sibling user scope outside the owning gate; standalone focused tests retain equivalent live behavior for every leg; aggregate safety skips name each omitted leg in test output; skip policy and capability requirements are explicit and machine-readable.
