#!/usr/bin/env bash
# Demo 07: reproducible task evolution from the phase-5 QEMU snapshot.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
ASSETS="${QEMU_ASSETS:-$ROOT/ignored/qemu-linux}"
ARTIFACTS="${DEMO07_ARTIFACTS:-$ROOT/ignored/demo07-drgn}"

usage() {
  cat <<'EOF'
Usage: demos/07-drgn-kernel.sh

Restore the phase-5 QEMU/Linux snapshot twice. Each restore gets a read-only
drgn task-list snapshot, a fixed 1000-us guest-virtual-time advance which adds
two tasks, and a second task-list snapshot. The two evolutions must match.
Each drgn read interval executes no guest instructions.

Useful overrides:
  DEMO07_RUNS=2              independent restores (minimum and default: 2)
  DEMO07_TASK_LIMIT=16       displayed task-list prefix (all rows are compared)
  HERMIT_RELEASE=/path       release Hermit binary
  QEMU_BIN=/path             qemu-system-x86_64 binary
  QEMU_ASSETS=/path          bzImage/initramfs cache
  DEMO07_SNAPSHOT_DISK=/path phase-5 boot snapshot copy
  DEMO07_SNAPSHOT_NAME=name  internal snapshot name (default: hermit-boot)
  DEMO07_VMLINUX=/path       matching ELF debug/BTF image (auto-extracted by default)
  DEMO07_TIMEOUT=240         restore/advance timeout in seconds
EOF
}

case "${1:-}" in
  "") ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

DRGN_BIN="${DRGN_BIN:-$(command -v drgn || true)}"
if [ -z "$DRGN_BIN" ]; then
  echo "error: drgn is required (https://github.com/osandov/drgn)" >&2
  exit 1
fi

HERMIT_RELEASE="${HERMIT_RELEASE:-$ROOT/hermit/target/release/hermit}"
if [ ! -x "$HERMIT_RELEASE" ]; then
  make -C "$ROOT" --no-print-directory -s build-hermit
fi
if [ ! -x "$HERMIT_RELEASE" ]; then
  echo "error: missing Hermit release binary: $HERMIT_RELEASE" >&2
  exit 1
fi

QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
if [ -z "$QEMU_BIN" ] && [ -x /usr/libexec/qemu-kvm ]; then
  QEMU_BIN=/usr/libexec/qemu-kvm
fi
RESEARCH_QEMU_ROOT="$ROOT/ignored/demo07-drgn_20260728/qemu-root"
if [ -z "$QEMU_BIN" ] && [ -x "$RESEARCH_QEMU_ROOT/usr/bin/qemu-system-x86_64" ]; then
  QEMU_BIN="$RESEARCH_QEMU_ROOT/usr/bin/qemu-system-x86_64"
  DEMO07_QEMU_BIOS="${DEMO07_QEMU_BIOS:-$RESEARCH_QEMU_ROOT/usr/share/qemu}"
  DEMO07_QEMU_LIBRARY_PATH="${DEMO07_QEMU_LIBRARY_PATH:-$RESEARCH_QEMU_ROOT/usr/lib64}"
fi
if [ -z "$QEMU_BIN" ] || [ ! -x "$QEMU_BIN" ]; then
  echo "error: qemu-system-x86_64 is required (or set QEMU_BIN)" >&2
  exit 1
fi

DEMO07_KERNEL="${DEMO07_KERNEL:-$ASSETS/bzImage}"
DEMO07_INITRD="${DEMO07_INITRD:-$ASSETS/initramfs.cpio.gz}"
DEMO07_SNAPSHOT_DISK="${DEMO07_SNAPSHOT_DISK:-$ASSETS/hermit-boot.qcow2}"
if [ ! -r "$DEMO07_KERNEL" ] || [ ! -r "$DEMO07_INITRD" ]; then
  QEMU_BIN="$QEMU_BIN" "$DEMO_DIR/lib/qemu-assets.sh"
fi
if [ ! -r "$DEMO07_KERNEL" ] || [ ! -r "$DEMO07_INITRD" ]; then
  echo "error: QEMU kernel/initramfs provisioning failed under $ASSETS" >&2
  exit 1
fi
if [ ! -r "$DEMO07_SNAPSHOT_DISK" ]; then
  default_snapshot="$ASSETS/hermit-boot.qcow2"
  if [ "$DEMO07_SNAPSHOT_DISK" != "$default_snapshot" ]; then
    echo "error: missing custom phase-5 snapshot: $DEMO07_SNAPSHOT_DISK" >&2
    echo "produce the custom snapshot before Demo 07" >&2
    exit 1
  fi
  echo "Phase-5 snapshot missing; running demo 5 prerequisite..."
  QEMU_ASSETS="$ASSETS" QEMU_BIN="$QEMU_BIN" HERMIT_RELEASE="$HERMIT_RELEASE" \
    make -C "$DEMO_DIR" --no-print-directory demo5
fi
if [ ! -r "$DEMO07_SNAPSHOT_DISK" ]; then
  echo "error: Demo 5 did not produce $DEMO07_SNAPSHOT_DISK" >&2
  exit 1
fi

DEMO07_VMLINUX="${DEMO07_VMLINUX:-$ASSETS/vmlinux}"
mkdir -p "$ARTIFACTS" "$ARTIFACTS/drgn-par"

export HERMIT_RELEASE QEMU_BIN DEMO07_KERNEL DEMO07_INITRD DEMO07_VMLINUX
export DEMO07_SNAPSHOT_DISK
export DEMO07_SNAPSHOT_NAME="${DEMO07_SNAPSHOT_NAME:-hermit-boot}"
export DEMO07_ARTIFACTS="$ARTIFACTS"
export DEMO07_QEMU_BIOS="${DEMO07_QEMU_BIOS:-}"
export DEMO07_QEMU_LIBRARY_PATH="${DEMO07_QEMU_LIBRARY_PATH:-}"
export FB_PAR_UNPACK_BASEDIR="${FB_PAR_UNPACK_BASEDIR:-$ARTIFACTS/drgn-par}"

echo "=== Demo 07: snapshot -> zero-cost read -> deterministic advance -> read ==="
"$DRGN_BIN" -q -p "$$" "$DEMO_DIR/07-drgn-kernel.py"
