#!/usr/bin/env bash
# Memory profiling harness for hermit+QEMU vs bare QEMU.
#
# NOTE: this host runs MANY concurrent qemu processes (other agents). We must
# NOT identify "our" qemu by pattern matching. Instead we record the exact PID
# we launch and walk its descendant tree to find the qemu process.
set -uo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT" || exit 1

HERMIT_BIN="${HERMIT_BIN:-target/release/hermit}"
QEMU_BIN="$(command -v qemu-system-x86_64)"
BUSYBOX_BIN="/usr/sbin/busybox"
KERNEL_REAL="$(readlink -f /boot/vmlinuz)"
SAMPLE_AT="${SAMPLE_AT:-10}"
GUEST_SLEEP="${GUEST_SLEEP:-25}"

mkdir -p "$REPO_ROOT/target"
WORKDIR="$(mktemp -d "$REPO_ROOT/target/qemu-mem.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT
ROOT="$WORKDIR/initramfs"; INITRD="$WORKDIR/initramfs.cpio.gz"

mkdir -p "$ROOT"/{bin,proc,sys,dev}
cp "$BUSYBOX_BIN" "$ROOT/bin/busybox"
for app in sh cat mount uname poweroff echo sleep mknod sync grep; do
    ln -sf busybox "$ROOT/bin/$app"
done
cat > "$ROOT/init" <<GUEST_INIT
#!/bin/busybox sh
export PATH=/bin
mount -t proc none /proc 2>/dev/null
echo "MEM-PROFILE-BOOT-OK \$(uname -r)"
grep -m1 MemTotal /proc/meminfo
sleep ${GUEST_SLEEP}
sync
poweroff -f
GUEST_INIT
chmod +x "$ROOT/init"
( cd "$ROOT" && find . -print0 | cpio --null --create --format=newc 2>/dev/null | gzip -9 > "$INITRD" )

QEMU_FLAGS_COMMON=( -accel tcg,thread=single -smp 1 -icount shift=0,sleep=off
             -kernel "$KERNEL_REAL" -initrd "$INITRD"
             -display none -serial file:CONSOLE -monitor none -no-reboot
             -append "console=ttyS0 panic=-1 rdinit=/init" )
HERMIT_FLAGS=( --log error run --no-sequentialize-threads --preemption-timeout 10000000000 )

# All descendant PIDs of $1 (inclusive), via repeated ppid walk.
descendants() {
    local root="$1" frontier="$1" next all="$1" p kids
    for _ in 1 2 3 4 5 6; do
        next=""
        for p in $frontier; do
            kids="$(pgrep -P "$p" 2>/dev/null)"
            [[ -n "$kids" ]] && { next="$next $kids"; all="$all $kids"; }
        done
        frontier="$next"
        [[ -z "${frontier// }" ]] && break
    done
    echo "$all"
}

find_qemu() {  # among descendants of $1, print pid whose comm == qemu-system-x86
    local pid
    for pid in $(descendants "$1"); do
        [[ -r "/proc/$pid/comm" ]] || continue
        if grep -q '^qemu-system-x86' "/proc/$pid/comm" 2>/dev/null; then
            echo "$pid"; return
        fi
    done
}

vmrss() { awk '/^VmRSS:/{print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }
statline() { awk '/^VmRSS:|^VmSize:|^VmPeak:|^VmHWM:/{printf "%s=%s ",$1,$2}' "/proc/$1/status" 2>/dev/null; echo; }
mib() { awk -v k="${1:-0}" 'BEGIN{printf "%.1f", k/1024}'; }

run_case() {
    local label="$1" mem="$2" mode="$3"; shift 3   # mode: hermit | bare
    local console="$WORKDIR/${label}.console"; : > "$console"
    local -a cmd=( "$@" )
    # substitute the CONSOLE placeholder
    local i; for i in "${!cmd[@]}"; do cmd[$i]="${cmd[$i]/CONSOLE/$console}"; done
    echo "=================================================================="
    echo "CASE: $label   (-m $mem, mode=$mode)"
    "${cmd[@]}" >/dev/null 2>&1 &
    local launch_pid=$!
    sleep "$SAMPLE_AT"
    local qpid hpid=""
    if [[ "$mode" == "hermit" ]]; then
        hpid="$launch_pid"
        qpid="$(find_qemu "$launch_pid")"
    else
        # launched process IS qemu (or its direct child)
        if grep -q '^qemu-system-x86' "/proc/$launch_pid/comm" 2>/dev/null; then
            qpid="$launch_pid"
        else
            qpid="$(find_qemu "$launch_pid")"
        fi
    fi
    local q_rss=0 h_rss=0
    echo "  launch_pid=$launch_pid  qemu_pid=${qpid:-none}  hermit_pid=${hpid:-n/a}"
    if [[ -n "$qpid" ]]; then echo "  qemu   : $(statline "$qpid")"; q_rss="$(vmrss "$qpid")"; fi
    if [[ -n "$hpid" ]]; then echo "  hermit : $(statline "$hpid")"; h_rss="$(vmrss "$hpid")"; fi
    local total=$(( q_rss + h_rss ))
    echo "  RSS: qemu=$(mib $q_rss) MiB  hermit=$(mib $h_rss) MiB  combined=$(mib $total) MiB"
    wait "$launch_pid" 2>/dev/null
    echo "  console: $(head -c 200 "$console" | tr '\n' '|')"
    echo "RESULT-ROW $label mem=$mem qemu_rss_kb=$q_rss hermit_rss_kb=$h_rss combined_kb=$total"
}

for mem in ${MEMS:-256M 128M 64M}; do
    run_case "hermit+qemu-$mem" "$mem" hermit "$HERMIT_BIN" "${HERMIT_FLAGS[@]}" -- "$QEMU_BIN" -m "$mem" "${QEMU_FLAGS_COMMON[@]}"
    run_case "bare-qemu-$mem"   "$mem" bare   "$QEMU_BIN" -m "$mem" "${QEMU_FLAGS_COMMON[@]}"
done
echo "=================================================================="
echo "DONE"
