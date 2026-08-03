#!/usr/bin/env bash
# Report a safe DAG memory budget from host availability and ancestor cgroup headroom.

set -euo pipefail

available_kib=${EFFECTIVE_MEM_AVAILABLE_KIB:-$(awk '$1 == "MemAvailable:" { print $2; exit }' /proc/meminfo)}
[[ $available_kib =~ ^[1-9][0-9]*$ ]] || available_kib=1048576
available=$((available_kib * 1024))

cgroup_root=${EFFECTIVE_CGROUP_ROOT:-/sys/fs/cgroup}
cgroup_path=${EFFECTIVE_CGROUP_PATH:-$(awk -F: '$1 == "0" { print $3; exit }' /proc/self/cgroup 2>/dev/null)}
cgroup_path=${cgroup_path%/}
path=$cgroup_root$cgroup_path

while [[ $path == "$cgroup_root"* ]]; do
    memory_max=$path/memory.max
    memory_current=$path/memory.current
    if [[ -r $memory_max && -r $memory_current ]] &&
        read -r maximum <"$memory_max" && [[ $maximum =~ ^[0-9]+$ ]]; then
        current=$(<"$memory_current")
        [[ $current =~ ^[0-9]+$ ]] || current=0
        headroom=$((maximum - current))
        ((headroom > 0)) || headroom=1
        ((headroom >= available)) || available=$headroom
    fi
    [[ $path != "$cgroup_root" ]] || break
    path=${path%/*}
done

# Leave room for the runner, the OS, and unrelated processes in the same parent.
budget=$((available * 4 / 5))
((budget > 0)) || budget=1
printf '%s\n' "$budget"
