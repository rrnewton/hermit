#!/usr/bin/env bash
# Cap check: the GitHub Actions workflow registry must match this checkout.
#
# Expected count is derived from the checkout's own .github/workflows/
# files, never a hardcoded total: the API also returns GitHub-managed
# synthetic workflows under dynamic/ (Copilot code review, Dependabot,
# CodeQL, Copilot agents). Those are not repository workflows and their
# count changes whenever GitHub enables a feature, which is how an
# unfiltered total drifted 14 -> 15 on 2026-11-04 while the checkout
# still had 11 files. Only paths under .github/workflows/ are counted.
#
# Works bare (plain shell, no pre-exported proxy) and in-tree (the
# ci-trigger workflow env): proxy and gh config default to this host's
# supported route when unset, and a transport failure is reported as
# such instead of as a count mismatch.
set -u
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root" || exit 2
repo=${CAP_REPO:-facebookexperimental/hermit}
export GH_CONFIG_DIR="${GH_CONFIG_DIR:-$HOME/.config/gh}"

expected=$(find .github/workflows -maxdepth 1 \( -name '*.yml' -o -name '*.yaml' \) -type f | wc -l | tr -d ' ')
if ! command -v gh >/dev/null 2>&1; then
    echo "run-cap: gh not found on PATH; cannot read the workflow registry" >&2
    exit 2
fi
if command -v with-proxy >/dev/null 2>&1; then
    gh_cmd="with-proxy gh"
else
    gh_cmd="gh"
fi
out=$($gh_cmd api "repos/$repo/actions/workflows" --cache 5m --jq \
    '[.workflows[].path | select(startswith(".github/workflows/"))] | sort | .[]' 2>&1)
rc=$?
if [ "$rc" -ne 0 ]; then
    echo "run-cap: workflow registry read failed (rc=$rc), not a count verdict:" >&2
    printf '%s\n' "$out" | head -5 >&2
    exit 2
fi
walked=$(printf '%s\n' "$out" | grep -c '^')
if [ "$walked" -ne "$expected" ]; then
    echo "workflow count: expected $expected, walked $walked" >&2
    diff \
        <(printf '%s\n' "$out" | sed 's|.*/||' | sort) \
        <(for f in .github/workflows/*.yml .github/workflows/*.yaml; do basename "$f"; done | sort) >&2 || true
    exit 1
fi
echo "workflow count: expected $expected, walked $walked (dynamic/* registry entries excluded)"
