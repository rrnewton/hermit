#!/usr/bin/env bash
# Wipe stale computed results from the QEMU/Linux demo suite (demos 5 and 6).
#
# The Python demos (05-qemu-boot.py, 06-qemu-resume.py) persist a first-run
# "anchor" (demo 5: the boot-anchor/ directory; demo 6: resume-metadata/) and
# compare every later run against it. When the Hermit binary, kernel, or demo
# changes, that stale anchor makes fresh runs report a false PARTIAL. This
# script gives a one-command fresh start.
#
#   ./clean.sh              wipe COMPUTED RESULTS only (anchors, run history,
#                           snapshots, runtime cruft). Keeps the kernel/rootfs
#                           blobs so the next run does not re-download/rebuild.
#   ./clean.sh --distclean  additionally wipe the provisioned kernel download
#                           and BusyBox initramfs (bzImage, initramfs.cpio.gz).
#   ./clean.sh --dry-run    print what would be removed without deleting.
#
# The demo asset directory defaults to <repo>/ignored/qemu-linux and honors the
# same QEMU_ASSETS override the demos use. Only the specific paths the demo
# suite creates are removed; unrelated content in the asset directory (other
# initramfs variants, scx logs, the shell boot apparatus) is left untouched.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
ASSETS="${QEMU_ASSETS:-$ROOT/ignored/qemu-linux}"

distclean=0
dry_run=0
for arg in "$@"; do
  case "$arg" in
    --distclean) distclean=1 ;;
    --dry-run|-n) dry_run=1 ;;
    -h|--help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      printf 'clean.sh: unknown argument: %s\n' "$arg" >&2
      printf "Try './clean.sh --help'.\n" >&2
      exit 2
      ;;
  esac
done

# Guard against a mis-resolved asset directory before deleting anything.
if [ -z "$ASSETS" ] || [ "$ASSETS" = "/" ]; then
  printf 'clean.sh: refusing to operate on unsafe QEMU_ASSETS=%q\n' "$ASSETS" >&2
  exit 1
fi

# Computed results: recreated by the next demo run. Safe to wipe for a fresh
# start; this is what clears the stale-anchor false-PARTIAL effect.
computed=(
  "boot-anchor"                # 05-qemu-boot.py atomic first-run anchor (dir)
  "boot-anchor.claim.lock"     # fallback anchor-claim lock (non-renameat2 FS)
  ".work"                      # private per-run working dirs (concurrent runs)
  "run-metadata.json"          # legacy top-level boot anchor (pre-concurrent)
  "run-history"                # 05-qemu-boot.py per-run archives
  "resume-metadata"            # 06-qemu-resume.py per-command anchors + history
  "hermit-snapshot.qcow2"      # live snapshot store written by the boot demo
  "hermit-snapshot.qcow2.id"
  "hermit-boot.qcow2"          # archived boot snapshot
  "serial.log"                 # runtime serial capture
  "serial-pipe.in"             # runtime QEMU serial input FIFO (demo 6 resume)
  "serial-pipe.out"            # runtime QEMU serial output FIFO (demo 6 resume)
  "qmp.sock"                   # runtime QEMU QMP socket
  ".qemu-demo.lock"            # single-writer demo lock (demo 6)
)

# Provisioned inputs: expensive to recreate (kernel download + initramfs build).
# Only removed by --distclean.
provisioned=(
  "bzImage"                    # kernel from KERNEL_IMAGE, URL, or configured storage
  "initramfs.cpio.gz"          # BusyBox rootfs built by lib/qemu-assets.sh
  ".initramfs-version"         # initramfs cache-version marker
)

# Transient temp files the demos/assets script may leave behind on interruption.
transient_globs=(
  "run-metadata.json.tmp."*
  "hermit-boot.qcow2.tmp."*
  ".bzImage."*
  ".initramfs.cpio.gz."*
  ".initramfs-version."*
)

remove_path() {
  local path="$1"
  [ -e "$path" ] || [ -L "$path" ] || return 0
  local rel="${path#"$ROOT"/}"
  if [ "$dry_run" -eq 1 ]; then
    printf '  would remove %s\n' "$rel"
  else
    rm -rf -- "$path"
    printf '  removed %s\n' "$rel"
  fi
}

if [ ! -d "$ASSETS" ]; then
  printf 'Nothing to clean: %s does not exist.\n' "${ASSETS#"$ROOT"/}"
  exit 0
fi

if [ "$dry_run" -eq 1 ]; then
  printf 'Dry run (no files will be deleted).\n'
fi
printf 'Cleaning computed demo results under %s\n' "${ASSETS#"$ROOT"/}"

for name in "${computed[@]}"; do
  remove_path "$ASSETS/$name"
done

shopt -s nullglob
for glob in "${transient_globs[@]}"; do
  for path in "$ASSETS"/$glob; do
    remove_path "$path"
  done
done
shopt -u nullglob

if [ "$distclean" -eq 1 ]; then
  printf 'Removing provisioned kernel download and initramfs (--distclean)\n'
  for name in "${provisioned[@]}"; do
    remove_path "$ASSETS/$name"
  done
fi

if [ "$dry_run" -eq 1 ]; then
  printf 'Dry run complete.\n'
elif [ "$distclean" -eq 1 ]; then
  printf 'distclean complete: computed results and provisioned assets removed.\n'
else
  printf 'clean complete: computed results removed (kernel/rootfs kept).\n'
fi
