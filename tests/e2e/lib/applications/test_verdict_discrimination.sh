#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Bracket `run_hermit_verify`'s verdict reading from BOTH sides.
#
# The bug this guards against is not "verification fails"; it is verification
# reporting PASS for work it never did. The previous helper decided success by
# grepping stderr for a banner while running bare `--verify` (the lossy Stripped
# policy), so a stripped match, and anything that merely printed the banner,
# read as strict L2.
#
# These cases drive the reader with SYNTHETIC `--verify-json` reports via a fake
# Hermit, so each outcome is exercised deterministically and without needing a
# real guest. The positive case is what keeps the rest honest: a reader that
# rejected everything would satisfy every negative below and be useless.

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/hermit-verdict-test.XXXXXX")
trap 'rm -rf -- "$WORK"' EXIT

failures=0

# The producer is synthetic; the report reader is the real current binary.
export VERIFICATION_REPORT_BIN=${VERIFICATION_REPORT_BIN:-"$HERE/../../../../target/debug/verification-report"}
if [[ ! -x $VERIFICATION_REPORT_BIN ]]; then
    printf 'Build the current reader with cargo build -p hermit --bin verification-report, or set VERIFICATION_REPORT_BIN\n' >&2
    exit 1
fi

# A fake Hermit that writes $FAKE_REPORT (if set) to the --verify-json path and
# exits $FAKE_STATUS. Nothing here depends on a real guest or a real comparison.
cat >"$WORK/fake-hermit" <<'FAKE'
#!/usr/bin/env bash
set -uo pipefail
verdict_path=""
prev=""
for arg in "$@"; do
    [[ $prev == --verify-json ]] && verdict_path=$arg
    prev=$arg
done
if [[ -n ${FAKE_REPORT:-} && -n $verdict_path ]]; then
    printf '%s' "$FAKE_REPORT" >"$verdict_path"
fi
if [[ ${FAKE_EMPTY_REPORT:-0} == 1 && -n $verdict_path ]]; then
    : >"$verdict_path"
fi
if [[ -n ${FAKE_ARGS_FILE:-} ]]; then
    printf '%s\n' "$@" >"$FAKE_ARGS_FILE"
fi
printf 'fake-guest-stdout\n'
printf 'Success: deterministic. Determinism verified.\n' >&2
exit "${FAKE_STATUS:-0}"
FAKE
chmod +x "$WORK/fake-hermit"

# `common.sh` resolves HERMIT_BIN at source time, so it is exported first.
export HERMIT_BIN="$WORK/fake-hermit"
export HERMIT_APPLICATION_TIMEOUT=30
# shellcheck source=/dev/null
source "$HERE/common.sh"

function expect {
    local name=$1 want=$2 report=$3 fake_status=$4
    shift 4
    local -a invocation=(--require-absolute-arg 1 -- /bin/true)
    if (($#)); then
        invocation=("$@")
    fi
    local out rc=0
    # Exported, not prefixed: `VAR=x out=$(cmd)` is an assignment statement, so
    # the prefix never reaches the command substitution's environment.
    export FAKE_REPORT="$report" FAKE_STATUS="$fake_status"
    out=$(cd "${EXPECT_CWD:-$PWD}" && \
        run_hermit_verify "$name" "${invocation[@]}" 2>&1) || rc=$?
    unset FAKE_REPORT FAKE_STATUS

    if [[ $want == STATUS23 ]]; then
        if ((rc == 23)) && [[ $out == *'strict L2 parity held, but the guest exited nonzero (status 23)'* ]]; then
            printf '  ok   %-28s valid evidence before guest status 23\n' "$name"
        else
            printf '  FAIL %-28s expected typed parity and status 23, got rc=%s:\n%s\n' "$name" "$rc" "$out"
            failures=$((failures + 1))
        fi
        return
    fi

    if [[ $want == PASS ]]; then
        if ((rc == 0)); then
            printf '  ok   %-28s PASS as expected\n' "$name"
        else
            printf '  FAIL %-28s expected PASS, got rc=%s:\n%s\n' "$name" "$rc" "$out"
            failures=$((failures + 1))
        fi
        return
    fi

    if ((rc == 0 || rc == 23)); then
        printf '  FAIL %-28s expected refusal (%s) but it PASSED\n' "$name" "$want"
        failures=$((failures + 1))
    elif [[ $out != *"$want"* ]]; then
        printf '  FAIL %-28s expected reason %s, got:\n%s\n' "$name" "$want" "$out"
        failures=$((failures + 1))
    else
        printf '  ok   %-28s refused: %s\n' "$name" "$want"
    fi
}

# Complete synthetic current producer report, including both output operands.
# The retained historical reports below intentionally remain unchanged.
parity_report=$(python3 - <<'PY'
import json
empty = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
output = dict(exit_code=0, signal=None, stdout_sha256=empty, stdout_bytes=0,
              stderr_sha256=empty, stderr_bytes=0)
print(json.dumps(dict(
    verified=True, bitwise_parity=True, verdict='matched',
    infrastructure_error=None, no_result_reason=None,
    comparison=dict(strictness='canonical', display_name='BitwiseInfoV1',
        compare_logs=True, compare_io_buffers=True, log_scope='info',
        record_envelope='all_records_v1', virtualize_time=True, strip_lines=False,
        canonicalize_addresses=True, full_trace=True, exact_remainder=True,
        stripped_prefixes=['real-wall-clock-prefix/v1'],
        canonicalizations=['host-address-to-first-appearance-ordinal/v1'],
        ignore_lines=False, skip_commit=False, skip_detlog=False),
    compared_log_messages=dict(left=1200, right=1200),
    compared_outputs=dict(left=output, right=output),
    guest_exit_code=0, guest_signal=None,
    first_divergent_scheduler_turn=None, first_divergent_virtual_nanoseconds=None,
    first_divergent_record=None, first_divergent_syscall=None,
    first_divergent_left_message=None, first_divergent_right_message=None)))
PY
)

printf 'run_hermit_verify verdict discrimination\n'

# POSITIVE. Without this the negatives prove nothing: a reader that always
# refused would pass every one of them.
expect strict-parity PASS "$parity_report" 0

status23_report=$(python3 -c 'import json,sys; r=json.loads(sys.argv[1]); r["guest_exit_code"]=23; r["compared_outputs"]["left"]["exit_code"]=23; r["compared_outputs"]["right"]["exit_code"]=23; print(json.dumps(r))' "$parity_report")
expect valid-guest-status23 STATUS23 "$status23_report" 23

for mutation in strictness compare_logs verified missing_field unequal_counts; do
    report=$(python3 - "$parity_report" "$mutation" <<'PY'
import json,sys
r=json.loads(sys.argv[1])
if sys.argv[2] == 'strictness': r['comparison']['strictness']='stripped'
elif sys.argv[2] == 'compare_logs': r['comparison']['compare_logs']=False
elif sys.argv[2] == 'verified': r['verified']=False
elif sys.argv[2] == 'missing_field': del r['guest_signal']
elif sys.argv[2] == 'unequal_counts': r['compared_log_messages']['right']=1199
else: raise AssertionError(sys.argv[2])
print(json.dumps(r))
PY
)
    expect "contradictory-$mutation" REFUSED "$report" 23
done

export FAKE_EMPTY_REPORT=1
expect empty-report REFUSED "$parity_report" 0
unset FAKE_EMPTY_REPORT

expect infrastructure-error 'INFRASTRUCTURE ERROR: kind=skid_overshoot count=2' \
    '{"verified":false,"bitwise_parity":false,"verdict":"infrastructure_error","infrastructure_error":{"kind":"skid_overshoot","count":2},"comparison":null,"compared_log_messages":null}' 1

export HERMIT_E2E_EMPTY_WORKDIR=/test FAKE_ARGS_FILE="$WORK/pinned-root-args"
expect pinned-root-workdir PASS "$parity_report" 0
grep -Fx -- '--base-env=minimal' "$FAKE_ARGS_FILE" >/dev/null
grep -Fx -- '--mount=type=tmpfs,target=/test' "$FAKE_ARGS_FILE" >/dev/null
grep -Fx -- '--workdir=/test' "$FAKE_ARGS_FILE" >/dev/null
if grep -Fx -- '--workdir=/tmp' "$FAKE_ARGS_FILE" >/dev/null; then
    printf '  FAIL pinned-root-workdir retained the ordinary /tmp workdir\n'
    failures=$((failures + 1))
else
    printf '  ok   pinned-root-workdir command uses /test\n'
fi
unset HERMIT_E2E_EMPTY_WORKDIR FAKE_ARGS_FILE

export HERMIT_E2E_EMPTY_WORKDIR=/tmp
expect invalid-pinned-root-workdir PATH-CONTRACT "$parity_report" 0
unset HERMIT_E2E_EMPTY_WORKDIR

# Ordinary guest data is not a path merely because the host cwd contains an
# entry with the same spelling.
touch "$WORK/literal-token"
EXPECT_CWD=$WORK expect literal-token-is-data PASS "$parity_report" 0 \
    --require-absolute-arg 1 -- /bin/echo literal-token

# A caller-declared path position is checked lexically, so a relative path that
# does not exist yet is still refused before the guest launch.
EXPECT_CWD=$WORK expect nonexistent-relative-path PATH-CONTRACT "$parity_report" 0 \
    --require-absolute-arg 1 --require-absolute-arg 2 -- \
    /bin/cat future-relative-path

# The original defect, planted exactly. A stripped match sets verified=true and
# the fake still prints the old success banner, so the previous banner-grep
# implementation accepted this. It is NOT L2.
expect stripped-match DIVERGED \
    '{"verified":true,"bitwise_parity":false,"verdict":"matched","comparison":{"strictness":"stripped"},"compared_log_messages":{"left":1200,"right":1200},"guest_exit_code":0,"guest_signal":null}' 0

# A strict CONFIGURATION that compared nothing. Parity over an empty selection
# is vacuous; this is the "ok with zero executed tests" failure.
expect zero-compared NO-RESULT \
    '{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":{"strictness":"canonical"},"compared_log_messages":{"left":0,"right":0},"guest_exit_code":0,"guest_signal":null}' 0

# Log comparison never ran at all (output-only fallback): null, not zero.
expect null-compared NO-RESULT \
    '{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":null,"compared_log_messages":null,"guest_exit_code":0,"guest_signal":null}' 0

# Hermit's own pre-run stamp, left behind by an early abort.
expect no-result-stamp NO-RESULT \
    '{"verified":false,"bitwise_parity":false,"verdict":"no_result","comparison":null,"compared_log_messages":null,"guest_exit_code":null,"guest_signal":null}' 1

# A real comparison that failed.
expect diverged DIVERGED \
    '{"verified":false,"bitwise_parity":false,"verdict":"diverged","comparison":{"strictness":"canonical"},"compared_log_messages":{"left":1200,"right":1199},"guest_exit_code":0,"guest_signal":null}' 1

# Launch refusal: no report written at all. Distinct from a no-result report.
expect launch-refusal REFUSED '' 1

# Malformed report must not be read as anything.
expect malformed-json NO-RESULT 'not json at all' 0

printf '\n'
if ((failures != 0)); then
    printf '%s case(s) FAILED\n' "$failures" >&2
    exit 1
fi
printf 'all verdict-discrimination cases passed\n'
