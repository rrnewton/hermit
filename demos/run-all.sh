#!/usr/bin/env bash
# Run the selected Hermit demos with one result row and log per demo.

set -uo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
LOG_DIR="${DEMO_SWEEP_LOG_DIR:-$ROOT/target/demo-sweep}"
SUMMARY="$LOG_DIR/summary.tsv"

with_analyze=0
with_qemu=0
with_all=0
for arg in "$@"; do
  case "$arg" in
    --with-analyze) with_analyze=1 ;;
    --with-qemu) with_qemu=1 ;;
    --all) with_analyze=1; with_qemu=1; with_all=1 ;;
    *)
      echo "usage: $0 [--with-analyze] [--with-qemu] [--all]" >&2
      exit 2
      ;;
  esac
done

if [ -n "${DEMO_SWEEP_TARGETS:-}" ]; then
  # Test/debug override. Production super validation intentionally leaves this
  # unset and uses --all, which is fixed to all eight checked-in demos.
  read -r -a demos <<<"$DEMO_SWEEP_TARGETS"
else
  demos=(demo1 demo2 demo3)
  [ "$with_analyze" -eq 0 ] || demos+=(demo4)
  [ "$with_qemu" -eq 0 ] || demos+=(demo5 demo6)
  [ "$with_all" -eq 0 ] || demos+=(demo7 demo8)
fi

if [ "${#demos[@]}" -eq 0 ]; then
  echo "error: no demos selected" >&2
  exit 2
fi

read -r -a make_command <<<"${MAKE:-make}"
mkdir -p "$LOG_DIR"
: >"$SUMMARY"
printf 'demo\tstatus\texit\tduration_seconds\tlog\n' >>"$SUMMARY"

# Build once and share one scratch directory across the process demos.
export DEMO_TMP="${DEMO_TMP:-$(mktemp -d -t hermit-demo.XXXXXX)}"

failures=0
for demo in "${demos[@]}"; do
  log="$LOG_DIR/$demo.log"
  started=$SECONDS
  printf '\n=== %s: START ===\n' "$demo"

  "${make_command[@]}" -C "$DEMO_DIR" --no-print-directory "$demo" \
    2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  duration=$((SECONDS - started))

  if [ "$rc" -eq 0 ]; then
    status=PASS
    printf '=== %s: PASS (%ss) ===\n' "$demo" "$duration"
    # Demos 2-4 consume the same already-built Hermit binaries as demo 1.
    [ "$demo" != demo1 ] || export DEMO_SKIP_BUILD=1
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
    echo '## Nightly Hermit demo sweep'
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
  printf '\n=== Demo suite: FAILURE — %s demo(s) failed ===\n' "$failures" >&2
  exit 1
fi

printf '\n=== Demo suite: SUCCESS — all %s requested demos passed ===\n' \
  "${#demos[@]}"
