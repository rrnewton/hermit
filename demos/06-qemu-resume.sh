#!/usr/bin/env bash
#
# Demo 6: resume the saved QEMU snapshot and inject one serial command.

set -euo pipefail

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 6: QEMU Snapshot Resume"
echo ''
echo '=========================================='
echo '=== Demo 6: QEMU Snapshot Resume ==='
echo '=========================================='
echo ''
echo 'QEMU resumes the live Linux shell saved by Demo 5. The requested command is'
echo 'injected over the guest serial socket, then the normalized Hermit INFO tail is'
echo 'saved and compared with the previous run of that same command.'
echo ''
echo '=========================================='

# shellcheck source=demos/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
# shellcheck source=demos/lib/qemu-snapshot.sh
source "$DEMO_DIR/lib/qemu-snapshot.sh"

export QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
export QEMU_TIMEOUT="${QEMU_TIMEOUT:-600}"
export HERMIT_RELEASE="${HERMIT_RELEASE:-$HERMIT_REPO/target/release/hermit}"
export QEMU_ASSETS="${QEMU_ASSETS:-$ROOT/ignored/qemu-linux}"
export QEMU_LOG_FILTER="${QEMU_LOG_FILTER:-detcore::scheduler::runqueue=info,detcore::tool_global=info}"
export QEMU_SNAPSHOT_NAME="${QEMU_SNAPSHOT_NAME:-hermit-boot}"
export QEMU_SNAPSHOT_DISK="${QEMU_SNAPSHOT_DISK:-$QEMU_ASSETS/hermit-snapshot.qcow2}"
export QEMU_SNAPSHOT_ID_FILE="${QEMU_SNAPSHOT_ID_FILE:-$QEMU_SNAPSHOT_DISK.id}"

guest_command=${*:-uname -a}
case "$guest_command" in
  *$'\n'*|*$'\r'*)
    echo "guest command must be a single line" >&2
    exit 2
    ;;
esac

test -x "$HERMIT_RELEASE" || {
  echo "missing release Hermit binary: $HERMIT_RELEASE" >&2
  exit 1
}
if [ -z "$QEMU_BIN" ] || [ ! -x "$QEMU_BIN" ]; then
  echo "qemu-system-x86_64 is required" >&2
  exit 1
fi
test -r "$QEMU_ASSETS/bzImage" || {
  echo "missing QEMU kernel; run $DEMO_DIR/05-qemu-boot.sh first" >&2
  exit 1
}
test -r "$QEMU_ASSETS/initramfs.cpio.gz" || {
  echo "missing QEMU initramfs; run $DEMO_DIR/05-qemu-boot.sh first" >&2
  exit 1
}
test -r "$QEMU_SNAPSHOT_DISK" || {
  echo "missing snapshot disk; run $DEMO_DIR/05-qemu-boot.sh first" >&2
  exit 1
}
test -r "$QEMU_SNAPSHOT_ID_FILE" || {
  echo "missing snapshot identity; run $DEMO_DIR/05-qemu-boot.sh first" >&2
  exit 1
}
qemu_snapshot_require_tools
qemu_snapshot_exists "$QEMU_SNAPSHOT_DISK" "$QEMU_SNAPSHOT_NAME" || {
  echo "missing snapshot $QEMU_SNAPSHOT_NAME; run Demo 5 first" >&2
  exit 1
}

snapshot_identity="$(cat "$QEMU_SNAPSHOT_ID_FILE")"
command_key="$(printf '%s\0%s' "$snapshot_identity" "$guest_command" \
  | sha256sum | cut -d' ' -f1)"
comparison_dir="$QEMU_ASSETS/resume-info"
previous_info="$comparison_dir/$command_key.info.log"
mkdir -p "$comparison_dir"

export QEMU_LOG="${QEMU_LOG:-$DEMO_ARTIFACTS/qemu-resume.log}"
serial_log="$DEMO_ARTIFACTS/qemu-resume.serial.log"
info_tail="$DEMO_ARTIFACTS/qemu-resume.info.log"
serial_socket="$DEMO_ARTIFACTS/qemu-resume-serial.sock"
input_fifo="$DEMO_ARTIFACTS/qemu-resume-input.$$"
hermit_pid=""
serial_pid=""

cleanup_qemu() {
  exec 3>&- 2>/dev/null || true
  qemu_stop_pid "$serial_pid"
  qemu_stop_pid "$hermit_pid"
  rm -f "$input_fifo" "$serial_socket"
}
trap cleanup_qemu EXIT

rm -f "$input_fifo" "$serial_socket"
mkfifo "$input_fifo"
exec 3<>"$input_fifo"
: >"$QEMU_LOG"
: >"$serial_log"

demo_banner "Resume $QEMU_SNAPSHOT_NAME and run: $guest_command"
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
  -drive "if=none,id=hermit-snapshot-store,file=$QEMU_SNAPSHOT_DISK,format=qcow2" \
  -loadvm "$QEMU_SNAPSHOT_NAME" \
  -icount shift=0,sleep=off \
  -rtc base=2022-01-01T00:00:00,clock=vm \
  -kernel "$QEMU_ASSETS/bzImage" \
  -initrd "$QEMU_ASSETS/initramfs.cpio.gz" \
  -append 'console=ttyS0 reboot=t' \
  >"$QEMU_LOG" 2>&1 &
hermit_pid=$!

qemu_wait_for_socket "$serial_socket" "$hermit_pid" 60
nc -U "$serial_socket" <"$input_fifo" >"$serial_log" 2>&1 &
serial_pid=$!

# The snapshot stops with the shell blocked in read(2). An empty command makes
# it print a fresh prompt and proves the restored serial path is ready.
sleep "${QEMU_RESUME_CONNECT_DELAY:-0.5}"
printf '\n' >&3
qemu_wait_for_log_line "$serial_log" '~ #' "$hermit_pid" 60

begin_marker='__HERMIT_COMMAND_BEGIN__'
end_marker='__HERMIT_COMMAND_END__'
printf 'echo %s\n' "$begin_marker" >&3
sleep 0.2
printf '%s\n' "$guest_command" >&3
sleep 0.2
printf 'echo %s\n' "$end_marker" >&3
sleep 0.2
printf 'poweroff -f\n' >&3

set +e
wait "$hermit_pid"
resume_rc=$?
hermit_pid=""
qemu_stop_pid "$serial_pid"
serial_pid=""
set -e

if [ "$resume_rc" -ne 0 ]; then
  echo "QEMU resume exited with status $resume_rc; log: $QEMU_LOG" >&2
  exit "$resume_rc"
fi
grep -Fq "$begin_marker" "$serial_log" || {
  echo "guest command did not start; transcript: $serial_log" >&2
  exit 1
}
grep -Fq "$end_marker" "$serial_log" || {
  echo "guest command did not finish; transcript: $serial_log" >&2
  exit 1
}
grep -Fq 'reboot: Power down' "$serial_log" || {
  echo "resumed guest did not power off cleanly; transcript: $serial_log" >&2
  exit 1
}

demo_banner "Guest serial output"
sed -n "/$begin_marker/,/$end_marker/p" "$serial_log"

qemu_write_stable_info_tail "$QEMU_LOG" "$info_tail"
demo_banner "Hermit INFO log tail"
cat "$info_tail"

if [ -r "$previous_info" ]; then
  if cmp -s "$previous_info" "$info_tail"; then
    printf '\nINFO tail matches the previous run of %q.\n' "$guest_command"
  else
    echo "INFO tail differs from the previous run of: $guest_command" >&2
    diff -u "$previous_info" "$info_tail" || true
    exit 1
  fi
else
  cp "$info_tail" "$previous_info"
  printf '\nSaved the first INFO tail for %q. Run this command again to compare.\n' \
    "$guest_command"
fi
printf 'Evidence: %s\n' "$DEMO_ARTIFACTS"

demo_success
