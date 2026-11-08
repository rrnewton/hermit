#!/usr/bin/env bash
# Census gate for one counted Nextest node: compare this run's listed
# population against a base (last landed main) population with ci/census-diff.
#
# Usage: ci/census-check.sh --candidate-listed FILE [--base-listed FILE]
#          [--candidate-outcomes FILE] [--dropped FILE] [--report-only]
#          [--suite NAME]
#
# With no --base-listed there is no base population in-node (the queue
# adapter owns the cached base; see HERMIT_CENSUS_BASE_LISTED_IDS in
# ci/run-nextest-counted.sh): the check reports that and passes, so wiring
# it into every counted node is safe before the adapter supplies a base.
# With a base, an unrecorded disappearance fails naming the missing IDs;
# additions pass; totals are reported, never asserted.
set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR

base_listed= candidate_listed= candidate_outcomes= dropped= suite=nextest
report_only=0
while (($#)); do
    case $1 in
        --base-listed) base_listed=$2; shift 2 ;;
        --candidate-listed) candidate_listed=$2; shift 2 ;;
        --candidate-outcomes) candidate_outcomes=$2; shift 2 ;;
        --dropped) dropped=$2; shift 2 ;;
        --suite) suite=$2; shift 2 ;;
        --report-only) report_only=1; shift ;;
        *) printf 'census-check: unknown argument %s\n' "$1" >&2; exit 2 ;;
    esac
done
if [[ -z $candidate_listed && -z $candidate_outcomes ]]; then
    printf 'census-check: --candidate-listed or --candidate-outcomes is required\n' >&2
    exit 2
fi
if [[ -z $base_listed ]]; then
    printf 'census %s: report-only (no base population supplied; queue adapter has not provided HERMIT_CENSUS_BASE_LISTED_IDS)\n' "$suite" >&2
    exit 0
fi
# A stamped first base (census_base.py, for a suite with no prior base)
# names itself with a `# census-first-base:` marker. The candidate's own
# listing becomes that first base when the landing gate records it, so
# there is no prior population to disappear from and no diff to run. A
# base without the marker keeps the full disappearance check below,
# including an existing suite whose base file is simply missing (the
# adapter supplies no file for it at all, and the counted wrapper's
# require-base refusal fires before this script runs).
if grep -q '^# census-first-base:' "$base_listed"; then
    marker=$(grep -m1 '^# census-first-base:' "$base_listed")
    count=0
    if [[ -n $candidate_listed ]]; then
        count=$(grep -cv '^#' "$candidate_listed" || true)
    fi
    printf 'census %s: first base recorded from candidate (%s; candidate ids=%s; no prior base, no disappearance check)\n' "$suite" "$marker" "$count" >&2
    exit 0
fi

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/base" "$scratch/candidate"
cp -- "$base_listed" "$scratch/base/$suite.listed-ids"
if [[ -n $candidate_outcomes ]]; then
    cp -- "$candidate_outcomes" "$scratch/candidate/$suite.outcomes.json"
elif [[ -n $candidate_listed ]]; then
    cp -- "$candidate_listed" "$scratch/candidate/$suite.listed-ids"
fi
args=(--base "$scratch/base" --candidate "$scratch/candidate")
[[ -n $dropped ]] && args+=(--dropped "$dropped")
((report_only)) && args+=(--report-only)
python3 "$SCRIPT_DIR/census-diff" "${args[@]}"
