#!/usr/bin/env bash
# End-to-end exercise of image-backed slots against real git, real mounts.
# Usage: e2e_images.sh [kernel|fuse] [nested|flat]
set -euo pipefail
BACKEND=${1:-kernel}
LAYOUT=${2:-nested}
HERE=$(cd "$(dirname "$0")" && pwd)
PYROOT=$(cd "$HERE/../.." && pwd)
BASE=${E2E_BASE:-/var/tmp/wrkslots-e2e-$USER}/$BACKEND-$LAYOUT
export WRKSLOTS_IMAGE_BACKEND=$BACKEND
W() { PYTHONPATH="$PYROOT" python3 -m wrkslots "$@"; }
say() { printf '\n=== %s\n' "$*"; }

# ---- clean up anything left from an earlier run
if [ -d "$BASE" ]; then
  for m in $(awk -v b="$BASE" '$5 ~ "^"b {print $5}' /proc/self/mountinfo | sort -r); do
    sudo -n umount "$m" 2>/dev/null || fusermount -u "$m" 2>/dev/null || true
  done
  rm -rf "$BASE"
fi
mkdir -p "$BASE"; cd "$BASE"

say "source repo + bare remote"
git init -q --bare remote.git
git init -q -b main src
git -C src config user.email e2e@example.com; git -C src config user.name e2e
echo hello > src/README; git -C src add README; git -C src commit -qm init
git -C src remote add origin "$BASE/remote.git"; git -C src push -q origin main
git -C src fetch -q origin

say "project init (new project => image default)"
mkdir -p project/tools
cat > project/tools/liveness.py <<'EOF'
#!/usr/bin/env python3
import os, sys
pid = os.environ.get("E2E_OWNER_ALIVE_FILE")
sys.exit(1 if pid and os.path.exists(pid) else 0)
EOF
chmod +x project/tools/liveness.py
cd project
W init . --liveness-command tools/liveness.py --heartbeat-ttl-seconds 1 --layout "$LAYOUT" \
  --image-ceiling-gib 20 --cache-glob target
grep -E 'slot_representation|image' .wrkslots.yml || { echo "FAIL: image not default"; exit 1; }

REPO=../src
sleep 1000 & OWNER=$!
export E2E_OWNER_ALIVE_FILE=$BASE/owner-alive; touch "$E2E_OWNER_ALIVE_FILE"

say "create image-backed agent slot"
W create s1 --slot-type agent --coordinator-authorized --agent a1 --task t1 --purpose e2e \
  --coordinator-pid $$ --owner-pid $OWNER --repo src=$REPO --branch src=agent/s1
SLOT=worktrees/slots/s1
CO=$SLOT; [ "$LAYOUT" = nested ] && CO=$SLOT/src
findmnt -no FSTYPE,SOURCE "$(realpath $SLOT)"
ls -la "$SLOT"
ls worktrees/ worktrees/slot-images/agent/s1
git -C "$CO" status --short --branch

say "write content, including a big regenerable cache dir"
mkdir -p "$CO/target"; dd if=/dev/zero of="$CO/target/blob" bs=1M count=200 status=none
echo work > "$CO/work.txt"; git -C "$CO" add work.txt; git -C "$CO" -c user.email=e -c user.name=e commit -qm work
W status >/dev/null && echo "status ok (slot not flagged)"
W image status

# The box is the same for both representations; run_box_probe is reused for a
# plain-worktree slot below.
run_box_probe() {
  W run "$1" --memory-max 2G --tasks-max 256 -- bash -c '
  set -e; echo "cwd=$PWD home=$HOME tmp=$TMPDIR repr=$WRKSLOTS_SLOT_REPRESENTATION"; cat /proc/self/cgroup
  [ "$WRKSLOTS_SLOT_REPRESENTATION" = "'"$2"'" ]
  touch $WRKSLOTS_SLOT_PATH/ok-from-sandbox && echo slot-writable
  touch /tmp/x && echo tmp-writable
  for p in '"$BASE"'/escape '"$BASE"'/src/escape '"$BASE"'/project/escape ~/.config/.wrkslots-e2e-escape; do
    if touch "$p" 2>/dev/null; then echo "ESCAPED $p"; exit 9; fi
  done
  echo outside-readonly
  echo x > ~/.wrkslots-e2e-top && echo home-top-level-private
  mkdir -p ~/.cache/probe && echo cache-writable
'
  [ ! -e "$HOME/.wrkslots-e2e-top" ] || { echo "FAIL: real HOME modified"; exit 1; }
}

say "sandboxed run: writes allowed only in the slot"
run_box_probe s1 image
MARKER=$HOME/.wrkslots-e2e-marker-$$; touch "$MARKER"
W run s1 --home hidden -- bash -c 'test ! -e "'"$MARKER"'" || { echo "HOME leaked"; exit 9; }; echo home-hidden'
W run s1 -- bash -c 'test -e "'"$MARKER"'" && ! touch "'"$MARKER"'" 2>/dev/null && echo home-readonly'
rm -f "$MARKER"
rm -f "$SLOT/ok-from-sandbox" "$CO/ok-from-sandbox" 2>/dev/null || true

say "remove: owner dies, TTL expires, salvage publishes, image destroyed"
kill $OWNER; wait $OWNER 2>/dev/null || true; rm -f "$E2E_OWNER_ALIVE_FILE"; sleep 2
git -C "$CO" status --short
W remove s1 --coordinator-pid $$ --expected-generation 1
[ ! -e "$SLOT" ] && echo "slot path gone"
ls worktrees/slot-images/agent 2>/dev/null | grep -q s1 && { echo "FAIL: image dir remains"; exit 1; } || echo "image dir gone"
git -C "$BASE/remote.git" log --oneline --all | head -5

say "worktree-representation slot in the same project still works; convert both ways"
sleep 1000 & OWNER=$!; touch "$E2E_OWNER_ALIVE_FILE"
W create s2 --slot-type agent --coordinator-authorized --agent a2 --task t2 --purpose e2e \
  --coordinator-pid $$ --owner-pid $OWNER --repo src=$REPO --branch src=agent/s2 --representation worktree
CO2=worktrees/slots/s2; [ "$LAYOUT" = nested ] && CO2=$CO2/src
echo dirty > "$CO2/untracked.txt"
findmnt "$(realpath worktrees/slots/s2)" >/dev/null && { echo "FAIL: s2 should not be a mount"; exit 1; } || echo "s2 is plain"
say "sandboxed run on the plain-worktree slot: the same box"
run_box_probe s2 worktree
rm -f worktrees/slots/s2/ok-from-sandbox "$CO2/ok-from-sandbox" 2>/dev/null || true
W image convert s2 --to image
findmnt -no FSTYPE "$(realpath worktrees/slots/s2)"
git -C "$CO2" status --short
cat "$CO2/untracked.txt"
W image convert s2 --to worktree
findmnt "$(realpath worktrees/slots/s2)" >/dev/null && { echo "FAIL: still mounted"; exit 1; } || echo "s2 plain again"
git -C "$CO2" status --short
kill $OWNER; wait $OWNER 2>/dev/null || true; rm -f "$E2E_OWNER_ALIVE_FILE"; sleep 2
W remove s2 --coordinator-pid $$ --expected-generation 1
echo "E2E PASS ($BACKEND, $LAYOUT)"
