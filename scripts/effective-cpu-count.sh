#!/usr/bin/env bash
# Report the CPUs this process can effectively use, including ancestor cgroup quotas.

set -euo pipefail

cpus=${EFFECTIVE_CPU_NPROC:-$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || printf '1')}
[[ $cpus =~ ^[1-9][0-9]*$ ]] || cpus=1

cgroup_root=${EFFECTIVE_CGROUP_ROOT:-/sys/fs/cgroup}
cgroup_path=${EFFECTIVE_CGROUP_PATH:-$(awk -F: '$1 == "0" { print $3; exit }' /proc/self/cgroup 2>/dev/null)}
cgroup_path=${cgroup_path%/}
path=$cgroup_root$cgroup_path

while [[ $path == "$cgroup_root"* ]]; do
    cpu_max=$path/cpu.max
    if [[ -r $cpu_max ]] && read -r quota period <"$cpu_max" &&
        [[ $quota =~ ^[0-9]+$ && $period =~ ^[1-9][0-9]*$ ]]; then
        quota_cpus=$(((quota + period - 1) / period))
        ((quota_cpus > 0)) || quota_cpus=1
        ((quota_cpus >= cpus)) || cpus=$quota_cpus
    fi
    [[ $path != "$cgroup_root" ]] || break
    path=${path%/*}
done

printf '%s\n' "$cpus"
