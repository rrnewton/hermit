#!/usr/bin/env bash
# Run the selected Hermit demos, with one log and one summary row per demo.
# Exits nonzero if any selected demo fails.

set -uo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
LOG_DIR="${DEMO_SWEEP_LOG_DIR:-$ROOT/target/demo-sweep}"
SUMMARY="$LOG_DIR/summary.tsv"

usage() {
  cat <<'EOF'
Usage: demos/run-all.sh [--with-analyze] [--with-qemu] [--all] [--group N]

With no option, run the quick demos 1-3.
  --with-analyze  add demo 4 (schedule bisection with `hermit analyze`)
  --with-qemu     add demos 5, 6, and 9 (QEMU/Linux)
  --all           run demos 1-9
  --group N       run one verification group:
                    1  demos 1, 2, 3        (quick, process level)
                    2  demos 4, 8, 9        (analyze, btrfs, QEMU BusyBox)
                    3  demos 5, 6, 7        (QEMU snapshot, resume, drgn)

Logs and summary.tsv go to target/demo-sweep/ (override: DEMO_SWEEP_LOG_DIR).
EOF
}

with_analyze=0
with_qemu=0
with_all=0
group=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --with-analyze) with_analyze=1 ;;
    --with-qemu) with_qemu=1 ;;
    --all) with_analyze=1; with_qemu=1; with_all=1 ;;
    --group)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      group="$2"
      shift
      ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
  shift
done

if [ -n "${DEMO_SWEEP_TARGETS:-}" ]; then
  # Test and debugging override: an explicit list of make targets.
  read -r -a demos <<<"$DEMO_SWEEP_TARGETS"
elif [ -n "$group" ]; then
  case "$group" in
    1) demos=(demo1 demo2 demo3) ;;
    2) demos=(demo4 demo8 demo9) ;;
    3) demos=(demo5 demo6 demo7) ;;
    *) echo "error: --group must be 1, 2, or 3" >&2; exit 2 ;;
  esac
else
  demos=(demo1 demo2 demo3)
  [ "$with_analyze" -eq 0 ] || demos+=(demo4)
  [ "$with_qemu" -eq 0 ] || demos+=(demo5 demo6)
  [ "$with_all" -eq 0 ] || demos+=(demo7 demo8)
  [ "$with_qemu" -eq 0 ] || demos+=(demo9)
fi

if [ "${#demos[@]}" -eq 0 ]; then
  echo "error: no demos selected" >&2
  exit 2
fi

read -r -a make_command <<<"${MAKE:-make}"
mkdir -p "$LOG_DIR"
: >"$SUMMARY"
printf 'demo\tstatus\texit\tduration_seconds\tlog\n' >>"$SUMMARY"

# Demos 1-4 share one scratch directory and one build of their guest programs.
export DEMO_TMP="${DEMO_TMP:-$(mktemp -d -t hermit-demo.XXXXXX)}"

failures=0
passes=0
skips=0
for demo in "${demos[@]}"; do
  log="$LOG_DIR/$demo.log"
  started=$SECONDS
  printf '\n=== %s: START ===\n' "$demo"

  "${make_command[@]}" -C "$DEMO_DIR" --no-print-directory "$demo" \
    2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  duration=$((SECONDS - started))

  if [ "$rc" -eq 0 ]; then
    # A demo that cannot run exits 0 and prints a SKIPPED line (demo 8 does
    # this when its prepared assets are absent). Count it as a skip, not a pass.
    if grep -qE '^=== Demo [0-9]+: SKIPPED' "$log"; then
      status=SKIP
      skips=$((skips + 1))
      printf '=== %s: SKIP (%ss; log %s) ===\n' "$demo" "$duration" "$log"
    else
      status=PASS
      passes=$((passes + 1))
      printf '=== %s: PASS (%ss) ===\n' "$demo" "$duration"
    fi
    case "$demo" in
      demo1|demo2|demo3|demo4) export DEMO_SKIP_BUILD=1 ;;
    esac
  else
    status=FAIL
    failures=$((failures + 1))
    printf '=== %s: FAIL (exit %s, %ss; log %s) ===\n' \
      "$demo" "$rc" "$duration" "$log" >&2
    if [ "${GITHUB_ACTIONS:-}" = true ]; then
      printf '::error title=%s failed::exit %s after %ss; inspect %s\n' \
        "$demo" "$rc" "$duration" "$log"
    fi
  fi
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$demo" "$status" "$rc" "$duration" "$log" >>"$SUMMARY"
done

printf '\n=== Demo sweep summary ===\n'
while IFS=$'\t' read -r demo status rc duration log; do
  [ "$demo" != demo ] || continue
  printf '%-8s %-4s exit=%-3s duration=%4ss log=%s\n' \
    "$demo" "$status" "$rc" "$duration" "$log"
done <"$SUMMARY"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo '## Hermit demo sweep'
    echo
    echo '| Demo | Status | Exit | Duration |'
    echo '|---|---:|---:|---:|'
    while IFS=$'\t' read -r demo status rc duration _log; do
      [ "$demo" != demo ] || continue
      printf '| `%s` | **%s** | %s | %ss |\n' "$demo" "$status" "$rc" "$duration"
    done <"$SUMMARY"
  } >>"$GITHUB_STEP_SUMMARY"
fi

if [ "$failures" -ne 0 ]; then
  printf '\n=== Demo suite: FAILURE — %s demo(s) failed, %s passed, %s skipped ===\n' \
    "$failures" "$passes" "$skips" >&2
  exit 1
fi

# A skipped demo produced no result, so never report it as passed.
if [ "$skips" -ne 0 ]; then
  printf '\n=== Demo suite: INCOMPLETE — %s of %s requested demos passed, %s skipped and unmeasured ===\n' \
    "$passes" "${#demos[@]}" "$skips"
  exit 0
fi

printf '\n=== Demo suite: SUCCESS — all %s requested demos passed ===\n' \
  "${#demos[@]}"
