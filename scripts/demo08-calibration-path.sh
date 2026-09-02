#!/usr/bin/env bash
# Classify the retained Demo 8 preparation evidence used by demo-hot-path.yml.

set -euo pipefail

log=
force_cold=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --log)
      log=${2:-}
      shift 2
      ;;
    --force-cold)
      force_cold=${2:-}
      shift 2
      ;;
    -h | --help)
      cat <<'EOF'
usage: demo08-calibration-path.sh --log FILE --force-cold true|false

Classify one retained Demo 8 preparation log and refuse missing, conflicting,
or forced-cold evidence that did not come from a cold calibration.
EOF
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

[[ -n $log ]] || { echo 'error: --log FILE is required' >&2; exit 2; }
case "$force_cold" in
  true | false) ;;
  *) echo 'error: --force-cold must be true or false' >&2; exit 2 ;;
esac

if [[ ! -r $log ]]; then
  echo no-evidence
  echo "error: Demo 8 calibration log is unreadable: $log" >&2
  exit 1
fi

read -r cold_count cached_count < <(
  awk '
    index($0, "Demo 8 crash seed calibrated:") { cold++ }
    index($0, "Demo 8 crash seed replayed: cached seed") { cached++ }
    END { print cold + 0, cached + 0 }
  ' "$log"
)

if [[ $cold_count -eq 1 && $cached_count -eq 0 ]]; then
  actual=cold-calibration
elif [[ $cold_count -eq 0 && $cached_count -eq 1 ]]; then
  actual=cached-seed-replay
elif [[ $cold_count -eq 0 && $cached_count -eq 0 ]]; then
  actual=no-evidence
else
  actual=conflicting-evidence
fi
printf '%s\n' "$actual"

if [[ $actual == no-evidence ]]; then
  echo 'error: Demo 8 preparation published no calibration-success marker' >&2
  exit 1
fi
if [[ $actual == conflicting-evidence ]]; then
  echo "error: Demo 8 preparation published $cold_count cold and $cached_count cached success markers" >&2
  exit 1
fi
if [[ $force_cold == true && $actual != cold-calibration ]]; then
  echo 'error: forced cold Demo 8 preparation did not publish the cold-calibration marker' >&2
  exit 1
fi
