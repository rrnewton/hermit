---
name: dagrun
description: Run a dependency graph of validation or build steps under CPU, memory, and named-resource limits. Use when steps can run concurrently but require dependency ordering, resource boxing, memory-aware parallelism, or measured-speedup planning.
---

# dagrun

Use this command to run a DAG of build or validation steps with dependency-aware scheduling, cgroup-v2
resource boxing, memory-aware concurrency, learned estimates, and resource reports.

The installed CLI is the source of truth:

- `dagrun quickstart`
- `dagrun --help`
- `dagrun --userguide`
