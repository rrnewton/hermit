#!/usr/bin/env bash
# Prove no guarded checker fetches the pinned authority AFTER obtaining it.
#
# ⚠️ THIS ASKS WHETHER THE RACE CAN OCCUR, NOT WHETHER IT DID. The first version
# of this fix was verified by running a flaky substitute and observing a good
# outcome, which only samples. An intermittent defect that does not reproduce is
# not a defect that is gone.
#
# The method removes the sampling: the authority is obtained up front, and every
# checker then runs with a `gh` that returns HTTP 504 on EVERY call and counts
# its invocations. If any path still reaches the real authority it is counted and
# the checker fails. A zero count is evidence about the path, not about a run.
#
# The defect this guards, reported by the independent codex lane at head
# 327f6713: a preflight that fetched once and returned success, while each
# checker then launched fresh adapter processes that fetched again -- so a 504
# arriving after the probe still exited the checker nonzero, and
# ci/lint-checks-node.sh::classify_run correctly ranks that above any marker.
#
# BASELINE, AND ITS LIMIT. This test cannot be run against the refused head
# 327f6713 -- it depends on --materialize-authority, which that head lacks, so it
# honestly reports itself unevaluable there rather than passing vacuously. The
# property was therefore baselined directly instead, by reverting the five
# checker/helper files to 327f6713 and keeping the new adapters so a payload
# could be staged: the refused head exits the checker 1 with TWO fetches, the
# fixed head exits 0 with exactly ONE.
set -uo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d) || exit 1
trap 'rm -rf "$work"' EXIT

# A `gh` that serves the pinned authority on its FIRST call and returns HTTP 504
# on every call after, counting each. This is the ordering the codex lane
# reported, and it is what makes the test discriminating: the checker must be
# able to obtain once, and must then need no further fetch.
#
# ⚠️ AN EARLIER DRAFT OF THIS TEST PRE-SET DEV_HERMIT_PARENT FOR THE CHECKER AND
# WAS THEREFORE VACUOUS -- the adapter read the authority locally whether or not
# the checker obtained and exported anything, so the unfixed code would have
# passed it too. The checker must do its own obtain for this to test the fix.
mkdir -p "$work/bin"
cat > "$work/bin/gh" <<'STUB'
#!/usr/bin/env bash
n=$(wc -c < "$OBTAINED_ONCE_GH_CALLS")
printf 'x' >> "$OBTAINED_ONCE_GH_CALLS"
if [ "$n" -eq 0 ]; then
    exec cat "$OBTAINED_ONCE_PAYLOAD"
fi
echo "gh: HTTP 504 Gateway Timeout" >&2
exit 1
STUB
chmod +x "$work/bin/gh"

# Obtain both authorities first, with the real environment.
authority="$work/authority"
mkdir -p "$authority"
if ! "$ROOT_DIR/scripts/check_outcome_adapter.py" --materialize-authority "$authority" \
        >/dev/null 2>&1 \
   || ! "$ROOT_DIR/scripts/review_contract_adapter.py" --materialize-authority "$authority" \
        >/dev/null 2>&1; then
    echo "NO-RESULT-CASE: test-authority-obtained-once.sh: the pinned authorities could not be obtained, so the no-later-fetch property could not be tested"
    exit 0
fi

failures=0
for checker in test-required-check-outcomes.sh test-check-status-outcome.sh \
               check-merge-gate-policy.sh core-review-protocol-lint-test.sh; do
    calls="$work/calls"
    : > "$calls"
    # DEV_HERMIT_PARENT is deliberately pointed at an EMPTY directory: the
    # checker must obtain the authority itself, which is the behaviour under
    # test. Pointing it at the materialised copy would test the adapter instead.
    empty="$work/empty"
    rm -rf "$empty"; mkdir -p "$empty"
    payload="$authority/ci-hub/check_outcome.py"
    case "$checker" in core-review-protocol-lint-test.sh)
        payload="$authority/ci-hub/review_contract.py" ;;
    esac
    if ! env PATH="$work/bin:$PATH" DEV_HERMIT_PARENT="$empty" \
            OBTAINED_ONCE_PAYLOAD="$payload" \
            OBTAINED_ONCE_GH_CALLS="$calls" \
            "$ROOT_DIR/scripts/$checker" >"$work/out" 2>&1; then
        echo "FAIL: ${checker} did not complete with the authority already obtained" >&2
        tail -5 "$work/out" >&2
        failures=$((failures + 1))
        continue
    fi
    n=$(wc -c < "$calls")
    if [ "$n" -gt 1 ]; then
        echo "FAIL: ${checker} made ${n} authority fetches; anything past the first is a window a 504 can land in" >&2
        failures=$((failures + 1))
    fi
done

if [ "$failures" -ne 0 ]; then
    exit 1
fi
echo "PASS: no guarded checker fetches the authority after obtaining it"
