#!/usr/bin/env bash
# Provision the fixed Linux kernel and BusyBox initramfs used by demo 5.

set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$LIB_DIR/../.." && pwd)"
HERMIT_REPO="${HERMIT_REPO:-$ROOT/hermit}"
ARTIFACT_DIR="${QEMU_ASSETS:-$ROOT/ignored/qemu-linux}"
BUSYBOX="${BUSYBOX:-$(command -v busybox || printf '%s' /usr/sbin/busybox)}"
KERNEL_SHA256="${QEMU_KERNEL_SHA256:-e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2}"
KERNEL_MANIFOLD_PATH="${QEMU_KERNEL_MANIFOLD_PATH:-test/tree/dev-hermit/qemu-kernels/$KERNEL_SHA256/bzImage}"
INITRAMFS_VERSION=3
INITRAMFS_VERSION_FILE="$ARTIFACT_DIR/.initramfs-version"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

size_mb() {
  awk -v bytes="$1" 'BEGIN { printf "%.1f", bytes / 1000000 }'
}

if [ -z "$BUSYBOX" ] || [ ! -x "$BUSYBOX" ]; then
  fail "a statically linked BusyBox is required; set BUSYBOX=/path/to/busybox"
fi

for tool in file cpio gzip sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing required tool: $tool"
done
file "$BUSYBOX" | grep -q 'statically linked' || \
  fail "$BUSYBOX is not statically linked"
[[ $KERNEL_SHA256 =~ ^[0-9a-f]{64}$ ]] || \
  fail "QEMU_KERNEL_SHA256 must be a lowercase 64-character SHA-256"

mkdir -p "$ARTIFACT_DIR" "$HERMIT_REPO/target"

kernel_tmp=""
initrd_tmp=""
version_tmp=""
workdir=""
cleanup() {
  [ -z "$kernel_tmp" ] || rm -f "$kernel_tmp"
  [ -z "$initrd_tmp" ] || rm -f "$initrd_tmp"
  [ -z "$version_tmp" ] || rm -f "$version_tmp"
  [ -z "$workdir" ] || rm -rf "$workdir"
}
trap cleanup EXIT

cached_kernel_sha=""
if [ -r "$ARTIFACT_DIR/bzImage" ]; then
  cached_kernel_sha="$(sha256sum "$ARTIFACT_DIR/bzImage" | cut -d' ' -f1)"
fi

if [ "$cached_kernel_sha" != "$KERNEL_SHA256" ]; then
  if [ -n "$cached_kernel_sha" ]; then
    printf 'kernel: replacing cache with unexpected sha256 %s\n' \
      "$cached_kernel_sha"
  fi
  kernel_tmp="$ARTIFACT_DIR/.bzImage.$$"
  if [ -n "${KERNEL_IMAGE:-}" ]; then
    [ -r "$KERNEL_IMAGE" ] || fail "unreadable KERNEL_IMAGE: $KERNEL_IMAGE"
    cp "$KERNEL_IMAGE" "$kernel_tmp"
    kernel_source="$KERNEL_IMAGE"
  else
    command -v manifold >/dev/null 2>&1 || \
      fail "manifold is required to download manifold://$KERNEL_MANIFOLD_PATH"
    echo 'Downloading kernel from Manifold...'
    manifold --quiet get --threads 20 \
      "$KERNEL_MANIFOLD_PATH" "$kernel_tmp" >/dev/null 2>&1 || \
      fail "kernel download failed: manifold://$KERNEL_MANIFOLD_PATH"
    kernel_source="manifold://$KERNEL_MANIFOLD_PATH"
  fi

  downloaded_kernel_sha="$(sha256sum "$kernel_tmp" | cut -d' ' -f1)"
  if [ "$downloaded_kernel_sha" != "$KERNEL_SHA256" ]; then
    fail "kernel sha256 mismatch from $kernel_source: expected $KERNEL_SHA256, got $downloaded_kernel_sha"
  fi
  mv "$kernel_tmp" "$ARTIFACT_DIR/bzImage"
  kernel_tmp=""
  kernel_bytes="$(stat -c%s "$ARTIFACT_DIR/bzImage")"
  printf '✓ Kernel ready (%sMB)\n' "$(size_mb "$kernel_bytes")"
else
  kernel_bytes="$(stat -c%s "$ARTIFACT_DIR/bzImage")"
  printf '✓ Kernel ready (%sMB, cached)\n' "$(size_mb "$kernel_bytes")"
fi

cached_initramfs_version="$(cat "$INITRAMFS_VERSION_FILE" 2>/dev/null || true)"
if [ ! -r "$ARTIFACT_DIR/initramfs.cpio.gz" ] || \
   [ "$cached_initramfs_version" != "$INITRAMFS_VERSION" ]; then
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
echo "rtc: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "=========================================="
echo "Interactive busybox shell. Type 'poweroff -f' to exit."
exec setsid cttyhack sh
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
  version_tmp="$ARTIFACT_DIR/.initramfs-version.$$"
  printf '%s\n' "$INITRAMFS_VERSION" >"$version_tmp"
  mv "$version_tmp" "$INITRAMFS_VERSION_FILE"
  version_tmp=""
  printf '✓ Initramfs ready (%sMB)\n' \
    "$(size_mb "$(stat -c%s "$ARTIFACT_DIR/initramfs.cpio.gz")")"
else
  printf '✓ Initramfs ready (%sMB, cached)\n' \
    "$(size_mb "$(stat -c%s "$ARTIFACT_DIR/initramfs.cpio.gz")")"
fi

echo 'QEMU assets ready.'
