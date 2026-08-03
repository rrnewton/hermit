#!/usr/bin/env bash
# Bound outer DAG fan-out by effective CPUs and conservative memory per active step.

set -euo pipefail

cpus=$(./scripts/effective-cpu-count.sh)
memory_budget=$(./scripts/effective-memory-budget.sh)
jobs_from_memory=$((memory_budget / (8 * 1024 * 1024 * 1024)))
((jobs_from_memory > 0)) || jobs_from_memory=1

jobs=$cpus
((jobs <= jobs_from_memory)) || jobs=$jobs_from_memory
# The portable graph has few simultaneously ready independent nodes; a larger
# outer width adds target/cache contention while Cargo already owns inner CPU use.
((jobs <= 16)) || jobs=16
printf '%s\n' "$jobs"
