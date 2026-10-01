#!/usr/bin/env bash
# Demo 9: boot a Linux kernel and a BusyBox userspace inside QEMU, with QEMU
# itself running as a guest under `hermit run --strict`.

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)
# shellcheck source=demos/lib/fetch-url.sh
source "$script_dir/../lib/fetch-url.sh"

kernel_image=${KERNEL_IMAGE:-}
# When KERNEL_IMAGE is unset, the demo downloads this exact kernel into the
# ignored cache under target/ and checks its SHA-256. Override QEMU_KERNEL_URL
# and QEMU_KERNEL_SHA256 together to pin a different kernel.
kernel_sha256=${QEMU_KERNEL_SHA256:-e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2}
kernel_url=${QEMU_KERNEL_URL:-https://github.com/rrnewton/hermit/releases/download/qemu-kernel-$kernel_sha256/bzImage}
qemu_bin=${QEMU_BIN:-}
output_dir=${OUTPUT_DIR:-$repo_root/target/qemu-busybox}
timeout_seconds=${DEMO_TIMEOUT_SECONDS:-300}
verify=${VERIFY:-0}
skid_margin=${SKID_MARGIN:-}

if [[ -z $qemu_bin ]]; then
  qemu_bin=$(command -v qemu-system-x86_64 || true)
fi

command -v hermit >/dev/null 2>&1 || fail \
  "hermit is not on PATH -- build it with 'make -C $repo_root release-core' and run: export PATH=\"$repo_root/target/release:\$PATH\""
[[ -n $qemu_bin && -x $qemu_bin ]] || fail \
  "qemu-system-x86_64 not found; install it or set QEMU_BIN"
[[ $timeout_seconds =~ ^[1-9][0-9]*$ ]] || fail \
  "DEMO_TIMEOUT_SECONDS must be a positive integer"
[[ $verify == 0 || $verify == 1 ]] || fail "VERIFY must be 0 or 1"
[[ -z $skid_margin || $skid_margin =~ ^[1-9][0-9]*$ ]] || fail \
  "SKID_MARGIN must be empty or a positive integer"

for command in grep sha256sum tee timeout; do
  command -v "$command" >/dev/null || fail "$command is required"
done

# Check for the verdict reader before the boot, which takes minutes, so a
# missing jq is reported as a setup problem rather than a failed run.
if [[ $verify == 1 ]]; then
  command -v jq >/dev/null 2>&1 \
    || fail "VERIFY=1 requires 'jq' to read the typed verdict; install jq or run without VERIFY=1"
fi

mkdir -p "$output_dir"

if [[ -z $kernel_image ]]; then
  [[ $kernel_sha256 =~ ^[0-9a-f]{64}$ ]] || fail \
    "QEMU_KERNEL_SHA256 must be a lowercase 64-character SHA-256"
  command -v curl >/dev/null || fail \
    "curl is required to download the kernel; install it or set KERNEL_IMAGE"
  kernel_image=$output_dir/bzImage
  cached_kernel_sha=""
  if [[ -r $kernel_image ]]; then
    cached_kernel_sha=$(sha256sum "$kernel_image" | cut -d' ' -f1)
  fi
  if [[ $cached_kernel_sha != "$kernel_sha256" ]]; then
    if [[ -n $cached_kernel_sha ]]; then
      printf 'kernel: replacing cache with unexpected sha256 %s\n' \
        "$cached_kernel_sha" >&2
    fi
    kernel_tmp=$output_dir/.bzImage.$$
    printf 'Downloading pinned QEMU kernel (%s)...\n' "$kernel_url" >&2
    fetch_url "$kernel_url" "$kernel_tmp" || \
      fail "kernel download failed: $kernel_url"
    downloaded_kernel_sha=$(sha256sum "$kernel_tmp" | cut -d' ' -f1)
    if [[ $downloaded_kernel_sha != "$kernel_sha256" ]]; then
      rm -f "$kernel_tmp"
      fail "kernel sha256 mismatch from $kernel_url: expected $kernel_sha256, got $downloaded_kernel_sha"
    fi
    mv "$kernel_tmp" "$kernel_image"
    printf 'kernel ready: %s\n' "$kernel_image" >&2
  else
    printf 'kernel ready: %s (cached)\n' "$kernel_image" >&2
  fi
fi
[[ -r $kernel_image ]] || fail "kernel image is not readable: $kernel_image"

initramfs_image=${INITRAMFS_IMAGE:-$output_dir/initramfs-busybox.cpio.gz}
console_log=$output_dir/console.log
info_log=$output_dir/hermit-info.log
stderr_log=$output_dir/hermit-stderr.log
verify_json=$output_dir/verify.json

if [[ -z ${INITRAMFS_IMAGE:-} ]]; then
  "$script_dir/build-initramfs.sh" "$initramfs_image"
else
  [[ -r $initramfs_image ]] || fail "initramfs is not readable: $initramfs_image"
fi

guest_command=(
  "$script_dir/boot_qemu.sh"
  "$kernel_image"
  "$initramfs_image"
  "$qemu_bin"
)

# Without --epoch (or HERMIT_EPOCH), Hermit starts the virtual clock at the
# host's current time. QEMU's real-time clock follows that clock, so the guest
# kernel's boot messages (the audit timestamp and "setting system clock to")
# change from one run to the next, and so does the console hash. A fixed epoch
# makes a repeat run print the same console hash.
#
# The guest's environment and standard input are inputs too, so they are fixed
# here rather than inherited from whoever runs this script. With the default
# --base-env=host, one extra variable in the caller's environment changes how
# many branches the dynamic loader executes. QEMU's -serial stdio reads
# standard input: a run whose standard input was a read-write pipe or socket
# printed a different console hash (kernel timestamps one microsecond apart)
# from runs whose standard input was /dev/null. The launcher needs no
# environment variables, because every path is passed as an argument.
hermit_args=(--log info --log-file "$info_log" run --strict
  --epoch=2026-01-01T00:00:00Z --base-env=minimal)
if [[ -n $skid_margin ]]; then
  hermit_args+=(--skid-margin="$skid_margin")
fi
if [[ $verify == 1 ]]; then
  # Plain --verify compares the two runs with a lossy comparator; only
  # --verify-strict compares the complete event logs.
  hermit_args+=(--verify --verify-strict --verify-json "$verify_json")
fi
hermit_args+=(--)

printf 'backend=ptrace verify=%s log=info relaxations=none\n' \
  "$([[ $verify == 1 ]] && printf strict || printf off)"
printf 'pmu_skid_margin=%s\n' "${skid_margin:-auto}"
printf 'hermit=%s (%s)\n' "$(command -v hermit)" \
  "$(hermit --version 2>/dev/null || printf 'version unavailable')"
printf 'qemu=%s\nkernel=%s\ninitramfs=%s\nconsole=%s\ninfo=%s\nstderr=%s\n' \
  "$qemu_bin" "$kernel_image" "$initramfs_image" \
  "$console_log" "$info_log" "$stderr_log"
if [[ $verify == 1 ]]; then
  printf 'verify_json=%s\n' "$verify_json"
fi
printf 'kernel_sha256=%s\ninitramfs_sha256=%s\n' \
  "$(sha256sum "$kernel_image" | cut -d' ' -f1)" \
  "$(sha256sum "$initramfs_image" | cut -d' ' -f1)"

: >"$console_log"
: >"$info_log"
: >"$stderr_log"
if [[ $verify == 1 ]]; then
  # A verdict left by an earlier run must not certify this one.
  : >"$verify_json"
fi
set +e
timeout --signal=TERM --kill-after=10 "${timeout_seconds}s" \
  hermit "${hermit_args[@]}" "${guest_command[@]}" \
  </dev/null \
  > >(tee "$console_log") \
  2> >(tee "$stderr_log" >&2)
status=$?
set -e

if ((status != 0)); then
  fail "Hermit/QEMU exited with status $status; inspect $console_log, $info_log, and $stderr_log"
fi

# Check the workload in both modes: two runs can agree perfectly on a guest
# that never did its job. Under --verify, Hermit writes the first run's
# stdout to the real stdout, so the console log is available either way.
marker=HERMIT-QEMU-BUSYBOX-PASS
grep -Fq "$marker" "$console_log" || fail \
  "guest exited without marker $marker; inspect $console_log"
clock_failures='Unable to calibrate against PIT|Clocksource .* skewed|Marking TSC unstable|No current clocksource'
if grep -Eq "^\[[[:space:]]*[0-9]+\.[0-9]+\].*($clock_failures)" "$console_log"; then
  fail "nested Linux reported a rejected clock failure; inspect $console_log"
fi
printf 'console_sha256=%s\n' \
  "$(sha256sum "$console_log" | cut -d' ' -f1)"

if [[ $verify == 1 ]]; then
  # Read the typed verdict rather than the banner. The message counts must be
  # positive: two empty logs also compare equal.
  [[ -s $verify_json ]] || fail \
    "VERIFY=1 produced no typed verdict at $verify_json; inspect $stderr_log"
  if ! jq -e '
    .verified == true and
    .verdict == "matched" and
    .bitwise_parity == true and
    .comparison.strictness == "canonical" and
    .comparison.compare_logs == true and
    .comparison.strip_lines == false and
    .comparison.ignore_lines == false and
    .comparison.skip_commit == false and
    .comparison.skip_detlog == false and
    (.compared_log_messages.left | type) == "number" and
    (.compared_log_messages.right | type) == "number" and
    .compared_log_messages.left > 0 and
    .compared_log_messages.right > 0 and
    .compared_log_messages.left == .compared_log_messages.right
  ' "$verify_json" >/dev/null; then
    fail "Hermit did not emit a matched, non-empty canonical verdict; inspect $verify_json and $stderr_log"
  fi
  printf 'compared_info_messages=%s\n' \
    "$(jq -r '.compared_log_messages.left' "$verify_json")"
  printf 'PASS: two runs of the BusyBox boot produced identical output and event logs (ptrace backend)\n'
else
  printf 'PASS: BusyBox userspace completed under Hermit/QEMU (ptrace backend, --strict)\n'
fi
echo
echo "=== Demo 9: QEMU BusyBox Boot: SUCCESS ==="
