#!/usr/bin/env bash
# shellcheck disable=SC2034,SC2016
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

# Functions are extracted from validate.sh with eval below, so static analysis
# cannot see their reads of the fixture globals declared in this script.

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VALIDATE="$ROOT_DIR/validate.sh"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

function die {
    printf 'test-validate-smart-selection.sh: %s\n' "$*" >&2
    exit 1
}

function function_body {
    local name=$1
    awk -v signature="function $name {" '
        $0 == signature { inside = 1 }
        inside { print }
        inside && $0 == "}" { exit }
    ' "$VALIDATE"
}

for function_name in usable_selective_baseline github_repo_slug \
    bounded_github_api resolve_selective_baseline print_selection_coverage \
    run_full_dispatch; do
    body=$(function_body "$function_name")
    [[ -n $body ]] || die "missing validate.sh function: $function_name"
    eval "$body"
done

fixture_repo="$scratch/repo"
git init -q "$fixture_repo"
git -C "$fixture_repo" config user.name smart-selection-test
git -C "$fixture_repo" config user.email smart-selection-test@example.invalid
git -C "$fixture_repo" commit -q --allow-empty -m grandparent
grandparent=$(git -C "$fixture_repo" rev-parse HEAD)
git -C "$fixture_repo" commit -q --allow-empty -m parent
parent=$(git -C "$fixture_repo" rev-parse HEAD)
git -C "$fixture_repo" commit -q --allow-empty -m head
head=$(git -C "$fixture_repo" rev-parse HEAD)
VALIDATION_COMMIT=$head
GREEN_BASE_MAX_COMMITS=20
GITHUB_LOOKUP_TIMEOUT_SECONDS=2
VALIDATION_LEDGER_FILE="$scratch/ledger.jsonl"
VALIDATION_SLOT=test-slot
LOG_FILE="$scratch/selection.log"
GITHUB_REPOSITORY=rrnewton/hermit
cd "$fixture_repo"

mock_bin="$scratch/bin"
mkdir -p "$mock_bin"
real_path=$PATH

function write_proxy_mock {
    cat >"$mock_bin/with-proxy" <<'EOF'
#!/usr/bin/env bash
exec "$@"
EOF
    chmod +x "$mock_bin/with-proxy"
}

function write_gh_failure_mock {
    cat >"$mock_bin/gh" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
    chmod +x "$mock_bin/gh"
}

function write_gh_evidence_mock {
    local label_sha=$1 ci_sha=$2
    cat >"$mock_bin/gh" <<EOF
#!/usr/bin/env bash
case "\${*: -1}" in
  *'/pulls?'*)
    printf '%s\n' '[{"head":{"sha":"$label_sha"},"labels":[{"name":"locally-validated"}]}]'
    ;;
  *'/runs?'*)
    printf '%s\n' '{"workflow_runs":[{"head_sha":"$ci_sha","conclusion":"success"}]}'
    ;;
  *) exit 1 ;;
esac
EOF
    chmod +x "$mock_bin/gh"
}

write_proxy_mock
write_gh_failure_mock
PATH="$mock_bin:$real_path"
export PATH GITHUB_REPOSITORY

function resolve_quiet {
    resolve_selective_baseline 2>>"$scratch/expected-diagnostics.log"
}

cat >"$VALIDATION_LEDGER_FILE" <<EOF
{"result":"pass","profile":"full","selection_mode":"full","commit_anchored":true,"tree_dirty":false,"commit":"$grandparent"}
{"result":"pass","profile":"full","selection_mode":"selective","commit_anchored":true,"tree_dirty":false,"commit":"$parent"}
EOF
resolved=$(resolve_quiet)
[[ $resolved == "$grandparent"$'\t'* ]] ||
    die "local evidence accepted a selective record or missed the full record: $resolved"

write_gh_evidence_mock "$parent" "0000000000000000000000000000000000000000"
resolved=$(resolve_quiet)
[[ $resolved == "$parent"$'\tGitHub PR head has locally-validated label' ]] ||
    die "locally-validated evidence did not select the newest ancestor: $resolved"

write_gh_evidence_mock "0000000000000000000000000000000000000000" "$parent"
resolved=$(resolve_quiet)
[[ $resolved == "$parent"$'\tGitHub-managed portable CI succeeded at exact SHA' ]] ||
    die "GitHub CI evidence did not select the newest ancestor: $resolved"

: >"$VALIDATION_LEDGER_FILE"
write_gh_failure_mock
resolved=$(resolve_quiet)
[[ -z $resolved ]] || die "missing evidence did not fail open to full: $resolved"

SELECTIVE_BASELINE=$parent
resolved=$(resolve_quiet)
[[ $resolved == "$parent"$'\texplicit --baseline (caller asserts green)' ]] ||
    die "explicit ancestor baseline was not honored: $resolved"
SELECTIVE_BASELINE=$head
resolved=$(resolve_quiet)
[[ -z $resolved ]] || die "HEAD was incorrectly accepted as its own baseline: $resolved"
unset SELECTIVE_BASELINE

selection_json='{"decision":"skip","nodes":[],"shards":[],"cell_matrix":{"include":[]},"reasons":["all changed files are CI-irrelevant -> skip CI"]}'
coverage=$(print_selection_coverage "$parent" "test evidence" "$selection_json")
expected_nodes=$(jq '.steps | length' "$ROOT_DIR/ci/dag/portable.json")
expected_shards=$(jq '(.debug_shards + .release_shards) | length' "$ROOT_DIR/ci/portable-shards.json")
expected_cells=$(jq '.cells | length' "$ROOT_DIR/ci/expected-e2e-plan.json")
grep -Fq 'Selector reasons:' <<<"$coverage" || die "coverage report omitted selector reasons"
grep -Fq "Skipped portable DAG nodes: $expected_nodes/$expected_nodes" <<<"$coverage" ||
    die "coverage report did not enumerate all skipped DAG nodes"
grep -Fq "Skipped portable shards: $expected_shards/$expected_shards" <<<"$coverage" ||
    die "coverage report did not enumerate all skipped shards"
grep -Fq "Skipped portable E2E cells: $expected_cells/$expected_cells" <<<"$coverage" ||
    die "coverage report did not enumerate all skipped E2E cells"
grep -Fq 'Selection decision: skip. Privileged coverage is independent and never pruned.' \
    <<<"$coverage" || die "coverage report omitted privileged-lane contract"
if print_selection_coverage "$parent" "test evidence" 'not-json' \
    >/dev/null 2>&1; then
    die "malformed selection JSON did not invalidate the mandatory coverage report"
fi

function run_full_suite { printf 'full\n'; }
function run_smart_full_suite { printf 'smart:%s\n' "${1:-}"; }
function smart_selection_chain_depth_forces_full { return 1; }
VALIDATION_COMMIT_ANCHORED=1
FORCE_FULL=0
SHALLOW_SELECT=0
[[ $(run_full_dispatch) == smart: ]] || die "plain full validation is not smart by default"
FORCE_FULL=1
[[ $(run_full_dispatch) == *full ]] || die "--all path did not force the complete suite"
FORCE_FULL=0
VALIDATION_COMMIT_ANCHORED=0
[[ $(run_full_dispatch) == *full ]] || die "unanchored run did not force the complete suite"
VALIDATION_COMMIT_ANCHORED=1
SHALLOW_SELECT=1
shallow=$(run_full_dispatch)
grep -Fq "smart:$parent" <<<"$shallow" || die "shallow mode did not use HEAD~1"

grep -Fq '[[ $VALIDATION_PROFILE == full ]]' "$VALIDATE" ||
    die "exact-SHA full cache lookup is not independent of smart selection intent"

printf 'PASS: validate smart-selection evidence, fail-open, coverage, dispatch, and cache contracts\n'
