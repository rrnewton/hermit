#!/usr/bin/env bash
# Provision the fixed Linux kernel and BusyBox initramfs used by demo 5.

set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$LIB_DIR/../.." && pwd)"
HERMIT_REPO="${HERMIT_REPO:-$ROOT/hermit}"
ARTIFACT_DIR="${QEMU_ASSETS:-$ROOT/ignored/qemu-linux}"
BUSYBOX="${BUSYBOX:-$(command -v busybox || true)}"
KERNEL_SHA256="${QEMU_KERNEL_SHA256:-e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2}"
DEFAULT_KERNEL_URL="https://github.com/rrnewton/dev-hermit/releases/download/qemu-kernel-$KERNEL_SHA256/bzImage"
KERNEL_URL="${QEMU_KERNEL_URL:-$DEFAULT_KERNEL_URL}"
KERNEL_MANIFOLD_PATH="${QEMU_KERNEL_MANIFOLD_PATH:-}"
QEMU="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
PYTHON="${QEMU_DEMO_PYTHON:-$(command -v python3 || true)}"
INITRAMFS_VERSION=3
INITRAMFS_VERSION_FILE="$ARTIFACT_DIR/.initramfs-version"
CHECK_ONLY=0
# Bounds for the direct-connectivity smoke test in fetch_url (seconds).
FETCH_CONNECT_TIMEOUT="${QEMU_FETCH_CONNECT_TIMEOUT:-10}"
FETCH_PROBE_TIMEOUT="${QEMU_FETCH_PROBE_TIMEOUT:-20}"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

case "${1:-}" in
  "") ;;
  --check) CHECK_ONLY=1 ;;
  *) fail "usage: $0 [--check]" ;;
esac

size_mb() {
  awk -v bytes="$1" 'BEGIN { printf "%.1f", bytes / 1000000 }'
}

available_executable() {
  [ -n "$1" ] || return 1
  case "$1" in
    */*) [ -x "$1" ] ;;
    *) command -v "$1" >/dev/null 2>&1 ;;
  esac
}

# Download a URL to a file, portably across open-internet and proxied networks.
# Strategy, in order:
#   1. Smoke-test a direct connection to the asset host (a lightweight, bounded
#      HEAD request).
#   2. If the direct probe succeeds, download directly (external machines, and
#      any host whose environment already routes to the internet).
#   3. If the direct probe fails but an optional `with-proxy` helper is on PATH,
#      retry the whole fetch through it (networks that require an egress proxy).
#   4. Otherwise fail with actionable guidance.
# No network-specific details are hardcoded: the only environment assumption is
# probing for an optional `with-proxy` command. curl additionally honors any
# http_proxy / https_proxy / ALL_PROXY variables already present in either the
# direct or the with-proxy attempt.
fetch_url() {
  local url="$1" out="$2"

  if curl --fail --location --silent --show-error --head \
       --connect-timeout "$FETCH_CONNECT_TIMEOUT" \
       --max-time "$FETCH_PROBE_TIMEOUT" \
       "$url" -o /dev/null 2>/dev/null; then
    curl --fail --location --silent --show-error "$url" --output "$out"
    return $?
  fi

  if command -v with-proxy >/dev/null 2>&1; then
    echo '  direct connection failed; retrying through with-proxy...' >&2
    with-proxy curl --fail --location --silent --show-error \
      "$url" --output "$out"
    return $?
  fi

  fail "cannot reach $url: direct connection failed and no 'with-proxy' helper is on PATH. Set http(s)_proxy for your network, or provide the kernel locally via KERNEL_IMAGE=/path/to/bzImage or QEMU_KERNEL_MANIFOLD_PATH."
}

preflight() {
  local issue
  local -a issues=()

  available_executable "$QEMU" || \
    issues+=("missing qemu-system-x86_64 (or set QEMU_BIN=/path/to/qemu)")
  command -v qemu-img >/dev/null 2>&1 || \
    issues+=("missing qemu-img")
  available_executable "$PYTHON" || \
    issues+=("missing Python 3 (or set QEMU_DEMO_PYTHON=/path/to/python3)")

  for tool in file cpio gzip sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || \
      issues+=("missing required tool: $tool")
  done

  if [ -z "$BUSYBOX" ] || [ ! -x "$BUSYBOX" ]; then
    issues+=("missing statically linked BusyBox (or set BUSYBOX=/path/to/busybox)")
  elif command -v file >/dev/null 2>&1 \
       && ! file "$BUSYBOX" | grep -q 'statically linked'; then
    issues+=("BUSYBOX is not statically linked: $BUSYBOX")
  fi

  if [ -n "${KERNEL_IMAGE:-}" ]; then
    [ -r "$KERNEL_IMAGE" ] || issues+=("unreadable KERNEL_IMAGE: $KERNEL_IMAGE")
  elif [ -n "$KERNEL_MANIFOLD_PATH" ]; then
    command -v manifold >/dev/null 2>&1 || \
      issues+=("missing manifold for QEMU_KERNEL_MANIFOLD_PATH")
  elif [ -n "$KERNEL_URL" ]; then
    command -v curl >/dev/null 2>&1 || \
      issues+=("missing curl for QEMU_KERNEL_URL")
  else
    issues+=("no kernel source; set KERNEL_IMAGE, QEMU_KERNEL_URL, or QEMU_KERNEL_MANIFOLD_PATH")
  fi

  [[ $KERNEL_SHA256 =~ ^[0-9a-f]{64}$ ]] || \
    issues+=("QEMU_KERNEL_SHA256 must be a lowercase 64-character SHA-256")

  if [ "${#issues[@]}" -ne 0 ]; then
    printf 'QEMU demo dependency check failed (%d issues):\n' \
      "${#issues[@]}" >&2
    for issue in "${issues[@]}"; do
      printf '  - %s\n' "$issue" >&2
    done
    printf '\nDebian/Ubuntu: sudo apt install python3 qemu-system-x86 qemu-utils busybox-static cpio gzip curl file\n' >&2
    printf 'Fedora: sudo dnf install python3 qemu-system-x86-core qemu-img busybox cpio gzip curl file\n' >&2
    printf 'CentOS/RHEL: install qemu-kvm-core, qemu-img, and EPEL busybox; set QEMU_BIN and BUSYBOX if their paths differ.\n' >&2
    return 1
  fi

  if [ "$CHECK_ONLY" -eq 1 ]; then
    echo 'QEMU dependency check passed: qemu-system-x86_64 qemu-img python3 static-busybox file cpio gzip sha256sum kernel-source'
  fi
}

preflight || exit 1
[ "$CHECK_ONLY" -eq 0 ] || exit 0

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
  elif [ -n "$KERNEL_MANIFOLD_PATH" ]; then
    echo 'Downloading kernel from configured artifact storage...'
    manifold --quiet get --threads 20 \
      "$KERNEL_MANIFOLD_PATH" "$kernel_tmp" >/dev/null 2>&1 || \
      fail "kernel download failed from configured artifact storage"
    kernel_source="configured artifact storage"
  elif [ -n "$KERNEL_URL" ]; then
    echo 'Downloading kernel...'
    fetch_url "$KERNEL_URL" "$kernel_tmp" || \
      fail "kernel download failed: $KERNEL_URL"
    kernel_source="$KERNEL_URL"
  else
    fail "set KERNEL_IMAGE, QEMU_KERNEL_URL, or QEMU_KERNEL_MANIFOLD_PATH"
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
