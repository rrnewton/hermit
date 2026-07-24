#!/bin/sh
# Busybox initramfs init: boot, run guest programs, power off.
mount -t proc     none /proc  2>/dev/null
mount -t sysfs    none /sys   2>/dev/null
mount -t devtmpfs none /dev   2>/dev/null || mount -t tmpfs none /dev 2>/dev/null

# Ensure a working console on the serial line.
[ -c /dev/ttyS0 ] || mknod /dev/ttyS0 c 4 64 2>/dev/null

echo "=========================================="
echo "HERMIT-QEMU-BASELINE-BOOT-OK"
echo "kernel: $(uname -r)"
echo "=========================================="

echo "===GUEST-PROGRAMS-BEGIN==="

echo "--- [1] uname -a ---"
uname -a

echo "--- [2] ls / ---"
ls /

echo "--- [3] cat /proc/cpuinfo | head -10 ---"
cat /proc/cpuinfo | head -10

echo "--- [4] echo ---"
echo 'Hello from hermit-controlled QEMU Linux!'

echo "--- [5] su demo -c 'id && echo hello from demo user' ---"
su demo -c 'id && echo hello from demo user'

echo "===GUEST-PROGRAMS-END==="
echo "HERMIT-QEMU-GUEST-PROGRAMS-DONE"

# Power off cleanly so the run terminates (needed for --verify's two runs).
poweroff -f
