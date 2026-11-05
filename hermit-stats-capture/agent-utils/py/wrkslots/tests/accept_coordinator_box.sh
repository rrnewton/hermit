#!/usr/bin/env bash
# Opt-in acceptance test: a real subcoordinator in a root coordinator box.
#
#   WRKSLOTS_ACCEPT_COORDINATOR=1 py/wrkslots/tests/accept_coordinator_box.sh
#
# A root-isolation coordinator box (`wrkslots box --isolation root`) creates two slots, one
# plain worktree and one disk image. The script then checks that the registry recorded both,
# and that the image slot is mounted and visible on the host and inside the box. The
# coordinator must be able to write both slots but not a sibling directory of the project
# or the real $HOME.
#
# A second coordinator box then launches one worker into each slot with the Rust agentctl
# (`--slot SLOT --slot-isolation root`, through Herdr) and asks each worker to run a write
# probe. Each worker must write only its own slot. This phase needs Herdr, a built Rust
# agentctl (AGENTCTL_BIN, default rs/target/{release,debug}/agentctl), and the worker
# harness (WRKSLOTS_ACCEPT_WORKER_HARNESS, default claude; claude and codex are supported).
# Without them it is skipped and the script says so. The harness's folder-trust question is
# answered in each slot's own private copy of ~/.claude.json, never in the real one. For
# codex, whose trust decisions live in the shared ~/.codex, the question must already be
# answered, or agentctl stops at the prompt.
#
# Harness launchers that stage credentials need that directory writable. List it in
# WRKSLOTS_ACCEPT_READ_WRITE (one path per line; $USER is expanded), for example
# /var/.../$USER/<staging dir>.
#
# The project is created under $HOME (WRKSLOTS_ACCEPT_BASE, default $HOME), so the
# read-only $HOME binds are exercised, and removed at exit unless WRKSLOTS_ACCEPT_KEEP is set.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
PYROOT=$(cd "$HERE/../.." && pwd)
REPO=$(cd "$PYROOT/.." && pwd)
skip() { echo "SKIP: $*"; exit 0; }
[ -n "${WRKSLOTS_ACCEPT_COORDINATOR:-}" ] || skip "WRKSLOTS_ACCEPT_COORDINATOR is not set"
[ "$(id -u)" != 0 ] || skip "run as a regular user"
sudo -n true 2>/dev/null || skip "passwordless sudo -n is unavailable"
systemctl --user show-environment >/dev/null 2>&1 || skip "no systemd user manager"
command -v git >/dev/null || skip "git is not installed"
command -v mkfs.ext4 >/dev/null || skip "mkfs.ext4 is not installed"

PY=$(python3 -c 'import os, sys; print(os.path.realpath(sys.executable))')
TIMEOUT=${WRKSLOTS_ACCEPT_TIMEOUT:-300}
BASE=$(mktemp -d "${WRKSLOTS_ACCEPT_BASE:-$HOME}/.wrkslots-accept-box-XXXXXX")
PROJECT=$BASE/project
CONTROL=$PROJECT/worktrees
W() { "$PY" "$PYROOT/wrkslots/__main__.py" --project-root "$PROJECT" "$@"; }
WORKSPACE=
cleanup() {
  if [ -n "$WORKSPACE" ]; then herdr workspace close "$WORKSPACE" >/dev/null 2>&1; fi
  [ -f "$CONTROL/owner.pid" ] && kill "$(cat "$CONTROL/owner.pid")" 2>/dev/null
  findmnt -rn -o TARGET | grep -F "$BASE/" | sort -r | while read -r target; do sudo -n umount "$target"; done
  chmod -R u+w "$BASE" 2>/dev/null
  rm -rf "$BASE"
}
[ -n "${WRKSLOTS_ACCEPT_KEEP:-}" ] || trap cleanup EXIT
failures=0
check() { # check LABEL EXPECTED ACTUAL
  if [ "$2" = "$3" ]; then echo "PASS $1"; else echo "FAIL $1: expected '$2', got '$3'"; failures=$((failures + 1)); fi
}

set -e
git init -q --bare "$BASE/remote.git"
mkdir -p "$PROJECT" "$BASE/sibling"
git clone -q "$BASE/remote.git" "$PROJECT/src" 2>/dev/null
git -C "$PROJECT/src" -c user.name=t -c user.email=t@example.invalid commit -q --allow-empty -m seed
git -C "$PROJECT/src" push -q -u origin HEAD 2>/dev/null
printf '#!/usr/bin/env python3\nraise SystemExit(1)\n' > "$PROJECT/liveness.py"
chmod +x "$PROJECT/liveness.py"
"$PY" "$PYROOT/wrkslots/__main__.py" init "$PROJECT" --worktrees-dir worktrees --liveness-command liveness.py \
  --slot-representation worktree --image-backend kernel >/dev/null
PYTHONPATH="$PYROOT" "$PY" - "$PROJECT/.wrkslots.yml" <<'EOF'
import os, sys
from pathlib import Path
from wrkslots import cli
path = Path(sys.argv[1])
config = dict(cli._read_config(path))
extra = [line for line in os.environ.get("WRKSLOTS_ACCEPT_READ_WRITE", "").splitlines() if line.strip()]
config["sandbox"] = {**config["sandbox"], "isolation": "root", "read_write": extra}
config["image"] = {**config.get("image", {}), "ceiling_bytes": 2 * 1024**3, "state_ceiling_bytes": 1024**3}
cli._write_config(path, config)
EOF
set +e

# ---------------------------------------------------------------- phase 1: create slots
cat > "$BASE/coordinator.sh" <<EOF
#!/bin/bash
W() { "$PY" "$PYROOT/wrkslots/__main__.py" "\$@"; }
out=$CONTROL/coordinator-results.txt
: > "\$out"
t() { if sh -c "\$2" >/dev/null 2>&1; then echo "\$1=yes" >> "\$out"; else echo "\$1=no" >> "\$out"; fi; }
# The slots' owner: a process that descends from this coordinator.
setsid sleep 100000 </dev/null >/dev/null 2>&1 & echo \$! > $CONTROL/owner.pid
for spec in cwt:worktree cimg:image; do
  slot=\${spec%%:*}; representation=\${spec#*:}
  W create "\$slot" --slot-type agent --coordinator-authorized --agent "a-\$slot" --task accept \\
    --purpose "coordinator box acceptance" --owner-pid "\$(cat $CONTROL/owner.pid)" --coordinator-pid \$\$ \\
    --repo src=src --branch "src=accept/\$slot" --representation "\$representation" >> $CONTROL/coordinator.log 2>&1
done
t image_mounted_inside '[ "\$(stat -c %d $CONTROL/cimg)" != "\$(stat -c %d $CONTROL)" ] && [ -d $CONTROL/cimg/src ]'
t write_worktree_slot 'echo x > $CONTROL/cwt/src/coordinator-wrote'
t write_image_slot 'echo x > $CONTROL/cimg/src/coordinator-wrote'
t write_registry 'echo x > $CONTROL/coordinator-registry-probe'
t write_sibling 'echo x > $BASE/sibling/coordinator-wrote'
t write_primary_checkout 'echo x > $PROJECT/src/coordinator-wrote'
probe=.wrkslots-accept-box-\$\$
t write_home_top_level "echo x > \$HOME/\$probe"
echo "home_probe=\$probe" >> "\$out"
EOF
echo "=== phase 1: a root coordinator box creates a worktree slot and an image slot"
( cd "$CONTROL" && timeout "$TIMEOUT" "$PY" "$PYROOT/wrkslots/__main__.py" --project-root "$PROJECT" \
    box --name accept --isolation root --cwd "$CONTROL" -- bash "$BASE/coordinator.sh" ) </dev/null > "$BASE/phase1.log" 2>&1
echo "exit $?"
results=$(grep -v '^home_probe=' "$CONTROL/coordinator-results.txt" 2>/dev/null | tr '\n' ' ' | sed 's/ $//')
check "coordinator writes" \
  "image_mounted_inside=yes write_worktree_slot=yes write_image_slot=yes write_registry=yes write_sibling=no write_primary_checkout=no write_home_top_level=yes" \
  "$results"
status=$(W status 2>&1)
check "registry records both slots" "2" "$(grep -cE '/(cwt|cimg): type=agent' <<< "$status")"
check "image slot mounted on the host" "yes" "$([ "$(stat -c %d "$CONTROL/cimg")" != "$(stat -c %d "$CONTROL")" ] && echo yes || echo no)"
check "coordinator's image write visible on the host" "yes" "$([ -f "$CONTROL/cimg/src/coordinator-wrote" ] && echo yes || echo no)"
if sudo -n nsenter -t 1 -m true 2>/dev/null; then
  check "image slot mounted in PID 1's mount namespace" "yes" \
    "$(sudo -n nsenter -t 1 -m findmnt -n -o SOURCE -T "$CONTROL/cimg" 2>/dev/null | grep -q '^/dev/loop' && echo yes || echo no)"
fi
probe=$(sed -n 's/^home_probe=//p' "$CONTROL/coordinator-results.txt" 2>/dev/null)
check "real \$HOME untouched" "no" "$([ -n "$probe" ] && [ -e "$HOME/$probe" ] && echo yes || echo no)"
check "top-level \$HOME write in the box's layer" "yes" \
  "$([ -n "$probe" ] && [ -e "$CONTROL/box-state/accept/home/$probe" ] && echo yes || echo no)"

# ------------------------------------------------------- phase 2: workers via agentctl
AGENTCTL=${AGENTCTL_BIN:-}
for candidate in "$REPO/rs/target/release/agentctl" "$REPO/rs/target/debug/agentctl"; do
  [ -n "$AGENTCTL" ] || { [ -x "$candidate" ] && AGENTCTL=$candidate; }
done
HARNESS=${WRKSLOTS_ACCEPT_WORKER_HARNESS:-claude}
worker_skip=
command -v herdr >/dev/null || worker_skip="herdr is not installed"
[ -n "$worker_skip" ] || [ -n "$AGENTCTL" ] || worker_skip="no Rust agentctl (set AGENTCTL_BIN or build rs/agentctl)"
[ -n "$worker_skip" ] || command -v "$HARNESS" >/dev/null || worker_skip="worker harness $HARNESS is not installed"
if [ -n "$worker_skip" ]; then
  echo "SKIP phase 2 (workers through agentctl): $worker_skip"
else
  echo "=== phase 2: the coordinator box launches a $HARNESS worker into each slot with agentctl"
  WORKSPACE=$(herdr workspace create --cwd "$PROJECT" --label wrkslots-accept-box --no-focus \
    | "$PY" -c 'import json, sys; print(json.load(sys.stdin)["result"]["workspace"]["workspace_id"])')
  if [ "$HARNESS" = claude ]; then
    # Answer the folder-trust question in each slot's private layer copy of ~/.claude.json.
    for slot in cwt cimg; do
      W run "$slot" --isolation root -- true >/dev/null 2>&1
      layer=$(find "$CONTROL/slot-state/agent/$slot/home" "$CONTROL/slot-images/agent/$slot/state/home" \
        -maxdepth 0 -type d 2>/dev/null | head -1)
      "$PY" - "$layer/.claude.json" "$CONTROL/$slot" <<'EOF'
import json, sys
from pathlib import Path
path, slot = Path(sys.argv[1]), sys.argv[2]
data = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
data.setdefault("projects", {}).setdefault(slot, {})["hasTrustDialogAccepted"] = True
path.write_text(json.dumps(data), encoding="utf-8")
EOF
    done
    ARG=--dangerously-skip-permissions
  else
    ARG=--dangerously-bypass-approvals-and-sandbox
  fi
  cat > "$BASE/probe.sh" <<EOF
#!/bin/sh
out="\$PWD/worker-probe.txt"; other=\$1
: > "\$out"
t() { if sh -c "\$2" >/dev/null 2>&1; then echo "\$1=yes" >> "\$out"; else echo "\$1=no" >> "\$out"; fi; }
t own_slot 'echo x > "\$PWD/src/worker-wrote"'
t other_slot "echo x > $CONTROL/\$other/src/worker-escape"
t registry 'echo x > $CONTROL/worker-registry-probe'
t sibling 'echo x > $BASE/sibling/worker-wrote'
EOF
  printf '#!/bin/sh\nexec "%s" "%s" "$@"\n' "$PY" "$PYROOT/wrkslots/__main__.py" > "$BASE/wrkslots"
  chmod +x "$BASE/wrkslots"
  cat > "$BASE/launcher.sh" <<EOF
#!/bin/bash
cd $CONTROL
export AGENTCTL_WRKSLOTS_BIN=$BASE/wrkslots
A() { "$AGENTCTL" "\$@"; }
for pair in cwt:cimg cimg:cwt; do
  slot=\${pair%%:*}; other=\${pair#*:}
  A start "w-\$slot" --harness $HARNESS --cwd $PROJECT --slot "\$slot" --slot-isolation root \\
    --workspace-id $WORKSPACE --harness-arg=$ARG --startup-timeout 180 > $CONTROL/start-\$slot.json 2>&1
  echo "start \$slot exit \$?" >> $CONTROL/launcher.log
  A send "w-\$slot" "Run exactly this shell command and then reply DONE: sh $BASE/probe.sh \$other" >> $CONTROL/launcher.log 2>&1
  A wait "w-\$slot" --timeout 240 >> $CONTROL/launcher.log 2>&1
  A stop "w-\$slot" >> $CONTROL/launcher.log 2>&1
done
EOF
  ( cd "$CONTROL" && timeout $((TIMEOUT * 4)) "$PY" "$PYROOT/wrkslots/__main__.py" --project-root "$PROJECT" \
      box --name accept --isolation root --cwd "$CONTROL" -- bash "$BASE/launcher.sh" ) </dev/null > "$BASE/phase2.log" 2>&1
  echo "exit $?"
  for slot in cwt cimg; do
    got=$(tr '\n' ' ' < "$CONTROL/$slot/worker-probe.txt" 2>/dev/null | sed 's/ $//')
    check "worker in $slot confined to its slot" "own_slot=yes other_slot=no registry=no sibling=no" "${got:-no probe output}"
    [ -n "$got" ] || tail -5 "$CONTROL/launcher.log" "$CONTROL/start-$slot.json" 2>/dev/null
  done
fi
for slot in cwt cimg; do systemctl --user stop "$("$PY" -c "import sys; sys.path.insert(0, '$PYROOT'); from wrkslots import sandbox; print(sandbox.slice_name('agent', '$slot'))")" 2>/dev/null; done
systemctl --user stop "$("$PY" -c "import sys; sys.path.insert(0, '$PYROOT'); from wrkslots import sandbox; print(sandbox.slice_name('box', 'accept'))")" 2>/dev/null
[ "$failures" = 0 ] && echo "ACCEPT PASS${worker_skip:+ (phase 2 skipped: $worker_skip)}" || { echo "ACCEPT FAIL ($failures)"; exit 1; }
