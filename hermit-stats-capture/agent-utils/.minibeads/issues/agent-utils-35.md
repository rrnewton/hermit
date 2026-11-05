---
title: 'contained-example-memory: make fixed-memory examples portable on small boxed runners'
status: open
priority: 1
issue_type: task
labels:
- validation
- dagrun
created_at: 2026-09-25T17:13:12.778565056+00:00
updated_at: 2026-09-25T17:13:12.778565056+00:00
---

# Description

[gpt-5.6-sol] A contained four-core audit exposed the broader boundary: the example harness reserves a 16 GiB hard cap so example 08 can execute, while a future small runner with working cgroup delegation may have less than that after safety headroom. Current hosted CI is unboxed and the contained devserver has ample memory, so this is not blocking today.

# Acceptance Criteria

On a cgroup-capable runner with approximately 16 GiB total memory and four CPUs, both example engines validate the documented example-08 contract without lying about capacity, silently skipping it, or weakening hard-cap admission; add a deterministic regression test and document whether the low-memory path executes or proves an expected pre-spawn refusal.
