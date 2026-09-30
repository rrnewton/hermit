#!/usr/bin/env bash
#
# Run the unit tests carried by rust-script entrypoints.
#
# rust-scripts are not Cargo workspace members, so `cargo test` never sees them.
# `build.rust_scripts` compiles all tracked entrypoints and their test harnesses
# before this graph consumer runs. This script executes those harnesses; outside
# the graph, rust-script retains its ordinary compile-and-run behavior. Measured
# 2026-08-25 at hermit main a5fef7ff7623: seven entrypoints carried 84 unit tests
# between them and no target anywhere passed `--test`, so all 84 were documentation.
# They all passed once run, so this gate starts green.
#
# The file list is DISCOVERED, not hard-coded: a new script that grows a test
# module is picked up with no edit here. A hard-coded list would reproduce exactly
# the "someone has to remember" failure this gate was added to close.
#
# ONE DISCOVERED FILE IS EXCLUDED, AND ONLY WHILE ANOTHER NODE OWNS IT.
# ci/compat-envelope/scorecard.rs carries 68 unit tests that took 211.96 s wall
# on their own on 2026-09-29, about 193 s of it in the 23
# post_verdict_transaction_tests that build scratch ledger and source
# repositories (https://github.com/rrnewton/hermit/issues/3381). They run in
# the validation DAG leaf node selftest.scorecard_tests instead of in
# check.lint_checks (`make lint-checks` runs this script). That node runs them
# when a path under its trigger paths changed since the merge base with
# origin/main, always on main, and with HERMIT_SELFTEST_SELECTION=all;
# otherwise it prints a "NOT RUN by file selection:" line.
#
# The exclusion FAILS CLOSED. This script skips scorecard.rs only when the
# committed ci/dag/validate.json has exactly one selftest.scorecard_tests node,
# that node carries every label check.lint_checks carries (so it is scheduled
# in every lane this script runs in), its command is exactly check.lint_checks's
# environment preamble (the text before its last "; ") followed by the harness
# invocation below, it sets no environment of its own, and it is not
# engine_only. Otherwise it counts scorecard.rs as a failed suite and says
# why. `--self-test` checks the committed DAG and plants each broken shape.
# ci/manifest-plan's validation_dag tests pin the other half: that
# `test-harness selftest scorecard_tests` runs
# `rust-script --test ci/compat-envelope/scorecard.rs` and that the constants
# below name that node and command.
set -euo pipefail

cd "$(dirname "$0")/.."

SCORECARD_SOURCE=ci/compat-envelope/scorecard.rs
SCORECARD_NODE=selftest.scorecard_tests
SCORECARD_NODE_COMMAND='target/debug/test-harness selftest scorecard_tests'
LINT_NODE=check.lint_checks
DAG=ci/dag/validate.json

# Checks that the DAG lets this script skip scorecard.rs. `scorecard_owner
# check DAG` exits 0 when it does and otherwise prints the reason on stderr and
# exits 1. `scorecard_owner self-test DAG` requires the check to pass on DAG,
# then plants each broken shape on copies of the two nodes it compares and
# requires a refusal naming that shape, all in one process.
scorecard_owner() {
    python3 - "$1" "$2" "$SCORECARD_NODE" "$SCORECARD_NODE_COMMAND" "$LINT_NODE" <<'PY'
import json
import os
import sys

mode, path, node, command, lint = sys.argv[1:]


class Refused(Exception):
    pass


def load(path):
    try:
        with open(path, encoding="utf-8") as handle:
            steps = json.load(handle)["steps"]
        if not isinstance(steps, list):
            raise TypeError("steps is not a list")
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise Refused(f"cannot read the steps of {path}: {error}")
    return steps


def tagged(steps, tag):
    return [step for step in steps if isinstance(step, dict)
            and f"{step.get('group')}.{step.get('job')}" == tag]


def check(steps):
    owners = tagged(steps, node)
    if len(owners) != 1:
        raise Refused(f"{path} has {len(owners)} {node} node(s), expected exactly one")
    lints = tagged(steps, lint)
    if len(lints) != 1:
        raise Refused(f"{path} has {len(lints)} {lint} node(s), expected exactly one, "
                      f"so there are no lanes to compare")
    owner = owners[0]
    lint_cmd = lints[0].get("cmd")
    if not isinstance(lint_cmd, str) or "; " not in lint_cmd:
        raise Refused(f"{lint}'s command {lint_cmd!r} has no environment preamble to compare")
    expected = lint_cmd.rsplit("; ", 1)[0] + "; " + command
    cmd = owner.get("cmd")
    if cmd != expected:
        raise Refused(f"{node}'s command must be exactly `{expected}` ({lint}'s preamble, "
                      f"then `{command}`); it is {cmd!r}")
    if owner.get("env") != {}:
        raise Refused(f"{node} must set no environment of its own; it sets {owner.get('env')!r}")
    if owner.get("engine_only") is not False:
        raise Refused(f"{node} must not be engine_only; it is {owner.get('engine_only')!r}")
    lint_labels = lints[0].get("labels") or []
    if not lint_labels:
        raise Refused(f"{lint} carries no labels, so there are no lanes to compare")
    missing = sorted(set(lint_labels) - set(owner.get("labels") or []))
    if missing:
        raise Refused(f"{node} is not scheduled in lane(s) {', '.join(missing)}, "
                      f"where {lint} runs this script")


def refusal(steps):
    try:
        check(steps)
    except Refused as error:
        return str(error)
    return None


if mode == "check":
    try:
        check(load(path))
    except Refused as error:
        print(f"run-script-tests: cannot exclude ci/compat-envelope/scorecard.rs: {error}",
              file=sys.stderr)
        sys.exit(1)
    sys.exit(0)

try:
    steps = load(path)
except Refused as error:
    sys.exit(f"FAIL: {error}")
reason = refusal(steps)
if reason is not None:
    sys.exit(f"FAIL: the committed {path} does not let scorecard.rs be excluded: {reason}")
[owner], [lint_step] = tagged(steps, node), tagged(steps, lint)
cases = [
    ("node deleted", f"has 0 {node} node(s)", [lint_step]),
    ("node duplicated", f"has 2 {node} node(s)", [owner, dict(owner), lint_step]),
    ("lint node deleted", f"has 0 {lint} node(s)", [owner]),
    ("lint labels emptied", "carries no labels", [owner, {**lint_step, "labels": []}]),
    ("lint command without a preamble", "has no environment preamble",
     [owner, {**lint_step, "cmd": "./ci/lint-checks-node.sh"}]),
    ("another self-test's command", "command must be exactly",
     [{**owner, "cmd": owner["cmd"].replace(node.split(".")[1], "scorecard_commands")},
      lint_step]),
    ("failure swallowed", "command must be exactly",
     [{**owner, "cmd": owner["cmd"] + " || true"}, lint_step]),
    ("exit before the command", "command must be exactly",
     [{**owner, "cmd": owner["cmd"].replace(command, "exit 0; " + command)}, lint_step]),
    ("environment override", "must set no environment",
     [{**owner, "env": {"HERMIT_SELFTEST_SELECTION": "none"}}, lint_step]),
    ("engine_only", "must not be engine_only", [{**owner, "engine_only": True}, lint_step]),
]
for label in lint_step["labels"]:
    cases.append((f"{label} dropped", f"not scheduled in lane(s) {label}",
                  [{**owner, "labels": [kept for kept in owner["labels"] if kept != label]},
                   lint_step]))
failures = []
for name, fragment, planted in cases:
    reason = refusal(planted)
    if reason is None:
        failures.append(f"{name}: the exclusion was allowed")
    elif fragment not in reason:
        failures.append(f"{name}: refused without {fragment!r}: {reason}")
try:
    load(os.path.join(path, "absent"))
    failures.append("an unreadable DAG file was not refused")
except Refused as error:
    if "cannot read the steps" not in str(error):
        failures.append(f"an unreadable DAG file was refused without 'cannot read the steps': {error}")
for failure in failures:
    print(f"FAIL: {failure}", file=sys.stderr)
if failures:
    sys.exit(f"run-script-tests --self-test: {len(failures)} of {len(cases) + 1} case(s) failed")
print(f"PASS: run-script-tests excludes ci/compat-envelope/scorecard.rs only while {node} runs it")
print(f"      in every lane {lint} runs in; {len(cases) + 1} planted drifts are refused")
PY
}

case "$#:${1:-}" in
    0:) ;;
    1:--self-test)
        scorecard_owner self-test "$DAG"
        exit $?
        ;;
    *)
        echo 'usage: scripts/run-script-tests.sh [--self-test]' >&2
        exit 2
        ;;
esac

if ! command -v rust-script >/dev/null 2>&1; then
    echo 'error: rust-script is not installed (cargo install rust-script)' >&2
    exit 1
fi

failed=0
total=0
excluded=0
for f in $(git ls-files '*.rs' ':!:third-party/**' ':!:scripts/lib/**'); do
    # Only standalone entrypoints: a rust-script shebang makes the file runnable
    # on its own, which is what `rust-script --test` requires.
    head -n 1 -- "$f" | grep -q 'rust-script' || continue
    grep -q '#\[cfg(test)\]' -- "$f" || continue

    if [ "$f" = "$SCORECARD_SOURCE" ]; then
        if scorecard_owner check "$DAG"; then
            excluded=$((excluded + 1))
            # shellcheck disable=SC2016 # the backquotes are literal text
            printf 'run-script-tests: NOT RUN HERE: %s unit tests. Validation DAG node %s runs them (`%s`) in every lane %s runs in, when a path under its trigger paths changed, always on main, and with HERMIT_SELFTEST_SELECTION=all. Run them directly with `./ci/rust-script-bin/run-test-harness %s -- rust-script --test %s`.\n' \
                "$f" "$SCORECARD_NODE" "$SCORECARD_NODE_COMMAND" "$LINT_NODE" "$f" "$f"
        else
            echo "run-script-tests: FAILED ${f}: its tests are neither run here nor owned by ${SCORECARD_NODE}" >&2
            total=$((total + 1))
            failed=$((failed + 1))
        fi
        continue
    fi

    total=$((total + 1))
    printf 'run-script-tests: %s\n' "$f"
    # Also cover ordinary rust-script outside the prepared validation graph.
    # The prepared dispatcher adds a nested scope of its own; both directories
    # are retained, and each loop iteration starts with the original outer root.
    if ! ./ci/rust-script-bin/run-test-harness "$f" -- rust-script --test "$f"; then
        echo "run-script-tests: FAILED ${f}" >&2
        failed=$((failed + 1))
    fi
done

if [ "$total" -eq 0 ]; then
    # Not a pass. Discovery returning nothing means the shebang or the test-module
    # spelling moved and this gate is now measuring an empty set -- the silent-green
    # shape. Fail loudly instead. The excluded scorecard suite does not count.
    echo 'error: run-script-tests discovered no test-carrying rust-scripts; expected at least one' >&2
    exit 1
fi

if [ "$failed" -ne 0 ]; then
    echo "run-script-tests: ${failed} of ${total} script test suites failed" >&2
    exit 1
fi

printf 'run-script-tests: OK -- %s script test suites passed; %s excluded (named above with the node that runs it)\n' "$total" "$excluded"
