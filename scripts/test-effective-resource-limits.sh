#!/usr/bin/env bash

set -euo pipefail

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/parent/leaf"

printf '200000 100000\n' >"$root/cpu.max"
printf 'max 100000\n' >"$root/parent/cpu.max"
printf '350000 100000\n' >"$root/parent/leaf/cpu.max"

actual=$(
    EFFECTIVE_CPU_NPROC=176 \
        EFFECTIVE_CGROUP_ROOT="$root" \
        EFFECTIVE_CGROUP_PATH=/parent/leaf \
        ./scripts/effective-cpu-count.sh
)
[[ $actual == 2 ]] || {
    echo "effective CPU probe: expected ancestor quota 2, got $actual" >&2
    exit 1
}

printf '83886080\n' >"$root/memory.max"
printf '16777216\n' >"$root/memory.current"
printf 'max\n' >"$root/parent/memory.max"
printf '0\n' >"$root/parent/memory.current"
printf '52428800\n' >"$root/parent/leaf/memory.max"
printf '10485760\n' >"$root/parent/leaf/memory.current"

actual=$(
    EFFECTIVE_MEM_AVAILABLE_KIB=102400 \
        EFFECTIVE_CGROUP_ROOT="$root" \
        EFFECTIVE_CGROUP_PATH=/parent/leaf \
        ./scripts/effective-memory-budget.sh
)
[[ $actual == 33554432 ]] || {
    echo "effective memory probe: expected 33554432, got $actual" >&2
    exit 1
}

actual=$(
    EFFECTIVE_CPU_NPROC=176 \
        EFFECTIVE_MEM_AVAILABLE_KIB=268435456 \
        EFFECTIVE_CGROUP_ROOT="$root/missing" \
        EFFECTIVE_CGROUP_PATH=/ \
        ./scripts/effective-dag-jobs.sh
)
[[ $actual == 16 ]] || {
    echo "effective DAG jobs probe: expected bounded width 16, got $actual" >&2
    exit 1
}

echo "test-effective-resource-limits.sh: OK"
