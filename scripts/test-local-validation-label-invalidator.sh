#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Execute the production workflow step with an inert label and a fake GitHub
# client. This test must never create a real locally-validated authorization.
set -euo pipefail

ROOT_DIR=${1:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}
WORKFLOW="$ROOT_DIR/.github/workflows/merge-gate.yml"
FIXTURE_LABEL=inert-validation-cache-fixture

fail() {
    echo "test-local-validation-label-invalidator.sh: $*" >&2
    exit 1
}

fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
mkdir -p "$fixture/bin"

# Extract the exact production mutation step instead of maintaining a test copy.
awk '
    $0 == "      - name: Remove local-validation label when its evidence is no longer valid" {
        found = 1
        next
    }
    found && $0 == "        run: |" {
        capture = 1
        next
    }
    capture && $0 != "" && $0 !~ /^          / {
        exit
    }
    capture {
        sub(/^          /, "")
        print
        lines++
    }
    END {
        if (!found || !capture || lines == 0) {
            exit 1
        }
    }
' "$WORKFLOW" >"$fixture/invalidate-step.sh" ||
    fail "could not extract the unique production invalidator step"

cat >"$fixture/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%q ' "$@" >>"$GH_FIXTURE_LOG"
printf '\n' >>"$GH_FIXTURE_LOG"

args=" $* "
if [[ $args == *" repos/fixture/repo/pulls/7 "* ]]; then
    jq -n --arg fixture_label "$FIXTURE_LABEL" '{
        state: "open",
        head: {sha: "fixture-head-sha", ref: "fixture-head", repo: {full_name: "fixture/repo"}},
        labels: [{name: $fixture_label}]
    }'
elif [[ $args == *" comments?per_page=100 "* && $args != *" --method POST "* ]]; then
    printf '[[{"body":"inert receipt marker fixture"}]]\n'
fi
EOF
chmod +x "$fixture/bin/gh"

cat >"$fixture/accept-receipt" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%q ' "$@" >>"$VERIFIER_FIXTURE_LOG"
printf '\n' >>"$VERIFIER_FIXTURE_LOG"
printf 'receipt_path=fixture/receipt.json sha256=fixture producer_coverage_status=complete\n'
EOF
cat >"$fixture/reject-receipt" <<'EOF'
#!/usr/bin/env bash
printf '%q ' "$@" >>"$VERIFIER_FIXTURE_LOG"
printf '\n' >>"$VERIFIER_FIXTURE_LOG"
exit 1
EOF
chmod +x "$fixture/accept-receipt" "$fixture/reject-receipt"

run_fixture() {
    local verifier=$1 log=$2
    : >"$log"
    : >"${log}.verifier"
    export FIXTURE_LABEL
    PATH="$fixture/bin:$PATH" \
        GH_FIXTURE_LOG="$log" \
        LABEL_NAME="$FIXTURE_LABEL" \
        PR_NUMBER=7 \
        RECEIPT_VERIFIER="$verifier" \
        REPO=fixture/repo \
        VERIFIER_FIXTURE_LOG="${log}.verifier" \
        GITHUB_RUN_ID=fixture-run \
        GITHUB_SERVER_URL=https://fixture.invalid \
        bash "$fixture/invalidate-step.sh"
}

backed_log="$fixture/backed.log"
run_fixture "$fixture/accept-receipt" "$backed_log"
backed_strips=$(grep -Fc -- '--method DELETE' "$backed_log" || true)
[[ $backed_strips == 0 ]] || fail "backed inert fixture was stripped $backed_strips time(s)"
[[ $(wc -l <"${backed_log}.verifier") == 1 ]] || fail "backed fixture did not invoke the verifier exactly once"
grep -Fq -- '--repo fixture/repo --sha fixture-head-sha --comments' "${backed_log}.verifier" ||
    fail "backed fixture did not bind the verifier to the exact fixture head"

unbacked_log="$fixture/unbacked.log"
run_fixture "$fixture/reject-receipt" "$unbacked_log"
unbacked_strips=$(grep -Fc -- '--method DELETE' "$unbacked_log" || true)
unbacked_failures=$(grep -Fc -- 'check-runs' "$unbacked_log" || true)
[[ $unbacked_strips == 1 ]] || fail "unbacked inert fixture was stripped $unbacked_strips time(s)"
[[ $unbacked_failures == 2 ]] || fail "unbacked inert fixture published $unbacked_failures failing check(s), expected 2"
[[ $(wc -l <"${unbacked_log}.verifier") == 1 ]] || fail "unbacked fixture did not invoke the verifier exactly once"
grep -Fq -- '--repo fixture/repo --sha fixture-head-sha --comments' "${unbacked_log}.verifier" ||
    fail "unbacked fixture did not bind the verifier to the exact fixture head"

echo "test-local-validation-label-invalidator.sh: N=1 backed inert fixture retained; N=1 unbacked inert fixture stripped; real authorization labels created=0"
