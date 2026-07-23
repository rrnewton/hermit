#!/usr/bin/env bash
# Provision the ignored Linux kernel and BusyBox initramfs used by demo 5.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
HERMIT_REPO="${HERMIT_REPO:-$ROOT/hermit}"
ARTIFACT_DIR="${QEMU_ASSETS:-$ROOT/ignored/qemu-linux}"
BUSYBOX="${BUSYBOX:-$(command -v busybox || printf '%s' /usr/sbin/busybox)}"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

if [ -z "${KERNEL_IMAGE:-}" ]; then
  if [ -r /boot/vmlinuz ]; then
    KERNEL_IMAGE="$(readlink -f /boot/vmlinuz)"
  else
    KERNEL_IMAGE="$(find /boot -maxdepth 1 -type f -name 'vmlinuz-*' \
      -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -1 | cut -d' ' -f2- || true)"
  fi
fi

if [ -z "${KERNEL_IMAGE:-}" ] || [ ! -r "$KERNEL_IMAGE" ]; then
  fail "no readable host kernel; set KERNEL_IMAGE=/path/to/bzImage"
fi
if [ -z "$BUSYBOX" ] || [ ! -x "$BUSYBOX" ]; then
  fail "a statically linked BusyBox is required; set BUSYBOX=/path/to/busybox"
fi

for tool in file cpio gzip; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing required tool: $tool"
done
file "$BUSYBOX" | grep -q 'statically linked' || \
  fail "$BUSYBOX is not statically linked"

mkdir -p "$ARTIFACT_DIR" "$HERMIT_REPO/target"

kernel_tmp=""
initrd_tmp=""
workdir=""
cleanup() {
  [ -z "$kernel_tmp" ] || rm -f "$kernel_tmp"
  [ -z "$initrd_tmp" ] || rm -f "$initrd_tmp"
  [ -z "$workdir" ] || rm -rf "$workdir"
}
trap cleanup EXIT

if [ ! -r "$ARTIFACT_DIR/bzImage" ]; then
  kernel_tmp="$ARTIFACT_DIR/.bzImage.$$"
  cp "$KERNEL_IMAGE" "$kernel_tmp"
  mv "$kernel_tmp" "$ARTIFACT_DIR/bzImage"
  kernel_tmp=""
  printf 'kernel: %s -> %s (%s bytes)\n' \
    "$KERNEL_IMAGE" "$ARTIFACT_DIR/bzImage" \
    "$(stat -c%s "$ARTIFACT_DIR/bzImage")"
else
  printf 'kernel: using cached %s (%s bytes)\n' \
    "$ARTIFACT_DIR/bzImage" "$(stat -c%s "$ARTIFACT_DIR/bzImage")"
fi

if [ ! -r "$ARTIFACT_DIR/initramfs.cpio.gz" ]; then
  workdir="$(mktemp -d "$HERMIT_REPO/target/qemu-demo-assets.XXXXXX")"
  root="$workdir/initramfs"
  mkdir -p "$root"/{bin,sbin,etc,proc,sys,dev,tmp,usr/bin,usr/sbin}
  cp "$BUSYBOX" "$root/bin/busybox"
  chmod +x "$root/bin/busybox"

  (
    cd "$root"
    while IFS= read -r applet; do
      mkdir -p "$(dirname "$applet")"
      [ "$applet" = bin/busybox ] || ln -sf /bin/busybox "$applet"
    done < <(./bin/busybox --list-full)
  )

  cat >"$root/init" <<'INIT'
#!/bin/sh
mount -t proc     none /proc 2>/dev/null
mount -t sysfs    none /sys  2>/dev/null
mount -t devtmpfs none /dev  2>/dev/null || mount -t tmpfs none /dev 2>/dev/null
echo "=========================================="
echo "HERMIT-QEMU-BASELINE-BOOT-OK"
echo "kernel: $(uname -r)"
echo "=========================================="
echo "Interactive busybox shell. Type 'poweroff -f' to exit."
exec /bin/sh
INIT
  chmod +x "$root/init"
  printf 'root:x:0:0:root:/:/bin/sh\n' >"$root/etc/passwd"
  printf 'root:x:0:\n' >"$root/etc/group"

  initrd_tmp="$ARTIFACT_DIR/.initramfs.cpio.gz.$$"
  (
    cd "$root"
    find . -print0 | cpio --null -o -H newc 2>/dev/null
  ) | gzip -9 >"$initrd_tmp"
  mv "$initrd_tmp" "$ARTIFACT_DIR/initramfs.cpio.gz"
  initrd_tmp=""
  printf 'initramfs: built %s (%s bytes)\n' \
    "$ARTIFACT_DIR/initramfs.cpio.gz" \
    "$(stat -c%s "$ARTIFACT_DIR/initramfs.cpio.gz")"
else
  printf 'initramfs: using cached %s (%s bytes)\n' \
    "$ARTIFACT_DIR/initramfs.cpio.gz" \
    "$(stat -c%s "$ARTIFACT_DIR/initramfs.cpio.gz")"
fi

printf 'QEMU assets ready in %s\n' "$ARTIFACT_DIR"
