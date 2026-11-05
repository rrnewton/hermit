#!/usr/bin/env bash
# End-to-end: image-slot mount operations from INSIDE a coordinator box, against real mounts.
#
#   e2e_coordinator_images.sh [root|userns] [kernel|fuse]
#
# A coordinator box's own mount table can keep copies of slot mounts that are already gone on
# the host, so every image operation it requests runs in the host namespace (through the
# user's systemd manager) and is verified in the host's mount table. This script drives that
# path for real. It first creates one image slot on the host, before the box starts. Then,
# from inside a coordinator box, it unmounts and remounts that slot, creates a second image
# slot, converts it to plain worktrees and back, and removes both slots. After every step it
# checks the host's mount table (PID 1's namespace, via sudo nsenter). At the end it checks
# that no mount or loop device under the test directory leaked.
#
# A slot's box must refuse the same operations, so the script also asks one to unmount an
# image and checks the refusal and that nothing changed.
#
# Needs passwordless sudo, a systemd user manager, git, and mkfs.ext4 (plus fuse2fs for fuse).
# The test directory (E2E_BASE, default /var/tmp) is removed at exit unless E2E_KEEP is set.
set -uo pipefail
ISOLATION=${1:-root}
BACKEND=${2:-kernel}
HERE=$(cd "$(dirname "$0")" && pwd)
PYROOT=$(cd "$HERE/../.." && pwd)
skip() { echo "SKIP: $*"; exit 0; }
[ "$(id -u)" != 0 ] || skip "run as a regular user"
sudo -n true 2>/dev/null || skip "passwordless sudo -n is unavailable"
systemctl --user show-environment >/dev/null 2>&1 || skip "no systemd user manager"
command -v mkfs.ext4 >/dev/null || skip "mkfs.ext4 is not installed"
[ "$BACKEND" != fuse ] || command -v fuse2fs >/dev/null || skip "fuse2fs is not installed"
if [ "$ISOLATION" = userns ]; then unshare -rm true 2>/dev/null || skip "no unprivileged user namespaces"; fi

PY=$(python3 -c 'import os, sys; print(os.path.realpath(sys.executable))')
BASE=$(mktemp -d "${E2E_BASE:-/var/tmp}/wrkslots-e2e-coordinator-XXXXXX")
PROJECT=$BASE/project
export WRKSLOTS_IMAGE_BACKEND=$BACKEND
W() { "$PY" "$PYROOT/wrkslots/__main__.py" --project-root "$PROJECT" "$@"; }
host_mounts() { sudo -n nsenter -t 1 -m findmnt -rn -o TARGET | grep -F "$BASE/" | sort; }
loops() { for f in /sys/block/loop*/loop/backing_file; do [ -f "$f" ] && grep -F "$BASE/" "$f"; done 2>/dev/null | sort; }
cleanup() {
  for pidfile in "$BASE/owner.pid" "$BASE/scratch/owner1.pid"; do
    [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null
  done
  host_mounts | sort -r | while read -r target; do
    sudo -n nsenter -t 1 -m umount "$target" 2>/dev/null || fusermount -u "$target" 2>/dev/null
  done
  chmod -R u+w "$BASE" 2>/dev/null
  rm -rf "$BASE"
}
[ -n "${E2E_KEEP:-}" ] || trap cleanup EXIT
failures=0
check() { # check LABEL EXPECTED ACTUAL
  if [ "$2" = "$3" ]; then echo "PASS $1"; else echo "FAIL $1: expected '$2', got '$3'"; failures=$((failures + 1)); fi
}
mounted_on_host() { host_mounts | grep -qx "$1" && echo yes || echo no; }

set -e
git init -q --bare "$BASE/remote.git"
git clone -q "$BASE/remote.git" "$PROJECT/src" 2>/dev/null
echo seed > "$PROJECT/src/seed"
git -C "$PROJECT/src" add seed
git -C "$PROJECT/src" -c user.name=t -c user.email=t@example.invalid commit -q -m seed
git -C "$PROJECT/src" push -q -u origin HEAD 2>/dev/null
mkdir -p "$BASE/scratch"  # writable from inside the box (--read-write)
cat > "$PROJECT/liveness.py" <<EOF
#!/usr/bin/env python3
import os, sys
sys.exit(1 if os.path.exists("$BASE/scratch/owner-alive") else 0)
EOF
chmod +x "$PROJECT/liveness.py"
export E2E_OWNER_ALIVE_FILE=$BASE/scratch/owner-alive
"$PY" "$PYROOT/wrkslots/__main__.py" init "$PROJECT" --worktrees-dir worktrees --liveness-command liveness.py \
  --heartbeat-ttl-seconds 1 --slot-representation image --image-ceiling-gib 2 >/dev/null
setsid sleep 100000 </dev/null >/dev/null 2>&1 & echo $! > "$BASE/owner.pid"
touch "$E2E_OWNER_ALIVE_FILE"
W create s0 --slot-type agent --coordinator-authorized --agent a0 --task t --purpose e2e \
  --owner-pid "$(cat "$BASE/owner.pid")" --coordinator-pid $$ --repo src=src --branch src=e2e/s0 >/dev/null
set +e
S0=$PROJECT/worktrees/s0
S0STATE=$PROJECT/worktrees/slot-images/agent/s0/state
S1=$PROJECT/worktrees/s1
check "s0 mounted on the host before the box starts" "yes yes" "$(mounted_on_host "$S0") $(mounted_on_host "$S0STATE")"

echo "=== a slot's box refuses image mount operations"
W run s0 --isolation "$ISOLATION" --tmp-size 64M -- "$PY" -I -c "
import sys; sys.path.insert(0, '$PYROOT')
from pathlib import Path
from wrkslots import slotimage
import tempfile
try:
    slotimage.mount(Path('$PROJECT/worktrees/slot-images/agent/s0/slot.img'), Path(tempfile.mkdtemp()), 'auto')
except slotimage.ImageError as exc:
    print('refused:', exc)
try:
    slotimage.remove_mount_point(Path('$S0'))
except slotimage.ImageError as exc:
    print('refused:', exc)
" > "$BASE/slotbox.out" 2>&1
check "slot box refuses" "yes" "$(grep -q "inside a slot's box" "$BASE/slotbox.out" && echo yes || echo no)"
check "slot box changed nothing" "yes yes" "$(mounted_on_host "$S0") $(mounted_on_host "$S0STATE")"

echo "=== inside a $ISOLATION coordinator box: unmount, mount, create, convert, remove"
cat > "$BASE/coordinator.sh" <<EOF
#!/bin/bash
W() { "$PY" "$PYROOT/wrkslots/__main__.py" "\$@"; }
step() {
  echo "--- \$1"; shift; "\$@" > "$BASE/scratch/step.log" 2>&1; rc=\$?; echo "rc=\$rc"
  if [ \$rc != 0 ]; then
    tail -3 "$BASE/scratch/step.log"
    pid=\$(grep -o 'live process [0-9]*' "$BASE/scratch/step.log" | grep -o '[0-9]*' | head -1)
    if [ -n "\$pid" ]; then
      echo "process \$pid: \$(tr '\\0' ' ' < /proc/\$pid/cmdline 2>/dev/null | cut -c1-200) ns=\$(readlink /proc/\$pid/ns/mnt 2>/dev/null) self=\$(readlink /proc/self/ns/mnt)"
      grep -F "$PROJECT" /proc/\$pid/mountinfo 2>/dev/null | head -8
    fi
  fi
  return \$rc
}
mark() { echo "\$1" >> "$BASE/scratch/steps.done"; }
step "unmount s0 (mounted before this box started)" W image unmount s0 && mark unmount-s0
# The box's own copies of s0's mounts must not keep its loop devices (or FUSE servers) alive.
held=0
for f in /sys/block/loop*/loop/backing_file; do [ -f "\$f" ] && grep -qF "$PROJECT/worktrees/slot-images/agent/s0/" "\$f" && held=\$((held + 1)); done
fuse=\$(pgrep -u "\$(id -u)" -f "fuse2fs.*$PROJECT/worktrees/slot-images/agent/s0/" | wc -l)
echo "s0 loop devices still bound: \$held; s0 fuse2fs servers: \$fuse"
[ "\$held" = 0 ] && [ "\$fuse" = 0 ] && mark s0-released
step "mount s0 again" W image mount && mark mount-s0
# s1's owner descends from this coordinator, as create requires.
setsid sleep 100000 </dev/null >/dev/null 2>&1 & echo \$! > $BASE/scratch/owner1.pid
step "create image slot s1" W create s1 --slot-type agent --coordinator-authorized --agent a1 --task t \\
  --purpose e2e --owner-pid "\$(cat $BASE/scratch/owner1.pid)" --coordinator-pid \$\$ --repo src=src --branch src=e2e/s1 && mark create-s1
echo boxed > "$S1/src/from-box"
step "convert s1 to worktree" W image convert s1 --to worktree && mark to-worktree
step "convert s1 back to image" W image convert s1 --to image && mark to-image
rm -f "$E2E_OWNER_ALIVE_FILE"; kill "\$(cat $BASE/owner.pid)" "\$(cat $BASE/scratch/owner1.pid)"; sleep 2
step "remove s1" W remove s1 --coordinator-pid \$\$ --expected-generation 1 && mark remove-s1
step "remove s0" W remove s0 --coordinator-pid \$\$ --expected-generation 1 && mark remove-s0
EOF
( cd "$PROJECT/worktrees" && timeout "${E2E_TIMEOUT:-3600}" "$PY" "$PYROOT/wrkslots/__main__.py" --project-root "$PROJECT" \
    box --name e2e --isolation "$ISOLATION" --tmp-size 64M --read-write "$BASE/remote.git" --read-write "$BASE/scratch" --cwd "$PROJECT/worktrees" \
    -- bash "$BASE/coordinator.sh" ) </dev/null 2>&1 | grep --line-buffered -v '^wrkslots run: limits' | tee "$BASE/box.log"
check "every step inside the box succeeded" \
  "unmount-s0 s0-released mount-s0 create-s1 to-worktree to-image remove-s1 remove-s0" \
  "$(tr '\n' ' ' < "$BASE/scratch/steps.done" 2>/dev/null | sed 's/ $//')"
published=no
for ref in $(git -C "$BASE/remote.git" for-each-ref --format='%(refname)'); do
  git -C "$BASE/remote.git" ls-tree -r --name-only "$ref" | grep -qx from-box && published=yes
done
check "remove published the file written in s1 from inside the box" "yes" "$published"
check "no slot directory remains" "no no" "$([ -e "$S0" ] && echo yes || echo no) $([ -e "$S1" ] && echo yes || echo no)"
check "no host mount under the test directory" "" "$(host_mounts | tr '\n' ' ' | sed 's/ $//')"
# A loop device can outlive its host mount only while some mount namespace still holds a copy
# of that mount (the kernel does not propagate an unmount into every namespace; concurrent test
# harnesses on a busy host create such namespaces). A loop device that no mount anywhere holds
# is a real leak.
orphans=0
for device in $(for f in /sys/block/loop*/loop/backing_file; do [ -f "$f" ] && grep -qF "$BASE/" "$f" && basename "$(dirname "$(dirname "$f")")"; done); do
  holders=$(sudo -n sh -c "grep -l ' /dev/$device ' /proc/[0-9]*/mountinfo 2>/dev/null" | head -3)
  if [ -z "$holders" ]; then orphans=$((orphans + 1)); else
    for holder in $holders; do pid=$(basename "$(dirname "$holder")"); echo "NOTE /dev/$device still held by a mount copy in the namespace of pid $pid: $(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | cut -c1-120)"; done
  fi
done
check "no loop device leaked (bound with no mount anywhere)" "0" "$orphans"
W status 2>&1 | grep -q 'active=0' && echo "PASS registry empty" || { echo "FAIL registry not empty"; failures=$((failures + 1)); }
[ "$failures" = 0 ] && echo "E2E PASS (coordinator box, $ISOLATION, $BACKEND)" || { echo "E2E FAIL ($failures)"; exit 1; }
