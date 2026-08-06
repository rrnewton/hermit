#!/usr/bin/env bash
# Pre-commit gate: block a COMMITTED capability decrease in any machine scorecard.
#
# Compares each staged scorecard.csv against the committed version. A decrease
# needs BOTH a strong reason and a P0 that stays open until the capability is
# restored; the reason lives in a `decrease-reason.txt` beside the scorecard so
# it is reviewed in the same diff that lowers the number.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
tool="$root/compat-scorecard/scorecard.py"
rc=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    old=$(mktemp); new=$(mktemp)
    git show "HEAD:$f" >"$old" 2>/dev/null || : >"$old"
    git show ":$f"     >"$new" 2>/dev/null || : >"$new"
    reason="$root/$(dirname "$f")/decrease-reason.txt"
    args=(lint --old "$old" --new "$new")
    [ -f "$reason" ] && args+=(--reason "$reason")
    python3 "$tool" "${args[@]}" || rc=1
    rm -f "$old" "$new"
done < <(git diff --cached --name-only --diff-filter=AM | grep -E '^compat-scorecard/machines/.*/scorecard\.csv$' || true)
exit $rc
