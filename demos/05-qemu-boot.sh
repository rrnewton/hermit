#!/usr/bin/env bash
#
# Demo 5: boot Linux in QEMU under Hermit's relaxed VMM profile.
#
# QEMU needs concurrent host threads to make progress, so this is virtual-time
# compatibility evidence rather than a strict/verify determinism claim.

set -euo pipefail

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 5: QEMU Linux Boot"
cat <<'DESC'
=== Demo 5: QEMU Linux Boot ===

Hermit runs QEMU's TCG emulator, which boots a real Linux kernel and prints its
guest-visible RTC. QEMU's host threads run concurrently in this profile, so the
result demonstrates Linux-boot compatibility and Hermit's fixed virtual-time
epoch; it is not a strict/verify (L2) determinism claim.
DESC

# shellcheck source=demos/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

export QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
export QEMU_TIMEOUT="${QEMU_TIMEOUT:-180}"
export HERMIT_RELEASE="${HERMIT_RELEASE:-$HERMIT_REPO/target/release/hermit}"

# The QEMU images are intentionally git-ignored. Prefer a path supplied by the
# caller, then check the product checkout, this parent checkout, and the shared
# dev-hermit root used by numbered worktrees.
if [ -z "${QEMU_ASSETS:-}" ]; then
  for candidate in \
    "$HERMIT_REPO/ignored/qemu-linux" \
    "$ROOT/ignored/qemu-linux" \
    "$ROOT/../../ignored/qemu-linux"; do
    if [ -r "$candidate/bzImage" ] && [ -r "$candidate/initramfs.cpio.gz" ]; then
      QEMU_ASSETS="$candidate"
      break
    fi
  done
fi
QEMU_ASSETS="${QEMU_ASSETS:-$HERMIT_REPO/ignored/qemu-linux}"

test -x "$HERMIT_RELEASE" || {
  echo "missing release Hermit binary: $HERMIT_RELEASE" >&2
  echo "Run: (cd $HERMIT_REPO && cargo build --release -p hermit)" >&2
  exit 1
}
if [ -z "$QEMU_BIN" ] || [ ! -x "$QEMU_BIN" ]; then
  echo "qemu-system-x86_64 is required" >&2
  exit 1
fi
test -r "$QEMU_ASSETS/bzImage" || {
  echo "missing QEMU kernel: $QEMU_ASSETS/bzImage" >&2
  exit 1
}
test -r "$QEMU_ASSETS/initramfs.cpio.gz" || {
  echo "missing QEMU initramfs: $QEMU_ASSETS/initramfs.cpio.gz" >&2
  exit 1
}

export QEMU_LOG="${QEMU_LOG:-$DEMO_ARTIFACTS/qemu-linux-boot.log}"
input_fifo="$DEMO_ARTIFACTS/qemu-linux-input.$$"
rm -f "$input_fifo"
mkfifo "$input_fifo"
exec 3<>"$input_fifo"
cleanup_qemu_input() {
  exec 3>&-
  rm -f "$input_fifo"
}
trap cleanup_qemu_input EXIT

demo_banner "Boot Linux and power off from its serial shell"
: >"$QEMU_LOG"

# -nographic assigns the QEMU monitor and serial port to stdio by default.
# Disabling the monitor is required before the requested explicit
# `-serial stdio`; without it QEMU 10.1 exits because two devices claim stdio.
set +e
timeout --kill-after=10 --signal=TERM "$QEMU_TIMEOUT" \
  "$HERMIT_RELEASE" run \
  --no-sequentialize-threads \
  --preemption-timeout 10000000000 -- \
  "$QEMU_BIN" \
  -machine q35 \
  -cpu max \
  -smp 1 \
  -m 512M \
  -nographic \
  -monitor none \
  -serial stdio \
  -icount shift=0,sleep=off \
  -kernel "$QEMU_ASSETS/bzImage" \
  -initrd "$QEMU_ASSETS/initramfs.cpio.gz" \
  -append 'console=ttyS0 reboot=t' \
  <"$input_fifo" 2>&1 | tee "$QEMU_LOG" &
boot_pid=$!
set -e

# This staged initramfs opens an interactive shell after its boot marker. Wait
# until the marker is visible so firmware cannot consume the poweroff command.
marker='HERMIT-QEMU-BASELINE-BOOT-OK'
for ((attempt = 0; attempt < QEMU_TIMEOUT * 5; attempt++)); do
  if grep -q "$marker" "$QEMU_LOG"; then
    sleep 1
    printf 'poweroff -f\n' >&3
    break
  fi
  if ! kill -0 "$boot_pid" 2>/dev/null; then
    break
  fi
  sleep 0.2
done

set +e
wait "$boot_pid"
boot_rc=$?
set -e

if [ "$boot_rc" -ne 0 ]; then
  echo "QEMU boot exited with status $boot_rc; transcript: $QEMU_LOG" >&2
  exit "$boot_rc"
fi
grep -q "$marker" "$QEMU_LOG" || {
  echo "QEMU exited without the expected boot marker: $marker" >&2
  exit 1
}
grep -q 'reboot: Power down' "$QEMU_LOG" || {
  echo "QEMU exited without a clean Linux power-down marker" >&2
  exit 1
}

rtc_line="$(grep 'rtc_cmos.*setting system clock to' "$QEMU_LOG" | tail -1)"
case "$rtc_line" in
  *'2022-01-01T'*' UTC ('*) ;;
  *)
    echo "missing Hermit virtual-epoch RTC timestamp in $QEMU_LOG" >&2
    exit 1
    ;;
esac

demo_banner "Guest-visible virtual timestamp"
printf '%s\n' "$rtc_line"
echo "Compatibility only: concurrent QEMU host-thread interleavings are not L2-controlled."

demo_success
