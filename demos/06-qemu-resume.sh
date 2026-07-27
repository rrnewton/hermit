#!/usr/bin/env bash
#
# Demo 6: resume the saved QEMU snapshot and inject one serial command.

set -euo pipefail

# shellcheck source=demos/lib/display.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/display.sh"

usage() {
  cat <<EOF
Usage: ${0##*/} [COMMAND...]

Resume the QEMU snapshot created by Demo 5, run one command in the guest
serial shell, and compare its normalized Hermit INFO tail with the previous
run of the same command. The default command is: uname -a

Examples:
  ./demos/06-qemu-resume.sh 'ls /'
  ./demos/06-qemu-resume.sh 'cat /proc/cpuinfo'
  ./demos/06-qemu-resume.sh 'echo hello'
EOF
}

case ${1:-} in
  -h|--help)
    usage
    exit 0
    ;;
esac

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 6: QEMU Snapshot Resume"
demo_header "$DEMO_LABEL"
echo 'QEMU resumes the live Linux shell saved by Demo 5. The requested command is'
echo 'injected over the guest serial socket, then the normalized Hermit INFO tail is'
echo 'saved and compared with the previous run of that same command.'
echo ''
echo '=========================================='

# shellcheck source=demos/common.sh
export DEMO_BUILD_MODE=release
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
# shellcheck source=demos/lib/qemu-snapshot.sh
source "$DEMO_DIR/lib/qemu-snapshot.sh"

export QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
export QEMU_TIMEOUT="${QEMU_TIMEOUT:-120}"
export HERMIT_RELEASE="${HERMIT_RELEASE:-$HERMIT_REPO/target/release/hermit}"
export QEMU_ASSETS="${QEMU_ASSETS:-$ROOT/ignored/qemu-linux}"
export QEMU_LOG_FILTER="${QEMU_LOG_FILTER:-warn,detcore::scheduler=info,detcore::tool_global=info,reverie_ptrace::task=info}"
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
  echo "missing QEMU kernel; run ./demos/05-qemu-boot.sh first" >&2
  exit 1
}
test -r "$QEMU_ASSETS/initramfs.cpio.gz" || {
  echo "missing QEMU initramfs; run ./demos/05-qemu-boot.sh first" >&2
  exit 1
}
test -r "$QEMU_SNAPSHOT_DISK" || {
  echo "missing snapshot disk; run ./demos/05-qemu-boot.sh first" >&2
  exit 1
}
test -r "$QEMU_SNAPSHOT_ID_FILE" || {
  echo "missing snapshot identity; run ./demos/05-qemu-boot.sh first" >&2
  exit 1
}
qemu_snapshot_require_tools
qemu_snapshot_exists "$QEMU_SNAPSHOT_DISK" "$QEMU_SNAPSHOT_NAME" || {
  echo "missing snapshot $QEMU_SNAPSHOT_NAME; run Demo 5 first" >&2
  exit 1
}

snapshot_identity="$(cat "$QEMU_SNAPSHOT_ID_FILE")"
info_comparison_version=3
command_key="$(printf '%s\0%s\0%s' \
  "$snapshot_identity" "$guest_command" "$info_comparison_version" \
  | sha256sum | cut -d' ' -f1)"
comparison_dir="$QEMU_ASSETS/resume-info"
previous_info="$comparison_dir/$command_key.info.log"
mkdir -p "$comparison_dir"

export QEMU_LOG="${QEMU_LOG:-$DEMO_ARTIFACTS/qemu-resume.log}"
serial_log="$DEMO_ARTIFACTS/qemu-resume.serial.log"
info_tail="$DEMO_ARTIFACTS/qemu-resume.info.log"
comparison_info="$DEMO_ARTIFACTS/qemu-resume.compare.info.log"
serial_socket="$DEMO_ARTIFACTS/qemu-resume-serial.sock"
input_fifo="$DEMO_ARTIFACTS/qemu-resume-input.$$"
hermit_pid=""
serial_pid=""
progress_pid=""

start_resume_progress() {
  local owner_pid=$1
  local label=${2:-Restoring snapshot}

  if [ -t 2 ]; then
    (
      local frames='|/-'
      local elapsed=0
      while kill -0 "$owner_pid" 2>/dev/null; do
        printf '\r%s... %s %ds' "$label" \
          "${frames:elapsed%${#frames}:1}" "$elapsed"
        sleep 1
        elapsed=$((elapsed + 1))
      done
    ) >&2 &
    progress_pid=$!
  else
    printf '%s (timeout: %ss)...\n' "$label" "$QEMU_TIMEOUT"
  fi
}

stop_resume_progress() {
  [ -n "$progress_pid" ] || return 0
  kill "$progress_pid" 2>/dev/null || true
  wait "$progress_pid" 2>/dev/null || true
  progress_pid=""
  printf '\r%*s\r' 48 '' >&2
}

cleanup_qemu() {
  stop_resume_progress
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
resume_started=$SECONDS
start_resume_progress "$hermit_pid" 'Restoring snapshot'

qemu_wait_for_socket "$serial_socket" "$hermit_pid" "$QEMU_TIMEOUT"
echo 'Serial socket connected; waiting for the restored shell prompt...'
nc -U "$serial_socket" <"$input_fifo" >"$serial_log" 2>&1 &
serial_pid=$!

# The snapshot stops with the shell blocked in read(2). An empty command makes
# it print a fresh prompt and proves the restored serial path is ready.
sleep "${QEMU_RESUME_CONNECT_DELAY:-0.5}"
printf '\n' >&3
qemu_wait_for_log_line "$serial_log" '~ #' "$hermit_pid" "$QEMU_TIMEOUT"
stop_resume_progress
printf 'Restored shell ready after %ds; sending command.\n' \
  "$((SECONDS - resume_started))"

begin_marker='__HERMIT_COMMAND_BEGIN__'
end_marker='__HERMIT_COMMAND_END__'
printf 'echo %s\n' "$begin_marker" >&3
sleep 0.2
printf '%s\n' "$guest_command" >&3
sleep 0.2
printf 'echo %s\n' "$end_marker" >&3
sleep 0.2
printf 'poweroff -f\n' >&3
echo 'Command sent; waiting for clean guest shutdown...'
start_resume_progress "$hermit_pid" 'Finishing Hermit run'

set +e
wait "$hermit_pid"
resume_rc=$?
stop_resume_progress
hermit_pid=""
qemu_stop_pid "$serial_pid"
serial_pid=""
set -e

if [ "$resume_rc" -eq 124 ] || [ "$resume_rc" -eq 137 ]; then
  echo "QEMU resume timed out after ${QEMU_TIMEOUT}s." >&2
  echo "Hermit log: ${QEMU_LOG#"$ROOT/"}" >&2
  exit "$resume_rc"
elif [ "$resume_rc" -ne 0 ]; then
  echo "QEMU resume exited with status $resume_rc; log: ${QEMU_LOG#"$ROOT/"}" >&2
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
awk -v begin="$begin_marker" -v end="$end_marker" '
  index($0, begin) { printing = 1; next }
  index($0, end) { exit }
  printing { print }
' "$serial_log"

qemu_write_stable_info_tail "$QEMU_LOG" "$info_tail"
demo_banner "Hermit INFO tail (wall-clock timestamps stripped)"
cat "$info_tail"
qemu_normalize_info_for_comparison "$info_tail" "$comparison_info"

if [ -r "$previous_info" ]; then
  if cmp -s "$previous_info" "$comparison_info"; then
    printf '\nNormalized INFO structure matches the previous run of %q.\n' \
      "$guest_command"
  else
    echo "Normalized INFO structure differs from the previous run of: $guest_command" >&2
    diff -u "$previous_info" "$comparison_info" || true
    exit 1
  fi
else
  cp "$comparison_info" "$previous_info"
  printf '\nSaved the first normalized INFO structure for %q. Run this command again to compare.\n' \
    "$guest_command"
fi
printf 'Evidence: %s\n' "${DEMO_ARTIFACTS#"$ROOT/"}"

demo_success
