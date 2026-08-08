#!/usr/bin/env bash
# Both-direction test for validate.sh's concurrency monitor.
#
# WHY THIS EXISTS. `concurrent_validates` is the field that makes a wall time
# comparable: median wall is 490s at 0-3 concurrent validates and 852s at 14+, so
# a wall time without it cannot be compared to another datapoint. It was recorded
# on only ~30% of ledger rows, and the cause was not a missing producer -- the
# monitor ran for the whole run and PROVED the answer was zero, then discarded
# that proof:
#
#   * the monitor wrote the marker only when `count > previous` with `previous=0`,
#     so an all-zero run never created the marker at all; and
#   * the finalizer matched `^[1-9][0-9]*$`, which rejects a measured 0 anyway.
#
# The fix must not be "assume 0 when we do not know" -- that would fabricate the
# very conditioning the series depends on. So the two cases this test separates
# are A MEASURED ZERO and A FAILED LOOK. They must not produce the same record.
#
# The monitor function is sourced FROM validate.sh, not reimplemented here: a
# copy would pass while the shipped code stayed broken.
set -uo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fail=0
pass=0

note() { printf '  %-58s %s\n' "$1" "$2"; }
check() { # name expected actual
  if [[ $2 == "$3" ]]; then note "$1" "ok ($3)"; pass=$((pass + 1));
  else note "$1" "FAIL expected=[$2] got=[$3]"; fail=$((fail + 1)); fi
}

# Extract the monitor verbatim from the shipped script.
sed -n '/^function start_validation_concurrency_monitor {$/,/^}$/p' \
    "$repo_root/validate.sh" >"$tmp/monitor.sh"
[[ -s $tmp/monitor.sh ]] || { echo "could not extract monitor from validate.sh"; exit 2; }

# The monitor watches `$$` and exits when that process dies, so it must run in its
# OWN process -- inside a subshell `$$` is still the parent and the loop never ends.
cat >"$tmp/driver.sh" <<'DRIVER'
set +u
VALIDATION_CONCURRENT_MARKER="$1"
PATH="$2:$PATH"
# shellcheck disable=SC1090
source "$3"
start_validation_concurrency_monitor
sleep 2
DRIVER

run_monitor() { # $1 = dir containing the ps stub
  VALIDATION_CONCURRENT_MARKER="$tmp/marker"
  rm -f "$VALIDATION_CONCURRENT_MARKER"
  timeout 20s bash "$tmp/driver.sh" "$tmp/marker" "$1" "$tmp/monitor.sh" >/dev/null 2>&1
  sleep 1.5   # let the loop observe the driver exit and stop
}

# ---------------------------------------------------------------- MEASURED ZERO
# ps works and reports a process table with no peer validate.sh. The monitor must
# record 0, because it looked and there was nothing there.
mkdir -p "$tmp/bin-zero"
cat >"$tmp/bin-zero/ps" <<'EOF'
#!/usr/bin/env bash
# The monitor calls ps TWICE with different argv; serve both.
case "$*" in
  *"-o pgid="*"-p"*) printf '%s\n' "  9999" ;;
  *) printf '%s\n' "  1234 /usr/bin/some-unrelated-process --flag" ;;
esac
EOF
chmod +x "$tmp/bin-zero/ps"
run_monitor "$tmp/bin-zero"
check "measured zero records 0 (was: no marker at all)" "0" "$(cat "$tmp/marker" 2>/dev/null)"

# ------------------------------------------------------------------ FAILED LOOK
# ps cannot enumerate. This is NOT zero peers -- it is "we did not find out", and
# it must leave NO marker so the receipt records null/UNKNOWN.
mkdir -p "$tmp/bin-broken"
cat >"$tmp/bin-broken/ps" <<'EOF'
#!/usr/bin/env bash
# The monitor calls ps TWICE with different argv; serve both.
case "$*" in
  *"-o pgid="*"-p"*) printf '%s\n' "  9999" ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$tmp/bin-broken/ps"
run_monitor "$tmp/bin-broken"
check "failed look leaves NO marker (stays UNKNOWN)" "" "$(cat "$tmp/marker" 2>/dev/null)"

# ------------------------------------------------------------- PEER OBSERVED
# A real peer in a different process group must be counted, and must beat the
# zero the first sample may have written.
mkdir -p "$tmp/bin-peer"
cat >"$tmp/bin-peer/ps" <<'EOF'
#!/usr/bin/env bash
# The monitor calls ps TWICE with different argv; serve both.
case "$*" in
  *"-o pgid="*"-p"*) printf '%s\n' "  9999" ;;
  *) printf '%s\n' "  4242 /home/other/validate.sh full"; printf '%s\n' "  4243 /home/other2/validate.sh full" ;;
esac
EOF
chmod +x "$tmp/bin-peer/ps"
run_monitor "$tmp/bin-peer"
check "two peer process groups record 2" "2" "$(cat "$tmp/marker" 2>/dev/null)"

# --------------------------------------------------------- OWN PGID EXCLUDED
# A gate that invokes validate.sh internally shares this process group and must
# not be able to forge concurrency.
mkdir -p "$tmp/bin-self"
cat >"$tmp/bin-self/ps" <<'EOF'
#!/usr/bin/env bash
# Report the monitor's OWN pgid as a validate.sh -- it must be excluded.
case "$*" in
  *"-o pgid="*"-p"*) printf '%s\n' "  9999" ;;
  *) printf '%s\n' "  9999 /repo/validate.sh full" ;;
esac
EOF
chmod +x "$tmp/bin-self/ps"
run_monitor "$tmp/bin-self"
check "own process group excluded -> 0, not 1" "0" "$(cat "$tmp/marker" 2>/dev/null)"

# ------------------------------------------------- FINALIZER ACCEPTS ZERO
# The other half of the bug: even a correct 0 was discarded by the finalizer's
# pattern. Assert against the pattern as it appears in the shipped script.
pattern_line=$(grep -n 'concurrent_validates_json =~' "$repo_root/validate.sh" | head -1)
if [[ $pattern_line == *'^[0-9]+$'* ]]; then
  note "finalizer accepts a measured 0" "ok"
  pass=$((pass + 1))
else
  note "finalizer accepts a measured 0" "FAIL still rejects 0: $pattern_line"
  fail=$((fail + 1))
fi

echo
echo "concurrency-marker: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
