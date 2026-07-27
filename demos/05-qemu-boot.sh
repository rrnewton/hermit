#!/usr/bin/env bash
#
# Demo 5: boot Linux under Hermit and save a reusable QEMU snapshot.

set -euo pipefail

# shellcheck source=demos/lib/display.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/display.sh"

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 5: QEMU Linux Snapshot"
demo_header "$DEMO_LABEL"
echo "Hermit runs QEMU's TCG emulator in strict mode, boots a real Linux kernel to"
echo 'its serial shell, and saves that live machine as the internal snapshot'
echo '"hermit-boot". Demo 6 can then resume the shell without booting Linux again.'
echo ''
echo '=========================================='

# shellcheck source=demos/common.sh
export DEMO_BUILD_MODE=release
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
# shellcheck source=demos/lib/qemu-snapshot.sh
source "$DEMO_DIR/lib/qemu-snapshot.sh"

export QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
export QEMU_TIMEOUT="${QEMU_TIMEOUT:-600}"
export HERMIT_RELEASE="${HERMIT_RELEASE:-$HERMIT_REPO/target/release/hermit}"
export QEMU_ASSETS="${QEMU_ASSETS:-$ROOT/ignored/qemu-linux}"
export QEMU_LOG_FILTER="${QEMU_LOG_FILTER:-warn,detcore::scheduler=info,detcore::tool_global=info,reverie_ptrace::task=info}"
export QEMU_SNAPSHOT_NAME="${QEMU_SNAPSHOT_NAME:-hermit-boot}"
export QEMU_SNAPSHOT_DISK="${QEMU_SNAPSHOT_DISK:-$QEMU_ASSETS/hermit-snapshot.qcow2}"
export QEMU_SNAPSHOT_ID_FILE="${QEMU_SNAPSHOT_ID_FILE:-$QEMU_SNAPSHOT_DISK.id}"
export QEMU_SNAPSHOT_SIZE="${QEMU_SNAPSHOT_SIZE:-64M}"

demo_banner "Verify QEMU kernel and initramfs"
"$DEMO_DIR/lib/qemu-assets.sh"

test -x "$HERMIT_RELEASE" || {
  echo "missing release Hermit binary: $HERMIT_RELEASE" >&2
  echo "Run: make" >&2
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
qemu_snapshot_require_tools

mkdir -p "$QEMU_ASSETS"
snapshot_tmp="$QEMU_SNAPSHOT_DISK.tmp.$$"
snapshot_id_tmp="$QEMU_SNAPSHOT_ID_FILE.tmp.$$"
rm -f "$QEMU_SNAPSHOT_ID_FILE"
qemu-img create -q -f qcow2 "$snapshot_tmp" "$QEMU_SNAPSHOT_SIZE"
mv -f "$snapshot_tmp" "$QEMU_SNAPSHOT_DISK"
snapshot_tmp=""

export QEMU_LOG="${QEMU_LOG:-$DEMO_ARTIFACTS/qemu-snapshot-boot.log}"
serial_log="$DEMO_ARTIFACTS/qemu-snapshot-boot.serial.log"
info_tail="$DEMO_ARTIFACTS/qemu-snapshot-boot.info.log"
qmp_socket="$DEMO_ARTIFACTS/qemu-snapshot-qmp.sock"
serial_socket="$DEMO_ARTIFACTS/qemu-snapshot-serial.sock"
input_fifo="$DEMO_ARTIFACTS/qemu-snapshot-input.$$"
hermit_pid=""
serial_pid=""

cleanup_qemu() {
  exec 3>&- 2>/dev/null || true
  qemu_stop_pid "$serial_pid"
  qemu_stop_pid "$hermit_pid"
  [ -z "$snapshot_tmp" ] || rm -f "$snapshot_tmp"
  [ -z "$snapshot_id_tmp" ] || rm -f "$snapshot_id_tmp"
  rm -f "$input_fifo" "$qmp_socket" "$serial_socket"
}
trap cleanup_qemu EXIT

rm -f "$input_fifo" "$qmp_socket" "$serial_socket"
mkfifo "$input_fifo"
exec 3<>"$input_fifo"
: >"$QEMU_LOG"
: >"$serial_log"

demo_banner "Boot Linux to its serial shell"
RUST_LOG="$QEMU_LOG_FILTER" \
timeout --kill-after=10 --signal=TERM "$QEMU_TIMEOUT" \
  "$HERMIT_RELEASE" run \
  --strict \
  --target-timeslice 100000 \
  --max-timeslice 2000000000 -- \
  "$QEMU_BIN" \
  -machine q35 \
  -cpu max \
  -smp 1 \
  -m 512M \
  -display none \
  -monitor none \
  -serial "unix:$serial_socket,server=on,wait=off" \
  -qmp "unix:$qmp_socket,server=on,wait=off" \
  -drive "if=none,id=hermit-snapshot-store,file=$QEMU_SNAPSHOT_DISK,format=qcow2" \
  -icount shift=0,sleep=off \
  -rtc base=2022-01-01T00:00:00,clock=vm \
  -kernel "$QEMU_ASSETS/bzImage" \
  -initrd "$QEMU_ASSETS/initramfs.cpio.gz" \
  -append 'console=ttyS0 reboot=t' \
  >"$QEMU_LOG" 2>&1 &
hermit_pid=$!

qemu_wait_for_socket "$qmp_socket" "$hermit_pid" 60
qemu_wait_for_socket "$serial_socket" "$hermit_pid" 60
nc -U "$serial_socket" <"$input_fifo" > >(tee "$serial_log") 2>&1 &
serial_pid=$!

marker='HERMIT-QEMU-BASELINE-BOOT-OK'
qemu_wait_for_log_line "$serial_log" "$marker" "$hermit_pid" "$QEMU_TIMEOUT"
qemu_wait_for_log_line "$serial_log" '~ #' "$hermit_pid" "$QEMU_TIMEOUT"
sleep "${QEMU_SNAPSHOT_SETTLE_SECONDS:-0.2}"

demo_banner "Save live snapshot $QEMU_SNAPSHOT_NAME"
qemu_qmp_command "$qmp_socket" human-monitor-command command-line \
  "savevm $QEMU_SNAPSHOT_NAME"
qemu_qmp_command "$qmp_socket" quit

set +e
wait "$hermit_pid"
boot_rc=$?
hermit_pid=""
qemu_stop_pid "$serial_pid"
serial_pid=""
set -e

if [ "$boot_rc" -ne 0 ]; then
  echo "QEMU snapshot boot exited with status $boot_rc; log: $QEMU_LOG" >&2
  exit "$boot_rc"
fi
grep -Fq "$marker" "$serial_log" || {
  echo "QEMU exited without the expected boot marker: $marker" >&2
  exit 1
}
qemu_snapshot_exists "$QEMU_SNAPSHOT_DISK" "$QEMU_SNAPSHOT_NAME" || {
  echo "snapshot $QEMU_SNAPSHOT_NAME is missing from $QEMU_SNAPSHOT_DISK" >&2
  exit 1
}
sha256sum "$QEMU_SNAPSHOT_DISK" | cut -d' ' -f1 >"$snapshot_id_tmp"
mv "$snapshot_id_tmp" "$QEMU_SNAPSHOT_ID_FILE"
snapshot_id_tmp=""

rtc_line="$(grep 'rtc_cmos.*setting system clock to' "$serial_log" | tail -1 || true)"
case "$rtc_line" in
  *'2022-01-01T'*' UTC ('*) ;;
  *)
    echo "missing Hermit virtual-epoch RTC timestamp in $serial_log" >&2
    exit 1
    ;;
esac

demo_banner "Snapshot ready"
printf 'Snapshot disk: %s (internal tag: %s)\n' \
  "${QEMU_SNAPSHOT_DISK#"$ROOT/"}" "$QEMU_SNAPSHOT_NAME"
qemu-img snapshot -l "$QEMU_SNAPSHOT_DISK"

qemu_write_stable_info_tail "$QEMU_LOG" "$info_tail"
demo_banner "Hermit INFO tail (wall-clock timestamps stripped)"
cat "$info_tail"

demo_banner "Paste a snapshot-resume command"
printf '  %q %q\n' './demos/06-qemu-resume.sh' 'ls /'
printf '  %q %q\n' './demos/06-qemu-resume.sh' 'cat /proc/cpuinfo'
printf '  %q %q\n' './demos/06-qemu-resume.sh' 'uname -a'
printf '  %q %q\n' './demos/06-qemu-resume.sh' 'echo hello'
echo
echo "Run the same line twice. Demo 6 compares its normalized INFO tail with the previous run."

demo_success
