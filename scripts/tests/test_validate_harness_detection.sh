#!/usr/bin/env bash
# Both-ways bracket for validate.rs harness detection.
#
# A refusal that fires EVERYWHERE is as broken as one that never fires: the
# second protects nothing, and the first gets disabled within a day because it
# blocks the legitimate path. So this asserts three things, not one:
#
#   1. bare inside a dev-hermit workspace  -> REFUSED (exit 4), naming the real command
#   2. bare in a standalone hermit checkout -> RUNS to completion (exit 0)
#   3. admitted (CI_HUB_VALIDATE_PRODUCER set) inside dev-hermit -> NOT refused
#
# (3) is the one that keeps the gate alive: ci-hub itself must still be able to
# drive validate.rs, or the gate would break the very path it points at.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
SUT="$here/../validate.rs"
fail=0
ok()   { printf '  ok    %s\n' "$1"; }
bad()  { printf '  FAIL  %s\n         %s\n' "$1" "$2"; fail=1; }

# ---------------------------------------------------------------- 1. refuse
echo "1. bare, inside the dev-hermit workspace -- must REFUSE"
out=$(cd "$here/.." && "$SUT" portable 2>&1); rc=$?
[ "$rc" -eq 4 ] && ok "exit 4" || bad "exit 4" "got exit=$rc"
grep -q "refusing to run bare inside the dev-hermit workspace" <<<"$out" \
  && ok "says it is refusing, and why" || bad "refusal text" "missing"
# The message must NAME the command, not merely complain.
grep -q "ci-hub/ci-hub validate-run" <<<"$out" \
  && ok "names ci-hub validate-run" || bad "names the command" "no 'ci-hub/ci-hub validate-run'"
grep -q -- "--checkout" <<<"$out" && grep -q -- "--target" <<<"$out" \
  && ok "names the required flags" || bad "flags" "missing --checkout/--target"
# It must not invent a subcommand that does not exist.
if grep -qE "ci-hub +validate( |$)" <<<"$out"; then
  bad "no invented subcommand" "names bare 'ci-hub validate', which does not exist"
else
  ok "does not name a nonexistent 'ci-hub validate'"
fi

# ------------------------------------------------------- 2. standalone runs
echo "2. bare, in a STANDALONE hermit checkout -- must SUCCEED"
sa=$(mktemp -d /tmp/standalone-hermit.XXXX)
mkdir -p "$sa/ci/dag" "$sa/scripts"
cp "$SUT" "$sa/scripts/validate.rs"; chmod +x "$sa/scripts/validate.rs"
# validate.rs takes a rust-script path dependency on ../agent-utils/rs/safe-ci-dag-runner.
# agent-utils is a SUBMODULE OF THE HERMIT REPO, so a real standalone checkout with
# submodules initialised has it; the fixture models that with a symlink rather than a
# copy. Without this the standalone case fails to BUILD, which would look like the gate
# rejecting it -- a false negative that hides whether the gate behaved.
ln -s "$here/../../agent-utils" "$sa/agent-utils"
# validate.rs also #[path]-includes scripts/lib/rust_script_prelude.rs.
cp -a "$here/../lib" "$sa/scripts/lib"
cat > "$sa/ci/dag/tiny.json" <<'JSON'
{
  "resource_caps": {},
  "mem_cap_factor": 1.25,
  "mem_cap_floor_bytes": 8589934592,
  "outer_mem_safety_factor": 1.0,
  "default_step_timeout": 60,
  "steps": [
    {"group": "t", "job": "noop", "desc": "a step that always succeeds", "cmd": "true", "timeout": 30}
  ]
}
JSON
( cd "$sa" && git init -q . && git add -A && git -c user.email=t@t -c user.name=t commit -qm init ) >/dev/null 2>&1
out=$(cd "$sa" && ./scripts/validate.rs tiny --allow-cgroup-failure 2>&1); rc=$?
if grep -q "refusing to run bare" <<<"$out"; then
  bad "must not refuse in a standalone checkout" "the gate fired where there is no ci-hub"
else
  ok "not refused"
fi
[ "$rc" -eq 0 ] && ok "ran to completion (exit 0)" \
  || bad "standalone run succeeds" "exit=$rc; tail: $(tail -3 <<<"$out" | tr '\n' ' ')"

# --------------------------------------------------- 3. admitted still runs
echo "3. ADMITTED inside dev-hermit (ci-hub's own path) -- must NOT be refused"
out=$(cd "$here/.." && CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 "$SUT" __no_such_profile__ 2>&1); rc=$?
if grep -q "refusing to run bare" <<<"$out"; then
  bad "admitted run must not be refused" "gate fired despite CI_HUB_VALIDATE_PRODUCER"
else
  ok "not refused when ci-hub launched it"
fi
# Reaching the DAG-file check proves we got PAST the gate rather than short-circuiting.
grep -q "no such DAG file" <<<"$out" && ok "proceeded to normal argument handling" \
  || bad "proceeds past the gate" "unexpected output: $(head -2 <<<"$out")"

rm -rf "$sa"
echo
[ "$fail" -eq 0 ] && echo "PASS" || echo "FAIL"
exit "$fail"
