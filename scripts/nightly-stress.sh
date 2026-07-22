#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Nightly chaos stress harness.
#
# Runs the concurrency and CAS stress guests under Hermit's chaos scheduler with
# many random seeds and emits a machine-readable JSON report (plus a short
# Markdown summary) for trend tracking. It is intended to be invoked from the
# `nightly.yml` scheduled workflow, but is a standalone script: it builds the
# guest binaries with `rustc` and drives the release `hermit` binary directly,
# using the same chaos flags as `hermit-cli/tests/stress_suite.rs`.
#
# What it checks
# --------------
#   1. Chaos matrix: every stress category is run at several thread counts with
#      N random chaos seeds. Race categories are *expected* to be exposed by
#      chaos; correctness categories must *never* be exposed. A correctness
#      category that fails, or any run that errors/times out, is a regression.
#   2. Record/replay determinism: a CAS race is searched under chaos with
#      preemptions recorded, then the recorded schedule is replayed. The replay
#      must reproduce the recorded outcome. A non-reproduction is a determinism
#      failure. This step needs PMU access and is skipped (not failed) without
#      it.
#
# Every seed used is recorded in the JSON so any failure is reproducible.
#
# Configuration (environment variables; all optional):
#   HERMIT_BIN            Path to the hermit binary (default: target/release/hermit,
#                         falling back to target/debug/hermit).
#   NIGHTLY_SEEDS         Random chaos seeds per (category, thread-count) cell
#                         (default: 10).
#   NIGHTLY_THREADS       Space-separated thread counts (default: "2 4 8 16").
#   NIGHTLY_CATEGORIES    Space-separated categories (default: all known).
#   NIGHTLY_CAS_SEEDS     Seeds to search for a CAS race to replay (default: 60).
#   NIGHTLY_RUN_TIMEOUT   Per-run wall-clock timeout, seconds (default: 15).
#   NIGHTLY_OUT_DIR       Output directory (default: target/nightly-stress).
#   NIGHTLY_MASTER_SEED   Seed for the seed generator, for a reproducible run
#                         (default: derived from the shell $RANDOM).
#   NIGHTLY_SKIP_RECORD_REPLAY   If set to 1, skip the record/replay step.
#
# Exit status: 0 if no regressions or determinism failures were found; 1
# otherwise. Environmental skips (e.g. no PMU) are reported, not failed.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT_DIR
cd "$ROOT_DIR" || exit 1

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
default_hermit=""
if [[ -x "$ROOT_DIR/target/release/hermit" ]]; then
    default_hermit="$ROOT_DIR/target/release/hermit"
elif [[ -x "$ROOT_DIR/target/debug/hermit" ]]; then
    default_hermit="$ROOT_DIR/target/debug/hermit"
fi
HERMIT_BIN="${HERMIT_BIN:-$default_hermit}"
NIGHTLY_SEEDS="${NIGHTLY_SEEDS:-10}"
NIGHTLY_THREADS="${NIGHTLY_THREADS:-2 4 8 16}"
NIGHTLY_CATEGORIES="${NIGHTLY_CATEGORIES:-atomic-lost-update publish-ordering producer-consumer missing-barrier condvar-lost-wakeup mutex-correctness rwlock-fairness store-buffer}"
NIGHTLY_CAS_SEEDS="${NIGHTLY_CAS_SEEDS:-60}"
NIGHTLY_RUN_TIMEOUT="${NIGHTLY_RUN_TIMEOUT:-15}"
NIGHTLY_OUT_DIR="${NIGHTLY_OUT_DIR:-$ROOT_DIR/target/nightly-stress}"
NIGHTLY_SKIP_RECORD_REPLAY="${NIGHTLY_SKIP_RECORD_REPLAY:-0}"

# Categories that must NEVER be exposed by chaos; exposure is a real bug.
readonly CORRECTNESS_CATEGORIES=" mutex-correctness rwlock-fairness store-buffer "

if ! command -v jq >/dev/null 2>&1; then
    echo "::error::jq is required by nightly-stress.sh" >&2
    exit 2
fi
if [[ -z "$HERMIT_BIN" || ! -x "$HERMIT_BIN" ]]; then
    echo "::error::hermit binary not found (set HERMIT_BIN or build the workspace)" >&2
    exit 2
fi

mkdir -p "$NIGHTLY_OUT_DIR" "$NIGHTLY_OUT_DIR/bin" "$NIGHTLY_OUT_DIR/schedules"
readonly JSON_OUT="$NIGHTLY_OUT_DIR/results.json"
readonly MD_OUT="$NIGHTLY_OUT_DIR/summary.md"

# Deterministic-ish seed generator so a run can be reproduced with
# NIGHTLY_MASTER_SEED. We avoid the guest's own RNG; these are just the chaos
# seeds handed to hermit.
MASTER_SEED="${NIGHTLY_MASTER_SEED:-$RANDOM$RANDOM}"
_seed_state="$MASTER_SEED"
next_seed() {
    # xorshift-ish LCG on a 31-bit space; deterministic given MASTER_SEED.
    _seed_state=$(( (_seed_state * 1103515245 + 12345) & 0x7fffffff ))
    printf '%s' "$(( _seed_state % 65536 ))"
}

git_sha="$(git -C "$ROOT_DIR" rev-parse HEAD 2>/dev/null || echo unknown)"
generated_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo ":: nightly-stress @ ${git_sha}"
echo ":: hermit=${HERMIT_BIN} seeds/cell=${NIGHTLY_SEEDS} threads=[${NIGHTLY_THREADS}] master_seed=${MASTER_SEED}"

# ---------------------------------------------------------------------------
# Build guest binaries (mirrors stress_suite.rs compilation)
# ---------------------------------------------------------------------------
CONCURRENCY_BIN="$NIGHTLY_OUT_DIR/bin/concurrency"
CAS_BIN="$NIGHTLY_OUT_DIR/bin/cas-sequence"
compile_guest() {
    local src="$1" out="$2"
    if ! rustc --edition=2024 -C opt-level=2 -C debuginfo=1 "$src" -o "$out"; then
        echo "::error::failed to compile guest $src" >&2
        exit 2
    fi
}
compile_guest "$ROOT_DIR/tests/stress/concurrency.rs" "$CONCURRENCY_BIN"
compile_guest "$ROOT_DIR/flaky-tests/cas_sequence_easy.rs" "$CAS_BIN"

# ---------------------------------------------------------------------------
# Run helpers. Classify a hermit run: clean|exposed|timeout|error.
# 0 -> clean, 1 -> exposed (race found), 124 -> timeout, else -> error.
# ---------------------------------------------------------------------------
classify_rc() {
    case "$1" in
        0) echo clean ;;
        1) echo exposed ;;
        124) echo timeout ;;
        *) echo error ;;
    esac
}

is_correctness_category() {
    [[ "$CORRECTNESS_CATEGORIES" == *" $1 "* ]]
}

# Accumulate scenario objects here (one compact JSON object per line).
SCEN_FILE="$(mktemp "${TMPDIR:-/tmp}/nightly-scenarios.XXXXXX")"
RR_FILE="$(mktemp "${TMPDIR:-/tmp}/nightly-rr.XXXXXX")"
trap 'rm -f "$SCEN_FILE" "$RR_FILE"' EXIT

overall_status=0
regressions=()

# ---------------------------------------------------------------------------
# 1. Chaos matrix
# ---------------------------------------------------------------------------
run_chaos_once() {
    # args: category threads seed ; prints classification
    local category="$1" threads="$2" seed="$3" rc
    timeout "${NIGHTLY_RUN_TIMEOUT}s" "$HERMIT_BIN" run \
        --base-env=minimal --chaos --sched-heuristic=random \
        --preemption-timeout=disabled --no-virtualize-cpuid \
        "--seed=${seed}" "$CONCURRENCY_BIN" "$category" "$threads" \
        >/dev/null 2>&1
    rc=$?
    classify_rc "$rc"
}

for category in $NIGHTLY_CATEGORIES; do
    for threads in $NIGHTLY_THREADS; do
        clean=0 exposed=0 timeout=0 error=0
        exposed_seeds=() error_seeds=() timeout_seeds=()
        for ((i = 0; i < NIGHTLY_SEEDS; i++)); do
            seed="$(next_seed)"
            outcome="$(run_chaos_once "$category" "$threads" "$seed")"
            case "$outcome" in
                clean) ((clean++)) ;;
                exposed) ((exposed++)); exposed_seeds+=("$seed") ;;
                timeout) ((timeout++)); timeout_seeds+=("$seed") ;;
                error) ((error++)); error_seeds+=("$seed") ;;
            esac
        done

        # Decide whether this cell is a regression.
        unexpected=false
        reason=""
        if is_correctness_category "$category"; then
            if ((exposed > 0)); then
                unexpected=true
                reason="correctness category exposed (real bug)"
            fi
        fi
        if ((error > 0)); then
            unexpected=true
            reason="${reason:+$reason; }unexpected error exit"
        fi
        if ((timeout > 0)); then
            unexpected=true
            reason="${reason:+$reason; }run timed out (possible deadlock/hang)"
        fi
        if [[ "$unexpected" == true ]]; then
            overall_status=1
            regressions+=("chaos ${category} threads=${threads}: ${reason}")
        fi

        # Build the JSON object for this cell with jq (safe numeric/string args).
        jq -cn \
            --arg category "$category" \
            --argjson threads "$threads" \
            --argjson clean "$clean" \
            --argjson exposed "$exposed" \
            --argjson timeout "$timeout" \
            --argjson error "$error" \
            --argjson seeds "$NIGHTLY_SEEDS" \
            --argjson unexpected "$unexpected" \
            --arg reason "$reason" \
            --arg exposed_seeds "${exposed_seeds[*]:-}" \
            --arg error_seeds "${error_seeds[*]:-}" \
            --arg timeout_seeds "${timeout_seeds[*]:-}" \
            --argjson correctness "$(is_correctness_category "$category" && echo true || echo false)" \
            '{
              kind: "chaos", category: $category, threads: $threads,
              correctness_category: $correctness, seeds: $seeds,
              clean: $clean, exposed: $exposed, timeout: $timeout, error: $error,
              unexpected: $unexpected, reason: (if $reason=="" then null else $reason end),
              exposed_seeds: ($exposed_seeds | if .=="" then [] else (split(" ")|map(tonumber)) end),
              error_seeds: ($error_seeds | if .=="" then [] else (split(" ")|map(tonumber)) end),
              timeout_seeds: ($timeout_seeds | if .=="" then [] else (split(" ")|map(tonumber)) end)
            }' >>"$SCEN_FILE"

        printf '   chaos %-20s threads=%-2s clean=%-3s exposed=%-3s timeout=%s error=%s%s\n' \
            "$category" "$threads" "$clean" "$exposed" "$timeout" "$error" \
            "$([[ $unexpected == true ]] && echo "  <== ${reason}" || echo "")"
    done
done

# ---------------------------------------------------------------------------
# 2. Record/replay determinism (CAS)
# ---------------------------------------------------------------------------
rr_status="skipped"
rr_detail="record/replay skipped"
if [[ "$NIGHTLY_SKIP_RECORD_REPLAY" == "1" ]]; then
    rr_detail="disabled via NIGHTLY_SKIP_RECORD_REPLAY"
else
    paranoid="$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unknown)"
    echo ":: record/replay determinism check (perf_event_paranoid=${paranoid})"
    failing_seed=""
    schedule=""
    cas_exposed=0 cas_clean=0 cas_other=0
    for ((i = 0; i < NIGHTLY_CAS_SEEDS; i++)); do
        seed="$(next_seed)"
        sched="$NIGHTLY_OUT_DIR/schedules/preemptions-${seed}.json"
        timeout "${NIGHTLY_RUN_TIMEOUT}s" "$HERMIT_BIN" run \
            --base-env=minimal --chaos --imprecise-timers \
            --preemption-timeout=10000000 --no-virtualize-cpuid \
            "--seed=${seed}" "--record-preemptions-to=${sched}" "$CAS_BIN" \
            >/dev/null 2>&1
        rc=$?
        case "$(classify_rc "$rc")" in
            exposed)
                ((cas_exposed++))
                if [[ -z "$failing_seed" && -s "$sched" ]]; then
                    failing_seed="$seed"; schedule="$sched"
                fi
                ;;
            clean) ((cas_clean++)) ;;
            *) ((cas_other++)) ;;
        esac
    done

    if [[ -z "$failing_seed" ]]; then
        rr_status="inconclusive"
        rr_detail="no CAS race exposed in ${NIGHTLY_CAS_SEEDS} seeds (exposed=${cas_exposed} clean=${cas_clean} other=${cas_other}); nothing to replay"
        echo "   $rr_detail"
    else
        timeout "$((NIGHTLY_RUN_TIMEOUT * 4))s" "$HERMIT_BIN" run \
            --base-env=minimal --chaos --preemption-timeout=10000000 \
            --no-virtualize-cpuid "--seed=${failing_seed}" \
            "--replay-preemptions-from=${schedule}" "$CAS_BIN" \
            >/dev/null 2>&1
        replay_rc=$?
        if [[ "$(classify_rc "$replay_rc")" == "exposed" ]]; then
            rr_status="pass"
            rr_detail="recorded seed=${failing_seed} reproduced on replay"
            echo "   PASS: $rr_detail"
        else
            rr_status="fail"
            rr_detail="recorded seed=${failing_seed} did NOT reproduce on replay (replay rc=${replay_rc}) -- DETERMINISM FAILURE"
            overall_status=1
            regressions+=("record/replay: ${rr_detail}")
            echo "   FAIL: $rr_detail"
        fi
    fi
    jq -cn \
        --arg status "$rr_status" --arg detail "$rr_detail" \
        --arg failing_seed "${failing_seed:-}" \
        --argjson exposed "${cas_exposed:-0}" --argjson clean "${cas_clean:-0}" \
        --argjson other "${cas_other:-0}" --argjson seeds "$NIGHTLY_CAS_SEEDS" \
        '{status:$status, detail:$detail,
          failing_seed:(if $failing_seed=="" then null else ($failing_seed|tonumber) end),
          seeds:$seeds, exposed:$exposed, clean:$clean, other:$other}' >>"$RR_FILE"
fi

# ---------------------------------------------------------------------------
# Assemble the final JSON report
# ---------------------------------------------------------------------------
status_str=$([[ $overall_status -eq 0 ]] && echo pass || echo fail)
regressions_json="$(printf '%s\n' "${regressions[@]:-}" | jq -R . | jq -s 'map(select(. != ""))')"

jq -n \
    --argjson schema_version 1 \
    --arg generated_utc "$generated_utc" \
    --arg hermit_sha "$git_sha" \
    --arg host "$(uname -srm 2>/dev/null || echo unknown)" \
    --arg hostname "$(hostname 2>/dev/null || echo unknown)" \
    --argjson master_seed "$MASTER_SEED" \
    --argjson seeds_per_cell "$NIGHTLY_SEEDS" \
    --arg thread_counts "$NIGHTLY_THREADS" \
    --arg status "$status_str" \
    --argjson regressions "$regressions_json" \
    --slurpfile scenarios "$SCEN_FILE" \
    --slurpfile record_replay "$RR_FILE" \
    '{
      schema_version: $schema_version,
      generated_utc: $generated_utc,
      hermit_sha: $hermit_sha,
      host: {uname: $host, hostname: $hostname},
      config: {
        master_seed: $master_seed,
        seeds_per_cell: $seeds_per_cell,
        thread_counts: ($thread_counts | split(" ") | map(tonumber))
      },
      scenarios: $scenarios,
      record_replay: ($record_replay | if length==0 then null else .[0] end),
      summary: {
        status: $status,
        total_runs: ($scenarios | map(.clean + .exposed + .timeout + .error) | add // 0),
        unexpected_cells: ($scenarios | map(select(.unexpected)) | length),
        regressions: $regressions
      }
    }' >"$JSON_OUT"

# ---------------------------------------------------------------------------
# Markdown summary (human-friendly, for the workflow step summary)
# ---------------------------------------------------------------------------
{
    echo "# Nightly chaos stress report"
    echo
    echo "- **Status:** ${status_str}"
    echo "- **Commit:** \`${git_sha}\`"
    echo "- **Generated:** ${generated_utc}"
    echo "- **Master seed:** ${MASTER_SEED} (rerun with \`NIGHTLY_MASTER_SEED=${MASTER_SEED}\` to reproduce)"
    echo "- **Record/replay:** ${rr_status} — ${rr_detail}"
    echo
    echo "| category | threads | clean | exposed | timeout | error | note |"
    echo "|---|---|---|---|---|---|---|"
    jq -r '.scenarios[] | "| \(.category) | \(.threads) | \(.clean) | \(.exposed) | \(.timeout) | \(.error) | \(.reason // "") |"' "$JSON_OUT"
    echo
    if [[ $overall_status -ne 0 ]]; then
        echo "## Regressions"
        echo
        jq -r '.summary.regressions[] | "- " + .' "$JSON_OUT"
    else
        echo "_No regressions or determinism failures detected._"
    fi
} >"$MD_OUT"

echo
echo ":: JSON report:     $JSON_OUT"
echo ":: Markdown summary: $MD_OUT"
echo ":: overall status:  $status_str"

# Emit to the GitHub step summary when running under Actions.
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    cat "$MD_OUT" >>"$GITHUB_STEP_SUMMARY"
fi

exit "$overall_status"
