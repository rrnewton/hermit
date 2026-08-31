#!/usr/bin/env bash
# Regression tests for the runnable-demo review gate.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECKER="$ROOT/scripts/check-demo-review.sh"
pass=0
fail=0

ok() { printf '  PASS  %s\n' "$1"; pass=$((pass + 1)); }
bad() { printf '  FAIL  %s\n' "$1"; fail=$((fail + 1)); }

fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
git -C "$fixture" init -q -b main
git -C "$fixture" config user.name "fixture agent"
git -C "$fixture" config user.email fixture@example.com
mkdir -p "$fixture/demos"
printf 'seed\n' >"$fixture/plain.txt"
git -C "$fixture" add plain.txt
git -C "$fixture" commit -q -m seed
base=$(git -C "$fixture" rev-parse HEAD)

if (cd "$fixture" && "$CHECKER" --range not-a-revision..HEAD >/dev/null 2>&1); then
    bad "invalid range was accepted"
else
    ok "invalid range is refused"
fi

printf 'one\n' >"$fixture/demos/one.sh"
git -C "$fixture" add demos/one.sh
git -C "$fixture" commit -q -m $'demo one\n\nDemo-Green-Review: reviewer=reviewer-a result=GREEN evidence=first.log'
if (cd "$fixture" && "$CHECKER" --commit HEAD >/dev/null 2>&1); then
    bad "trailer without demo= was accepted"
else
    ok "trailer without demo= is refused"
fi

first=$(git -C "$fixture" rev-parse HEAD)
git -C "$fixture" commit --amend -q -m $'demo one\n\nDemo-Green-Review: reviewer=reviewer-a demo=demos/one.sh result=GREEN evidence=first.log'
first=$(git -C "$fixture" rev-parse HEAD)
printf 'two\n' >"$fixture/demos/two.sh"
git -C "$fixture" add demos/two.sh
git -C "$fixture" commit -q -m 'demo two without review'
if (cd "$fixture" && "$CHECKER" --range "$base..HEAD" >/dev/null 2>&1); then
    bad "a trailer older than the latest runnable-demo change was accepted"
else
    ok "a trailer older than the latest runnable-demo change is refused"
fi

printf 'review recorded\n' >>"$fixture/plain.txt"
git -C "$fixture" add plain.txt
git -C "$fixture" commit -q -m $'record review\n\nDemo-Green-Review: reviewer=reviewer-b demo=all result=GREEN evidence=second.log'
if (cd "$fixture" && "$CHECKER" --range "$base..HEAD" >/dev/null 2>&1); then
    ok "a complete trailer after the latest runnable-demo change is accepted"
else
    bad "a complete current trailer was refused"
fi

if (cd "$fixture" && "$CHECKER" --range "$first..HEAD" >/dev/null 2>&1); then
    ok "range inspection accepts the current review"
else
    bad "range inspection rejected the current review"
fi

printf 'plain\n' >>"$fixture/plain.txt"
git -C "$fixture" add plain.txt
git -C "$fixture" commit -q -m 'plain change'
plain_parent=$(git -C "$fixture" rev-parse HEAD^)
if (cd "$fixture" && "$CHECKER" --range "$plain_parent..HEAD" >/dev/null 2>&1); then
    ok "a range with no runnable-demo change remains exempt"
else
    bad "a range with no runnable-demo change was refused"
fi

printf '\ncheck-demo-review tests: %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
