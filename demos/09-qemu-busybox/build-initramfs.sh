#!/usr/bin/env bash
# Build the demo 9 initramfs: a static BusyBox plus the /init script beside
# this file, archived reproducibly so the same BusyBox gives the same bytes.

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)
busybox=${BUSYBOX:-}
output=${1:-$repo_root/target/qemu-busybox/initramfs-busybox.cpio.gz}

if [[ -z $busybox ]]; then
  busybox=$(command -v busybox || true)
fi
[[ -n $busybox && -x $busybox ]] || fail \
  "statically linked BusyBox not found; install it or set BUSYBOX"
file "$busybox" | grep -q 'statically linked' || fail \
  "BusyBox must be statically linked: $busybox"

for command in cpio file find gzip install sha256sum sort stat touch wc; do
  command -v "$command" >/dev/null || fail "$command is required"
done

# Every command /init runs, other than the shell's builtins, must be a BusyBox
# applet, and its shell must support pipefail, which /init needs to see a
# failure in any stage of its pipeline. Either gap would otherwise show only
# inside the guest, after the boot.
applet_names=$("$busybox" --list) || fail \
  "BusyBox could not list its applets: $busybox"
missing_applets=()
for applet in bc head ls mknod mount poweroff printf sh sha256sum sort uname; do
  case $'\n'"$applet_names"$'\n' in
    *$'\n'"$applet"$'\n'*) ;;
    *) missing_applets+=("$applet") ;;
  esac
done
[[ ${#missing_applets[@]} -eq 0 ]] || fail \
  "BusyBox lacks applets the guest's /init runs (${missing_applets[*]}): $busybox"
"$busybox" sh -c 'set -o pipefail' 2>/dev/null || fail \
  "BusyBox's sh does not support 'set -o pipefail', which the guest's /init needs: $busybox"
if "$busybox" sh -c 'set -o pipefail; false | true' 2>/dev/null; then
  fail "BusyBox's sh accepts 'set -o pipefail' but ignores it: $busybox"
fi

mkdir -p "$repo_root/target/qemu-busybox" "$(dirname -- "$output")"
root=$(mktemp -d "$repo_root/target/qemu-busybox/root.XXXXXX")
cleanup() {
  rm -rf -- "$root"
}
trap cleanup EXIT

mkdir -p "$root"/{bin,dev,etc,home,proc,root,sbin,sys,tmp,usr}
install -m 0755 "$busybox" "$root/bin/busybox"
install -m 0755 "$script_dir/init" "$root/init"
printf 'root:x:0:0:root:/:/bin/sh\n' >"$root/etc/passwd"
printf 'root:x:0:\n' >"$root/etc/group"

while IFS= read -r applet; do
  [[ $applet == bin/busybox ]] && continue
  mkdir -p "$root/$(dirname -- "$applet")"
  ln -s /bin/busybox "$root/$applet"
done < <("$busybox" --list-full)

# Fix every archive input that otherwise varies across hosts or invocations.
find "$root" -exec touch -h -d @0 {} +
(
  cd "$root"
  find . -print0 | LC_ALL=C sort -z | \
    cpio --quiet --null --create --format=newc --owner=0:0 --reproducible
) | gzip -n -9 >"$output"

printf 'busybox=%s\nbusybox_sha256=%s\ninitramfs=%s\ninitramfs_sha256=%s\nentries=%s\nbytes=%s\n' \
  "$busybox" "$(sha256sum "$busybox" | cut -d' ' -f1)" \
  "$output" "$(sha256sum "$output" | cut -d' ' -f1)" \
  "$(gzip -dc "$output" | cpio -it 2>/dev/null | wc -l)" \
  "$(stat -c %s "$output")"
