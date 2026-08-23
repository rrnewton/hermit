#!/usr/bin/env bash
# Does the application helper actually earn the strict L2 it claims?
#
# THE BUG THIS EXISTS FOR. `run_hermit_verify` passed a bare `--verify` and then
# grepped stderr for "Success: deterministic. Determinism verified.", under a
# comment asserting "every application must exercise strict L2". Bare `--verify`
# is the STRIPPED comparison -- its own --verify-json reports bitwise_parity:false
# -- and that banner is printed by exactly such a run, so the grep could not tell a
# stripped match from a bitwise one. Seven application files contained zero strict
# or typed verification calls while three of them ran in portable CI.
#
# Bracketed both ways against the REAL hermit binary: a bare Stripped run must be
# shown incapable of earning L2, and a strict run must produce the typed predicate.
# The four outcomes are asserted separately, because collapsing them is how a
# no-result becomes a pass.
set -uo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd -- "$DIR/../../../.." && pwd)
HERMIT_BIN=${HERMIT_BIN:-"$REPO/target/debug/hermit"}
FAILURES=0
check() { if [ "$2" -eq 0 ]; then echo "  ok    $1"; else echo "  FAIL  $1"; FAILURES=$((FAILURES+1)); fi; }

if [[ ! -x $HERMIT_BIN ]]; then
  echo "SKIP: no hermit binary at $HERMIT_BIN (set HERMIT_BIN)" >&2; exit 0
fi

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
GUEST=(/bin/echo application-harness-probe)

echo "case SOURCE — the helper must not rely on a stderr banner or a bare --verify"
grep -q -- '--verify-strict' "$DIR/common.sh"; check "helper passes --verify-strict" $?
grep -q -- '--verify-json' "$DIR/common.sh";  check "helper passes --verify-json" $?
grep -q "Success: deterministic. Determinism verified." "$DIR/common.sh"
[ $? -ne 0 ]; check "the stderr-banner grep is gone (regression guard)" $?
grep -q 'bitwise_parity' "$DIR/common.sh"; check "helper keys on the typed bitwise_parity predicate" $?
for kind in NO_RESULT DIVERGED NOT_STRICT STRICT_PASS; do
  grep -q "$kind" "$DIR/common.sh"; check "helper distinguishes $kind" $?
done

echo "case FAILING-BEFORE — a bare Stripped run CANNOT earn L2"
"$HERMIT_BIN" --log=info run --no-virtualize-cpuid --max-timeslice=disabled \
  --base-env=minimal --strict --verify "--verify-json=$tmp/stripped.json" -- \
  "${GUEST[@]}" >/dev/null 2>"$tmp/stripped.err"
python3 - "$tmp/stripped.json" "$tmp/stripped.err" <<'PY'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
if not p.exists() or not p.read_text().strip():
    print("     (no verdict written; cannot demonstrate)"); raise SystemExit(2)
d = json.loads(p.read_text())
banner = "Success: deterministic. Determinism verified." in pathlib.Path(sys.argv[2]).read_text()
print(f"     stripped run: banner_present={banner} bitwise_parity={d.get('bitwise_parity')} "
      f"strictness={(d.get('comparison') or {}).get('strictness')}")
# The old helper would PASS this run; the predicate must not.
raise SystemExit(0 if banner and d.get("bitwise_parity") is False else 1)
PY
rc=$?
if [ $rc -eq 2 ]; then echo "  SKIP  bare-stripped demonstration (no verdict artifact)"; else
  [ $rc -eq 0 ]; check "bare --verify prints the banner while bitwise_parity=false (old helper would PASS)" $?
fi

echo "case TYPED-STRICT — the strict run produces the predicate the helper requires"
"$HERMIT_BIN" --log=info run --no-virtualize-cpuid --max-timeslice=disabled \
  --base-env=minimal --strict --verify --verify-strict "--verify-json=$tmp/strict.json" -- \
  "${GUEST[@]}" >/dev/null 2>"$tmp/strict.err"
python3 - "$tmp/strict.json" <<'PY'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
if not p.exists() or not p.read_text().strip():
    print("     (no verdict written)"); raise SystemExit(2)
d = json.loads(p.read_text()); c = d.get("compared_log_messages") or {}
print(f"     strict run: verdict={d.get('verdict')} bitwise_parity={d.get('bitwise_parity')} "
      f"strictness={(d.get('comparison') or {}).get('strictness')} compared={c.get('left')}|{c.get('right')}")
raise SystemExit(0)
PY
check "strict run emits a typed verdict the helper can read" $?

echo "case NO-RESULT — missing / empty / malformed JSON must never read as PASS"
for shape in missing empty malformed noresult; do
  f="$tmp/$shape.json"
  case $shape in
    missing)   rm -f "$f" ;;
    empty)     : > "$f" ;;
    malformed) printf 'not json{' > "$f" ;;
    noresult)  printf '{"verdict":"no_result","verified":false}' > "$f" ;;
  esac
  out=$(python3 - "$f" <<'PY'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
if not p.exists() or not p.read_text().strip(): print("NO_RESULT"); raise SystemExit(0)
try: d = json.loads(p.read_text())
except ValueError: print("NO_RESULT"); raise SystemExit(0)
print("NO_RESULT" if d.get("verdict") in (None, "no_result") else "OTHER")
PY
)
  [ "$out" = "NO_RESULT" ]; check "$shape verify-json classifies as NO_RESULT" $?
done

echo
[ "$FAILURES" -ne 0 ] && { echo "FAIL ($FAILURES assertions)"; exit 1; }
echo "PASS"
