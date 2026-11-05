#!/usr/bin/env bash
# Opt-in acceptance: run real agent harnesses headless inside `wrkslots run --isolation root`
# and check what they could write.
#
# Skipped (exit 0) unless every prerequisite is present:
#   WRKSLOTS_ACCEPT_HARNESSES  one harness per line, LABEL=COMMAND. COMMAND is a headless,
#                              non-interactive invocation; the probe prompt is appended as its
#                              last argument. Example:
#                                WRKSLOTS_ACCEPT_HARNESSES='codex=codex exec --dangerously-bypass-approvals-and-sandbox'
#   passwordless `sudo -n`, a systemd user manager, git.
# Optional:
#   WRKSLOTS_ACCEPT_READ_WRITE extra configuration.sandbox.read_write entries, one per line
#                              (a harness's credential staging directory, for example; `~` and
#                              $USER are expanded by wrkslots)
#   WRKSLOTS_ACCEPT_TIMEOUT    seconds per harness (default 300)
#   E2E_BASE                   scratch parent directory, outside /tmp (default /var/tmp)
#
# Each harness is asked to run one probe script. The probe records, in the slot, whether writes
# succeeded to: the slot, a blessed output directory, the project's primary checkout, a
# directory outside the project, /tmp, a new top-level $HOME file, ~/.cache, and the wrkslots
# control directory (the registry). The script then
# asserts the expected outcome of each and that the real $HOME was not modified.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
PYROOT=$(cd "$HERE/../.." && pwd)
skip() { echo "SKIP: $*"; exit 0; }
[ -n "${WRKSLOTS_ACCEPT_HARNESSES:-}" ] || skip "WRKSLOTS_ACCEPT_HARNESSES is not set"
[ "$(id -u)" != 0 ] || skip "run as a regular user"
sudo -n true 2>/dev/null || skip "passwordless sudo -n is unavailable"
systemctl --user show-environment >/dev/null 2>&1 || skip "no systemd user manager"
command -v git >/dev/null || skip "git is not installed"

TIMEOUT=${WRKSLOTS_ACCEPT_TIMEOUT:-300}
BASE=$(mktemp -d "${E2E_BASE:-/var/tmp}/wrkslots-accept-XXXXXX")
W() { python3 "$PYROOT/wrkslots/__main__.py" --project-root "$BASE/project" "$@"; }
OWNER=
cleanup() {
  [ -n "$OWNER" ] && kill "$OWNER" 2>/dev/null
  chmod -R u+w "$BASE" 2>/dev/null
  rm -rf "$BASE"
}
[ -n "${WRKSLOTS_ACCEPT_KEEP:-}" ] || trap cleanup EXIT

set -e
git init -q --bare "$BASE/remote.git"
mkdir -p "$BASE/project" "$BASE/outside"
git clone -q "$BASE/remote.git" "$BASE/project/src" 2>/dev/null
git -C "$BASE/project/src" -c user.name=t -c user.email=t@example.invalid commit -q --allow-empty -m seed
git -C "$BASE/project/src" push -q -u origin HEAD 2>/dev/null
mkdir -p "$BASE/project/ai_docs"
printf '#!/usr/bin/env python3\nraise SystemExit(1)\n' > "$BASE/project/liveness.py"
chmod +x "$BASE/project/liveness.py"
WRKSLOTS_INIT_REPRESENTATION=worktree python3 "$PYROOT/wrkslots/__main__.py" init "$BASE/project" --worktrees-dir worktrees \
  --liveness-command liveness.py >/dev/null
PYTHONPATH="$PYROOT" python3 - "$BASE/project/.wrkslots.yml" <<'EOF'
import os, sys
from pathlib import Path
from wrkslots import cli
path = Path(sys.argv[1])
config = dict(cli._read_config(path))
extra = [line for line in os.environ.get("WRKSLOTS_ACCEPT_READ_WRITE", "").splitlines() if line.strip()]
config["sandbox"] = {**config["sandbox"], "isolation": "root", "read_write": extra}
cli._write_config(path, config)
EOF
sleep 100000 & OWNER=$!
W create acc --slot-type agent --coordinator-authorized --agent accept --task accept --purpose accept \
  --owner-pid "$OWNER" --coordinator-pid $$ --repo src=src --branch src=accept/root >/dev/null
SLOT=$(W shell-command acc --format json | python3 -c 'import json,sys; print(json.load(sys.stdin)["slot_path"])')
PROBE=$BASE/probe.sh
cat > "$PROBE" <<EOF
#!/bin/sh
label=\$1
out="\$WRKSLOTS_SLOT_PATH/probe-\$label.txt"
: > "\$out"
# Each check runs in its own shell with the label as \$1.
try() { if sh -c "\$2" probe "\$label" >/dev/null 2>&1; then r=yes; else r=no; fi; echo "\$1=\$r" >> "\$out"; }
try slot 'echo x > "\$WRKSLOTS_SLOT_PATH/written-\$1"'
try output 'echo x > "$BASE/project/ai_docs/written-\$1"'
try primary_checkout 'echo x > "$BASE/project/src/written-\$1"'
try outside 'echo x > "$BASE/outside/written-\$1"'
try tmp 'echo x > /tmp/written-\$1'
try home_top_level 'echo x > "\$HOME/.wrkslots-accept-\$1"'
try home_cache 'echo x > "\$HOME/.cache/wrkslots-accept-\$1"'
try registry 'echo x > "$BASE/project/worktrees/stray-\$1"'
EOF
chmod +x "$PROBE"
set +e

EXPECTED="slot=yes output=yes primary_checkout=no outside=no tmp=yes home_top_level=yes home_cache=yes registry=no"
failures=0
while IFS= read -r entry; do
  [ -n "${entry// }" ] || continue
  label=${entry%%=*}; command=${entry#*=}
  echo "=== $label: $command"
  prompt="Run exactly this shell command and then reply DONE: sh $PROBE $label"
  # shellcheck disable=SC2086  # COMMAND is a word list by contract
  (cd "$SLOT" && timeout "$TIMEOUT" python3 "$PYROOT/wrkslots/__main__.py" --project-root "$BASE/project" \
     run acc --isolation root -- $command "$prompt") < /dev/null > "$BASE/$label.out" 2>&1
  echo "exit $? (log: $BASE/$label.out, removed at exit)"
  STATE=$(find "$BASE/project/worktrees" -type d -path '*slot-state/agent/acc' | head -1)
  results=$(tr '\n' ' ' < "$SLOT/probe-$label.txt" 2>/dev/null | sed 's/ $//')
  if [ "$results" != "$EXPECTED" ]; then
    echo "FAIL $label: got '${results:-no probe output}'"; tail -20 "$BASE/$label.out"; failures=$((failures + 1))
  elif [ -e "$HOME/.wrkslots-accept-$label" ] || [ -e "$HOME/.cache/wrkslots-accept-$label" ]; then
    echo "FAIL $label: the real \$HOME was modified"; failures=$((failures + 1))
  elif [ ! -e "$STATE/home/.wrkslots-accept-$label" ]; then
    echo "FAIL $label: the top-level \$HOME write did not land in the slot's layer"; failures=$((failures + 1))
  else
    echo "PASS $label: $results"
  fi
done <<< "$WRKSLOTS_ACCEPT_HARNESSES"
systemctl --user stop "$(python3 -c "import sys; sys.path.insert(0, '$PYROOT'); from wrkslots import sandbox; print(sandbox.slice_name('agent', 'acc'))")" 2>/dev/null
[ "$failures" = 0 ] && echo "ACCEPT PASS" || { echo "ACCEPT FAIL ($failures)"; exit 1; }
