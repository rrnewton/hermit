#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# run-node-args-test.sh — guard ci/run-node.sh's argument contract.
#
# ⚠️ WHY THIS EXISTS. Until 2026-08-25 run-node.sh read exactly two positional
# arguments and ignored the rest, so the natural attempt at a targeted test —
#     ci/run-node.sh portable test.detcore_unit -E 'test(=cpuid_leaf_count)'
# — ran the WHOLE node, all 534 tests, and printed PASS. Nothing anywhere said
# the filter had been dropped. The operator read a full-node green as a one-test
# green, which is a value that reads as information and carries none.
#
# The two properties below are what stop that recurring, and neither is visible
# in a passing run of anything else:
#   1. an unrecognised trailing argument is a HARD ERROR, never a silent drop;
#   2. `-- <args>` edits exactly ONE node's command and nothing else in the DAG.
#
# Runs no node and needs no build artifacts: the append case uses
# RUN_NODE_PRINT_ONLY, which stops after writing the scratch DAG.
set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR" || exit 2

RUN_NODE="$ROOT_DIR/ci/run-node.sh"
LANE=portable
DAG="$ROOT_DIR/ci/dag/$LANE.json"
# Any node with a stable tag works; this one is chosen because its command is
# short, so an assertion failure prints something readable.
NODE=check.dagrun_naming
failures=0

fail() {
    printf 'run-node-args-test: FAIL — %s\n' "$1" >&2
    failures=$((failures + 1))
}

# Refusals must not depend on the environment that only CI sets, so clear it for
# every case except the one that is specifically about CI.
run_local() {
    env -u CI -u GITHUB_ACTIONS "$@"
}

# ⚠️ EVERY REFUSAL CHECK ASSERTS ITS REASON, NOT ONLY ITS EXIT CODE. Exit 2 is
# ALSO what run-node.sh returns for an unknown lane, an unwritable perf dir, and
# "dagrun not found" — so a code-only assertion passes on a box where nothing
# works at all, which is a refusal test that cannot fail for the reason it names.
# The reason string is the only part that distinguishes the guard from the
# environment.
expect_refusal() {
    local what=$1 reason=$2
    shift 2
    local output status
    output=$(run_local "$@" 2>&1)
    status=$?
    if ((status != 2)); then
        fail "$what: expected exit 2, got $status. Output: $output"
        return
    fi
    if [[ $output != *"$reason"* ]]; then
        fail "$what: exited 2 but for the wrong reason — no '$reason' in the output.
  This is what an environment failure (missing dagrun, bad lane) looks like.
  Output: $output"
        return
    fi
    printf 'run-node-args-test: ok — %s refused with its own reason\n' "$what"
}

expect_refusal "a trailing argument with no '--'" \
    "unexpected argument '-E'" \
    "$RUN_NODE" "$LANE" "$NODE" -E 'test(=nothing)'
expect_refusal "'--' with a multi-node selection" \
    "requires exactly one node tag" \
    "$RUN_NODE" "$LANE" "$NODE,lint.rustfmt" -- --some-flag
expect_refusal "'--' with nothing after it" \
    "'--' given with nothing after it" \
    "$RUN_NODE" "$LANE" "$NODE" --
expect_refusal "'--' under \$CI" \
    "refused in CI" \
    env CI=1 "$RUN_NODE" "$LANE" "$NODE" -- --some-flag
expect_refusal "'--' under \$GITHUB_ACTIONS" \
    "refused in CI" \
    env GITHUB_ACTIONS=true "$RUN_NODE" "$LANE" "$NODE" -- --some-flag

# Positive control for the whole file: the trailing-argument refusal must also
# still print usage, so the operator is told the supported form rather than only
# that they were wrong.
usage_output=$(run_local "$RUN_NODE" "$LANE" "$NODE" -E 'test(=nothing)' 2>&1)
if [[ $usage_output != *"usage: ci/run-node.sh <lane> <group.job>"* ]]; then
    fail "the trailing-argument refusal did not print usage. Output: $usage_output"
else
    printf 'run-node-args-test: ok — the refusal prints the supported form\n'
fi

# ⚠️ A REAL CI SELECTION MUST SURVIVE THE SCRATCH-DAG WRITE.
#
# Every check above passes a SINGLE node tag, but ci-portable.yml invokes this
# script with a comma-joined MULTI-NODE selection — `preflight_nodes` is 11 tags
# and 251 bytes. When the selection went into the scratch filename, that
# overflowed NAME_MAX (255) and run-node.sh exited 2 with `File name too long`
# for the entire preflight job, while every single-node case here stayed green.
# A guard file whose cases are all one node cannot see that, so the real
# selection is read from ci/portable-shards.json rather than written down here.
#
# The runner is stubbed to /bin/true, so this still executes NO node: the scratch
# write happens before find_runner, which is exactly the part under test.
long_sel=$(python3 -c '
import json
print(",".join(json.load(open("ci/portable-shards.json"))["preflight_nodes"]))')
if [[ -z $long_sel ]]; then
    fail "could not read preflight_nodes from ci/portable-shards.json"
else
    long_output=$(run_local env DAGRUN_BIN=/bin/true "$RUN_NODE" "$LANE" "$long_sel" 2>&1)
    long_status=$?
    if ((long_status != 0)); then
        fail "the real ${#long_sel}-byte preflight selection was refused: exit $long_status.
  This is the selection ci-portable.yml passes, so a failure here reddens the whole job.
  Output: $long_output"
    else
        printf 'run-node-args-test: ok — a %d-byte CI selection writes its scratch DAG\n' \
            "${#long_sel}"
    fi
fi

# The append case. RUN_NODE_PRINT_ONLY stops before execution, so this asserts
# the edited command itself rather than a node's outcome.
tracked_cmd=$(python3 -c '
import json, sys
dag = json.load(open(sys.argv[1]))
tag = sys.argv[2]
hits = [s for s in dag["steps"]
        if "{}.{}".format(s.get("group", ""), s.get("job", "")) == tag]
if len(hits) != 1:
    sys.exit("expected exactly one step tagged {}, found {}".format(tag, len(hits)))
print(hits[0]["cmd"])
' "$DAG" "$NODE") || {
    fail "could not read the tracked command for $NODE"
    exit 1
}

edited_cmd=$(RUN_NODE_PRINT_ONLY=1 run_local "$RUN_NODE" "$LANE" "$NODE" -- -E 'test(=a::b)' 2>/dev/null)
status=$?
if ((status != 0)); then
    fail "RUN_NODE_PRINT_ONLY append run exited $status"
elif [[ $edited_cmd != "$tracked_cmd"* ]]; then
    fail "edited command does not extend the tracked one.
  tracked: $tracked_cmd
  edited:  $edited_cmd"
elif [[ $edited_cmd == "$tracked_cmd" ]]; then
    fail "'--' appended nothing; the arguments were dropped. cmd: $edited_cmd"
else
    # Shell-quoted, so the parenthesis and '=' survive as literals rather than
    # being re-parsed by the shell that eventually runs the node.
    suffix=${edited_cmd#"$tracked_cmd"}
    expected=$(printf ' %q' -E 'test(=a::b)')
    if [[ $suffix != "$expected" ]]; then
        fail "appended text is not shell-quoted as expected.
  expected: $expected
  actual:   $suffix"
    else
        printf 'run-node-args-test: ok — one node command extended by %s\n' "$suffix"
    fi
fi

# ...and nothing else in the DAG moved. The scratch DAG carries TWO edits and the
# guard has to tell them apart: the lane CPU budget stamped onto every step that
# declared none, and the appended command on exactly one step. "Some steps
# changed" would pass for either edit going wrong, so each is checked by name.
lane_budget=$(sed -n 's/^const LANE_DEFAULT_CPU_TIMEOUT_S: i64 = \([0-9]\{1,\}\);$/\1/p' \
    "$ROOT_DIR/scripts/lib/validate_plan.rs")
if [[ ! $lane_budget =~ ^[0-9]+$ ]]; then
    fail "could not read LANE_DEFAULT_CPU_TIMEOUT_S from scripts/lib/validate_plan.rs"
fi

scratch="$ROOT_DIR/ignored/ci/run-node/$LANE.$NODE.effective.json"
if [[ ! -f $scratch ]]; then
    fail "scratch DAG was not written: $scratch"
else
    python3 -c '
import json, sys

source, scratch, tag, budget = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])

def steps(path):
    return {
        "{}.{}".format(s.get("group", ""), s.get("job", "")): s
        for s in json.load(open(path))["steps"]
    }

before, after = steps(source), steps(scratch)
if before.keys() != after.keys():
    sys.exit("scratch DAG changed the node set: {} vs {}".format(
        sorted(before), sorted(after)))

cmd_drift, budget_drift, unexplained = [], [], []
for key, was in before.items():
    now = after[key]
    if was == now:
        continue
    probe = dict(now)
    # A step that declared its own cpu_timeout must keep it untouched; only an
    # undeclared one may take the lane default.
    if not was.get("cpu_timeout") and probe.get("cpu_timeout") == budget:
        del probe["cpu_timeout"]
        stripped = {k: v for k, v in was.items() if k != "cpu_timeout"}
        budget_drift.append(key)
    else:
        stripped = was
    if probe != stripped:
        if probe.get("cmd") != stripped.get("cmd") and {
            k: v for k, v in probe.items() if k != "cmd"
        } == {k: v for k, v in stripped.items() if k != "cmd"}:
            cmd_drift.append(key)
        else:
            unexplained.append(key)

if unexplained:
    sys.exit("steps changed in ways neither edit explains: {}".format(sorted(unexplained)))
if cmd_drift != [tag]:
    sys.exit("expected exactly {} to take an edited command, got {}".format(
        tag, sorted(cmd_drift)))
undeclared = [k for k, s in before.items() if not s.get("cpu_timeout")]
if sorted(budget_drift) != sorted(undeclared):
    sys.exit("lane CPU budget not carried onto every undeclared step: stamped {} of {}".format(
        len(budget_drift), len(undeclared)))
declared = [k for k, s in before.items() if s.get("cpu_timeout")]
for key in declared:
    if before[key].get("cpu_timeout") != after[key].get("cpu_timeout"):
        sys.exit("a step that DECLARED its own cpu_timeout was overwritten: {}".format(key))
print("  {} undeclared step(s) stamped {}s; {} declared step(s) left alone".format(
    len(budget_drift), budget, len(declared)))
' "$DAG" "$scratch" "$NODE" "$lane_budget" || fail "scratch DAG differs from the tracked DAG in an unexplained way"
    printf 'run-node-args-test: ok — one edited command, lane budget carried, nothing else moved\n'
fi

if ((failures > 0)); then
    printf 'run-node-args-test: %d check(s) FAILED\n' "$failures" >&2
    exit 1
fi
printf 'run-node-args-test: OK — 5 reasoned refusals, 1 usage check, 1 real CI selection, 1 single-node edit, lane CPU budget carried, no unexplained drift\n'
