#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Summarize GitHub-hosted and self-hosted CI throughput, queue time, and runtime.
# Queue time is measured from the primary job's created_at to started_at; the
# workflow run timestamp alone does not expose self-hosted runner contention.

set -euo pipefail

REPO=${CI_STATS_REPO:-rrnewton/hermit}
HOURS=${CI_STATS_HOURS:-12}
PARALLEL=${CI_STATS_PARALLEL:-8}
FORMAT=human

readonly HOSTED_WORKFLOW='CI (GitHub-hosted)'
readonly HOSTED_JOB='Regular tests (GitHub-hosted)'
readonly SELF_WORKFLOW='CI (self-hosted)'
readonly SELF_JOB='PMU and CPUID tests (self-hosted)'

usage() {
    cat <<'EOF'
Usage: scripts/ci_stats.sh [--repo OWNER/NAME] [--hours N]
                           [--parallel N] [--json]

Reports completed CI workflow counts, primary-job queue/runtime percentiles,
active workflows, and self-hosted runner capacity. Completion counts use each
workflow's updated_at timestamp; queue time uses job created_at -> started_at.

Options:
  --repo OWNER/NAME  GitHub repository (default: rrnewton/hermit)
  --hours N          Reporting window in hours (default: 12)
  --parallel N       Concurrent GitHub job-detail requests (default: 8)
  --json             Emit machine-readable JSON
  -h, --help         Show this help

Environment:
  CI_STATS_REPO, CI_STATS_HOURS, CI_STATS_PARALLEL
  CI_STATS_PROXY     Command used to wrap gh (default: with-proxy; empty disables)
EOF
}

die() { printf 'ci_stats: %s\n' "$*" >&2; exit 2; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) [[ $# -ge 2 ]] || die '--repo requires a value'; REPO=$2; shift 2 ;;
        --hours) [[ $# -ge 2 ]] || die '--hours requires a value'; HOURS=$2; shift 2 ;;
        --parallel) [[ $# -ge 2 ]] || die '--parallel requires a value'; PARALLEL=$2; shift 2 ;;
        --json) FORMAT=json; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

[[ $HOURS =~ ^[1-9][0-9]*$ ]] || die '--hours must be a positive integer'
[[ $PARALLEL =~ ^[1-9][0-9]*$ ]] || die '--parallel must be a positive integer'
[[ $REPO == */* ]] || die '--repo must be OWNER/NAME'
command -v gh >/dev/null 2>&1 || die 'gh not found on PATH'
command -v jq >/dev/null 2>&1 || die 'jq not found on PATH'

PROXY=${CI_STATS_PROXY-with-proxy}
if [[ -n $PROXY ]] && ! command -v "$PROXY" >/dev/null 2>&1; then
    PROXY=''
fi

gh_() {
    if [[ -n $PROXY ]]; then
        "$PROXY" gh "$@"
    else
        gh "$@"
    fi
}

SINCE=$(date -u -d "$HOURS hours ago" +'%Y-%m-%dT%H:%M:%SZ')
readonly SINCE
# A self-hosted job may wait for capacity before its four-hour execution
# timeout starts. The 24-hour request buffer includes runs created before the
# report window but completed inside it.
REQUEST_SINCE=$(date -u -d "$((HOURS + 24)) hours ago" +'%Y-%m-%dT%H:%M:%SZ')
readonly REQUEST_SINCE
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/hermit-ci-stats.XXXXXX")
readonly TMP_DIR
trap 'rm -rf "$TMP_DIR"' EXIT

readonly RUN_PAGES=$TMP_DIR/run-pages.json
readonly RUNS=$TMP_DIR/runs.json
readonly ACTIVE=$TMP_DIR/active.json
readonly JOB_DIR=$TMP_DIR/jobs
readonly JOBS=$TMP_DIR/jobs.json
readonly RUNNERS=$TMP_DIR/runners.json
readonly REPORT=$TMP_DIR/report.json
mkdir -p "$JOB_DIR"

gh_ api --method GET --paginate --slurp "repos/$REPO/actions/runs" \
    -f status=completed -f "created=>=$REQUEST_SINCE" -f per_page=100 \
    >"$RUN_PAGES"

jq --arg since "$SINCE" \
    --arg hosted "$HOSTED_WORKFLOW" --arg self "$SELF_WORKFLOW" '
    [.[].workflow_runs[]
      | select((.name == $hosted or .name == $self) and .updated_at >= $since)]
    | sort_by(.updated_at)
' "$RUN_PAGES" >"$RUNS"

fetch_active() {
    local status=$1
    gh_ api --method GET --paginate --slurp "repos/$REPO/actions/runs" \
        -f status="$status" -f per_page=100
}

jq -s --arg hosted "$HOSTED_WORKFLOW" --arg self "$SELF_WORKFLOW" '
    [.[][] | .workflow_runs[]
      | select(.name == $hosted or .name == $self)]
' <(fetch_active queued) <(fetch_active in_progress) >"$ACTIVE"

export REPO PROXY JOB_DIR HOSTED_JOB SELF_JOB
fetch_one_run() {
    local run_id=$1
    local tmp=$JOB_DIR/$run_id.tmp
    local out=$JOB_DIR/$run_id.json
    if [[ -n $PROXY ]]; then
        "$PROXY" gh api --method GET "repos/$REPO/actions/runs/$run_id/jobs" \
            -f per_page=100 >"$tmp"
    else
        gh api --method GET "repos/$REPO/actions/runs/$run_id/jobs" \
            -f per_page=100 >"$tmp"
    fi
    jq --arg hosted "$HOSTED_JOB" --arg self "$SELF_JOB" '
        [.jobs[] | select(.name == $hosted or .name == $self)]
    ' "$tmp" >"$out"
    rm -f "$tmp"
}
export -f fetch_one_run

# The child bash expands $1 after xargs supplies the run ID.
# shellcheck disable=SC2016
jq -r '.[] | select(.conclusion != "skipped") | .id' "$RUNS" \
    | xargs -r -n 1 -P "$PARALLEL" bash -c 'fetch_one_run "$1"' _

job_files=("$JOB_DIR"/*.json)
if [[ -e ${job_files[0]} ]]; then
    jq -s 'add // []' "${job_files[@]}" >"$JOBS"
else
    printf '[]\n' >"$JOBS"
fi

if gh_ api --method GET --paginate --slurp "repos/$REPO/actions/runners" \
    -f per_page=100 >"$TMP_DIR/runner-pages.json" 2>"$TMP_DIR/runner-error"; then
    jq '{available: true, runners: [.[].runners[]]}' \
        "$TMP_DIR/runner-pages.json" >"$RUNNERS"
else
    jq -n --rawfile error "$TMP_DIR/runner-error" \
        '{available: false, runners: [], error: ($error | rtrimstr("\n"))}' \
        >"$RUNNERS"
fi

jq -n \
    --arg repo "$REPO" --arg since "$SINCE" --argjson hours "$HOURS" \
    --arg hosted_workflow "$HOSTED_WORKFLOW" --arg hosted_job "$HOSTED_JOB" \
    --arg self_workflow "$SELF_WORKFLOW" --arg self_job "$SELF_JOB" \
    --slurpfile runs "$RUNS" --slurpfile active "$ACTIVE" \
    --slurpfile jobs "$JOBS" --slurpfile runners "$RUNNERS" '
    def outcomes:
      reduce .[] as $item ({};
        ($item.conclusion // "unknown") as $key
        | .[$key] = ((.[$key] // 0) + 1));
    def metrics:
      sort as $values
      | ($values | length) as $n
      | {
          samples: $n,
          average_s: (if $n == 0 then null else ($values | add / $n) end),
          p50_s: (if $n == 0 then null else $values[(($n * 0.50 | ceil) - 1)] end),
          p95_s: (if $n == 0 then null else $values[(($n * 0.95 | ceil) - 1)] end),
          max_s: (if $n == 0 then null else ($values | max) end)
        };
    def lane($id; $workflow; $job; $runs; $jobs):
      ($runs | map(select(.name == $workflow))) as $lane_runs
      | ($jobs | map(select(.name == $job and .conclusion != "skipped"))) as $lane_jobs
      | ($lane_jobs | map(select((.runner_name // "") != ""))) as $executed_jobs
      | {
          id: $id,
          workflow: $workflow,
          primary_job: $job,
          completed_workflows: ($lane_runs | length),
          workflow_outcomes: ($lane_runs | outcomes),
          primary_jobs: ($lane_jobs | length),
          executed_jobs: ($executed_jobs | length),
          unassigned_jobs: (($lane_jobs | length) - ($executed_jobs | length)),
          job_outcomes: ($lane_jobs | outcomes),
          executed_outcomes: ($executed_jobs | outcomes),
          queue: ([$executed_jobs[]
                    | select(.created_at != null and .started_at != null)
                    | ((.started_at | fromdateiso8601) -
                       (.created_at | fromdateiso8601))] | metrics),
          runtime: ([$executed_jobs[]
                      | select(.started_at != null and .completed_at != null)
                      | ((.completed_at | fromdateiso8601) -
                         (.started_at | fromdateiso8601))] | metrics)
        };
    ($runs[0]) as $run_data
    | ($jobs[0]) as $job_data
    | ($active[0]) as $active_data
    | ($runners[0]) as $runner_data
    | {
        repo: $repo,
        window_hours: $hours,
        since: $since,
        completed_ci_workflows: ($run_data | length),
        lanes: [
          lane("hosted"; $hosted_workflow; $hosted_job; $run_data; $job_data),
          lane("self-hosted"; $self_workflow; $self_job; $run_data; $job_data)
        ],
        active: {
          total: ($active_data | length),
          queued: ($active_data | map(select(.status == "queued")) | length),
          in_progress: ($active_data | map(select(.status == "in_progress")) | length),
          hosted: ($active_data | map(select(.name == $hosted_workflow)) | length),
          self_hosted: ($active_data | map(select(.name == $self_workflow)) | length),
          oldest_queued_age_s: ([$active_data[]
            | select(.status == "queued" and .created_at != null)
            | (now - (.created_at | fromdateiso8601))]
            | if length == 0 then null else max end)
        },
        runners: {
          available: $runner_data.available,
          total: ($runner_data.runners | length),
          online: ($runner_data.runners | map(select(.status == "online")) | length),
          busy: ($runner_data.runners | map(select(.busy == true)) | length),
          pmu_serial: ($runner_data.runners
            | map(select(any(.labels[]?; .name == "pmu-serial")))
            | {total: length,
               online: (map(select(.status == "online")) | length),
               busy: (map(select(.busy == true)) | length)}),
          error: ($runner_data.error // null)
        }
      }
' >"$REPORT"

if [[ $FORMAT == json ]]; then
    jq . "$REPORT"
    exit 0
fi

jq -r '
    def seconds:
      if . == null then "n/a"
      elif . < 60 then "\(. | round)s"
      else "\((. / 60) | floor)m\((. % 60) | round)s"
      end;
    def count($object; $key): $object[$key] // 0;
    "CI stats: \(.repo) (last \(.window_hours)h, since \(.since))",
    "Completed CI workflow runs: \(.completed_ci_workflows)",
    "",
    "Lane         workflows  jobs/ran  success  failure  cancelled  queue p50/p95/max  runtime p50/p95",
    (.lanes[]
      | "\(.id | . + "             " | .[0:12]) "
        + "\(.completed_workflows)          "
        + "\(.primary_jobs)/\(.executed_jobs)     "
        + "\(count(.job_outcomes; "success"))        "
        + "\(count(.job_outcomes; "failure"))        "
        + "\(count(.job_outcomes; "cancelled"))          "
        + "\(.queue.p50_s | seconds)/\(.queue.p95_s | seconds)/\(.queue.max_s | seconds)  "
        + "\(.runtime.p50_s | seconds)/\(.runtime.p95_s | seconds)"),
    "",
    "Active CI workflows: \(.active.total) (queued \(.active.queued), in progress \(.active.in_progress); hosted \(.active.hosted), self-hosted \(.active.self_hosted); oldest queued \(.active.oldest_queued_age_s | seconds))",
    (if .runners.available then
       "Self-hosted runners: \(.runners.online)/\(.runners.total) online, \(.runners.busy) busy; pmu-serial \(.runners.pmu_serial.online)/\(.runners.pmu_serial.total) online, \(.runners.pmu_serial.busy) busy"
     else
       "Self-hosted runners: unavailable (\(.runners.error))"
     end)
' "$REPORT"
