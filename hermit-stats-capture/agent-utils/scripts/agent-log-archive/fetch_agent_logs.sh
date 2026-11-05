#!/usr/bin/env bash
# Compatibility entrypoint; the typed Python implementation is canonical.
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
exec python3 "$script_dir/fetch_agent_logs.py" "$@"
