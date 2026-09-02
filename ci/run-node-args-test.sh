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
RUN_DAG="$ROOT_DIR/ci/run-dag.sh"
LANE=portable
DAG="$ROOT_DIR/ci/dag/validate.json"
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
# RUN_NODE_PRINT_ONLY asks the constructed-plan path to print the selected plan,
# so this still executes no node.
long_sel=$(python3 -c '
import json
print(",".join(json.load(open("ci/portable-shards.json"))["preflight_nodes"]))')
if [[ -z $long_sel ]]; then
    fail "could not read preflight_nodes from ci/portable-shards.json"
else
    long_output=$(run_local env RUN_NODE_PRINT_ONLY=1 \
        VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS=1 \
        "$RUN_NODE" "$LANE" "$long_sel" 2>&1)
    long_status=$?
    if ((long_status != 0)); then
        fail "the real ${#long_sel}-byte preflight selection was refused: exit $long_status.
  This is the selection ci-portable.yml passes, so a failure here reddens the whole job.
  Output: $long_output"
    elif [[ $long_output != *"scheduler-width=validate-default"* ]]; then
        fail "the constructed-plan path did not inherit validate's scheduler width. Output: $long_output"
    else
        printf 'run-node-args-test: ok — a %d-byte CI selection reaches the constructed plan\n' \
            "${#long_sel}"
    fi
fi

override_output=$(run_local env RUN_NODE_PRINT_ONLY=1 RUN_NODE_JOBS=3 \
    VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS=1 \
    "$RUN_NODE" "$LANE" "$NODE" 2>&1)
override_status=$?
if ((override_status != 0)); then
    fail "the explicit scheduler-width override was refused: exit $override_status. Output: $override_output"
elif [[ $override_output != *"scheduler-width=-j3"* ]]; then
    fail "the explicit scheduler-width override was not forwarded. Output: $override_output"
else
    printf 'run-node-args-test: ok — RUN_NODE_JOBS explicitly overrides validate default\n'
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

# The edited-command path bypasses validate only after writing the scratch DAG.
# Record the runner argv so this test catches use of a removed dagrun selector,
# rather than proving only that the JSON edit itself happened.
fake_dir=$(mktemp -d)
trap 'rm -rf -- "$fake_dir"' EXIT
fake_runner="$fake_dir/dagrun"
runner_argv="$fake_dir/argv"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$@" >"$RUN_NODE_TEST_ARGV"\n' >"$fake_runner"
chmod +x "$fake_runner"
run_local env DAGRUN_BIN="$fake_runner" RUN_NODE_TEST_ARGV="$runner_argv" \
    "$RUN_NODE" "$LANE" "$NODE" -- -E 'test(=a::b)' >/dev/null 2>&1
status=$?
if ((status != 0)); then
    fail "edited-command runner-argv probe exited $status"
elif ! grep -Fxq -- "--selected" "$runner_argv" \
    || ! grep -Fxq -- "$NODE" "$runner_argv" \
    || ! grep -Fxq -- "--ignore-selected-deps" "$runner_argv"; then
    fail "edited-command runner argv did not use --selected with explicit dependency omission"
elif grep -Fxq -- "--only" "$runner_argv"; then
    fail "edited-command runner argv still used removed dagrun --only"
else
    printf 'run-node-args-test: ok — edited command uses --selected with explicit dependency omission\n'
fi

run_local env DAGRUN_BIN="$fake_runner" RUN_NODE_TEST_ARGV="$runner_argv" \
    "$RUN_DAG" portable -q >/dev/null 2>&1
status=$?
if ((status != 0)) || ! grep -Fxq -- "--labels" "$runner_argv" \
    || ! grep -Fxq -- "portable" "$runner_argv"; then
    fail "run-dag did not pass the portable label to dagrun"
else
    printf 'run-node-args-test: ok — run-dag passes the requested label to dagrun\n'
fi

inspection_output=$(run_local env DAGRUN_BIN="$fake_runner" \
    "$RUN_DAG" portable ascii 2>&1)
status=$?
if ((status != 2)) || [[ $inspection_output != *"cannot represent the 'portable' label selection"* ]]; then
    fail "run-dag did not refuse an unfiltered labelled inspection"
else
    printf 'run-node-args-test: ok — unfiltered labelled inspection is refused\n'
fi

# ...and nothing else in the DAG moved. CPU budgets are committed on every step;
# this local iteration path is allowed to edit exactly one command and no policy.

scratch="$ROOT_DIR/ignored/ci/run-node/$LANE.$NODE.effective.json"
if [[ ! -f $scratch ]]; then
    fail "scratch DAG was not written: $scratch"
else
    python3 -c '
import json, sys

source, scratch, tag = sys.argv[1], sys.argv[2], sys.argv[3]

def steps(path):
    return {
        "{}.{}".format(s.get("group", ""), s.get("job", "")): s
        for s in json.load(open(path))["steps"]
    }

before, after = steps(source), steps(scratch)
if before.keys() != after.keys():
    sys.exit("scratch DAG changed the node set: {} vs {}".format(
        sorted(before), sorted(after)))

cmd_drift, unexplained = [], []
for key, was in before.items():
    now = after[key]
    if was == now:
        continue
    if now != was:
        if now.get("cmd") != was.get("cmd") and {
            k: v for k, v in now.items() if k != "cmd"
        } == {k: v for k, v in was.items() if k != "cmd"}:
            cmd_drift.append(key)
        else:
            unexplained.append(key)

if unexplained:
    sys.exit("steps changed in ways neither edit explains: {}".format(sorted(unexplained)))
if cmd_drift != [tag]:
    sys.exit("expected exactly {} to take an edited command, got {}".format(
        tag, sorted(cmd_drift)))
missing_cpu = [k for k, s in before.items() if not s.get("cpu_timeout")]
if missing_cpu:
    sys.exit("committed DAG has steps without cpu_timeout: {}".format(missing_cpu))
' "$DAG" "$scratch" "$NODE" || fail "scratch DAG differs from the tracked DAG in an unexplained way"
    printf 'run-node-args-test: ok — one edited command, no resource policy changed\n'
fi

if ((failures > 0)); then
    printf 'run-node-args-test: %d check(s) FAILED\n' "$failures" >&2
    exit 1
fi
printf 'run-node-args-test: OK — 5 reasoned refusals, 1 usage check, 1 real CI selection, scheduler-width default and override, 1 single-node edit, label forwarding/refusal, no resource-policy drift\n'
