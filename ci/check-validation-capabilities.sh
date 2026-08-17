#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

check_capabilities() {
    local pmu_overflow_probe=$1 hermit_bin=$2 cpuid_guest=$3 work=$4
    if ! timeout --kill-after=2s 20s "$pmu_overflow_probe" --iterations 4 --period 100000 \
        >"$work/pmu.stdout" 2>"$work/pmu.stderr"; then
        cat "$work/pmu.stderr" >&2
        echo "validation capability check: retired-branch PMU overflow delivery is required" >&2
        return 1
    fi
    if ! grep -Fxq 'Iterations: 4, programmed period: 100000 RCB' "$work/pmu.stdout" \
        || ! grep -Eq '^Skid \(RCB\): min=-?[0-9]+ max=-?[0-9]+ mean=-?[0-9]+\.[0-9]+ p99=-?[0-9]+$' \
            "$work/pmu.stdout" \
        || ! grep -Eq '^Recommended margin: [1-9][0-9]* RCB ' "$work/pmu.stdout"; then
        cat "$work/pmu.stdout" >&2
        cat "$work/pmu.stderr" >&2
        echo "validation capability check: PMU probe did not publish four overflow/skid samples" >&2
        return 1
    fi

    local verify_report=$work/cpuid-verify.json
    rm -f -- "$verify_report"
    local status=0
    VALIDATION_EXPECTED_GUEST=$cpuid_guest VALIDATION_EXPECTED_REPORT=$verify_report \
        timeout --kill-after=5s 30s "$hermit_bin" --log=info run --backend ptrace \
        --strict --verify --base-env=minimal --verify-json "$verify_report" \
        --tmp=/tmp -- "$cpuid_guest" >"$work/cpuid.stdout" \
        2>"$work/cpuid.stderr" || status=$?
    if ((status != 0)); then
        cat "$work/cpuid.stderr" >&2
        echo "validation capability check: Hermit could not execute the synthetic CPUID policy (exit $status)" >&2
        return 1
    fi
    if grep -Fq 'continuing with --max-timeslice=disabled' "$work/cpuid.stderr"; then
        cat "$work/cpuid.stderr" >&2
        echo "validation capability check: Hermit disabled requested PMU preemption" >&2
        return 1
    fi
    if grep -Fq 'continuing without CPUID interception' "$work/cpuid.stderr" \
        || grep -Fq 'CPUID faulting is unavailable' "$work/cpuid.stderr"; then
        cat "$work/cpuid.stderr" >&2
        echo "validation capability check: Hermit did not intercept CPUID" >&2
        return 1
    fi
    if ! grep -Fxq 'CPUID-SUCCESS vendor=GenuineIntel signature=00000663' "$work/cpuid.stdout"; then
        cat "$work/cpuid.stdout" >&2
        cat "$work/cpuid.stderr" >&2
        echo "validation capability check: guest did not observe Hermit's synthetic CPUID identity" >&2
        return 1
    fi
    if ! jq -e '
        (.verified == true) and (.verdict == "matched") and
        (.bitwise_parity == true) and (.comparison.strictness == "canonical") and
        (.comparison.compare_logs == true) and
        ((.compared_log_messages.left // 0) > 0) and
        ((.compared_log_messages.right // 0) > 0)
    ' "$verify_report" >/dev/null 2>&1; then
        cat "$verify_report" >&2 2>/dev/null || true
        echo "validation capability check: exact ptrace verification did not publish canonical evidence" >&2
        return 1
    fi
}

self_test() {
    local work
    work=$(mktemp -d "${TMPDIR:-/tmp}/validation-capabilities-self-test.XXXXXX")
    trap 'rm -rf -- "$work"' RETURN
    : >"$work/guest"
    chmod 755 "$work/guest"

cat >"$work/pmu-pass" <<'SH'
#!/bin/sh
test "$#" -eq 4 && test "$1" = --iterations && test "$2" = 4 && \
    test "$3" = --period && test "$4" = 100000 || exit 91
cat <<'OUT'
Iterations: 4, programmed period: 100000 RCB
Skid (RCB): min=1 max=4 mean=2.50 p99=4
Recommended margin: 100 RCB (2x observed max, minimum 100; empirical, not a hard bound)
OUT
SH
    cat >"$work/pmu-fail" <<'SH'
#!/bin/sh
echo planted-pmu-refusal >&2
exit 1
SH
    cat >"$work/pmu-broken-overflow" <<'SH'
#!/bin/sh
echo 'counter opened but no overflow signal was delivered'
SH
    cat >"$work/hermit-pass" <<'SH'
#!/bin/sh
test "$#" -eq 12 && test "$1" = --log=info && test "$2" = run && \
    test "$3" = --backend && test "$4" = ptrace && test "$5" = --strict && \
    test "$6" = --verify && test "$7" = --base-env=minimal && \
    test "$8" = --verify-json && test "$9" = "$VALIDATION_EXPECTED_REPORT" && \
    test "${10}" = --tmp=/tmp && test "${11}" = -- && \
    test "${12}" = "$VALIDATION_EXPECTED_GUEST" || exit 92
echo 'CPUID-SUCCESS vendor=GenuineIntel signature=00000663'
printf '%s\n' '{"verified":true,"verdict":"matched","bitwise_parity":true,"comparison":{"strictness":"canonical","compare_logs":true},"compared_log_messages":{"left":2,"right":2}}' > "$VALIDATION_EXPECTED_REPORT"
SH
    cat >"$work/hermit-wrong-cpuid" <<'SH'
#!/bin/sh
test "$#" -eq 12 && test "$1" = --log=info && test "$2" = run && \
    test "$3" = --backend && test "$4" = ptrace && test "$5" = --strict && \
    test "$6" = --verify && test "$7" = --base-env=minimal && \
    test "$8" = --verify-json && test "$9" = "$VALIDATION_EXPECTED_REPORT" && \
    test "${10}" = --tmp=/tmp && test "${11}" = -- && \
    test "${12}" = "$VALIDATION_EXPECTED_GUEST" || exit 92
echo 'CPUID-SUCCESS vendor=AuthenticAMD signature=00a60f12'
printf '%s\n' '{"verified":true,"verdict":"matched","bitwise_parity":true,"comparison":{"strictness":"canonical","compare_logs":true},"compared_log_messages":{"left":2,"right":2}}' > "$VALIDATION_EXPECTED_REPORT"
SH
    cat >"$work/hermit-pmu-fallback" <<'SH'
#!/bin/sh
test "$#" -eq 12 && test "$1" = --log=info && test "$2" = run && \
    test "$3" = --backend && test "$4" = ptrace && test "$5" = --strict && \
    test "$6" = --verify && test "$7" = --base-env=minimal && \
    test "$8" = --verify-json && test "$9" = "$VALIDATION_EXPECTED_REPORT" && \
    test "${10}" = --tmp=/tmp && test "${11}" = -- && \
    test "${12}" = "$VALIDATION_EXPECTED_GUEST" || exit 92
echo 'WARNING: perf_event_open is unavailable; continuing with --max-timeslice=disabled.' >&2
echo 'CPUID-SUCCESS vendor=GenuineIntel signature=00000663'
printf '%s\n' '{"verified":true,"verdict":"matched","bitwise_parity":true,"comparison":{"strictness":"canonical","compare_logs":true},"compared_log_messages":{"left":2,"right":2}}' > "$VALIDATION_EXPECTED_REPORT"
SH
    cat >"$work/hermit-wrong-argv" <<'SH'
#!/bin/sh
case " $* " in
  *' --no-virtualize-cpuid '*|*' --max-timeslice=disabled '*) ;;
  *) echo planted-wrong-argv-refusal >&2; exit 93 ;;
esac
echo 'CPUID-SUCCESS vendor=GenuineIntel signature=00000663'
printf '%s\n' '{"verified":true,"verdict":"matched","bitwise_parity":true,"comparison":{"strictness":"canonical","compare_logs":true},"compared_log_messages":{"left":2,"right":2}}' > "$VALIDATION_EXPECTED_REPORT"
SH
    cat >"$work/hermit-zero-messages" <<'SH'
#!/bin/sh
test "$#" -eq 12 && test "$1" = --log=info && test "$2" = run && \
    test "$3" = --backend && test "$4" = ptrace && test "$5" = --strict && \
    test "$6" = --verify && test "$7" = --base-env=minimal && \
    test "$8" = --verify-json && test "$9" = "$VALIDATION_EXPECTED_REPORT" && \
    test "${10}" = --tmp=/tmp && test "${11}" = -- && \
    test "${12}" = "$VALIDATION_EXPECTED_GUEST" || exit 92
echo 'CPUID-SUCCESS vendor=GenuineIntel signature=00000663'
printf '%s\n' '{"verified":true,"verdict":"matched","bitwise_parity":true,"comparison":{"strictness":"canonical","compare_logs":true},"compared_log_messages":{"left":0,"right":0}}' > "$VALIDATION_EXPECTED_REPORT"
SH
    cat >"$work/hermit-malformed-report" <<'SH'
#!/bin/sh
test "$#" -eq 12 && test "$1" = --log=info && test "$2" = run && \
    test "$3" = --backend && test "$4" = ptrace && test "$5" = --strict && \
    test "$6" = --verify && test "$7" = --base-env=minimal && \
    test "$8" = --verify-json && test "$9" = "$VALIDATION_EXPECTED_REPORT" && \
    test "${10}" = --tmp=/tmp && test "${11}" = -- && \
    test "${12}" = "$VALIDATION_EXPECTED_GUEST" || exit 92
echo 'CPUID-SUCCESS vendor=GenuineIntel signature=00000663'
printf '%s\n' '{not-json' > "$VALIDATION_EXPECTED_REPORT"
SH
    chmod 755 "$work"/pmu-* "$work"/hermit-*

    check_capabilities "$work/pmu-pass" "$work/hermit-pass" "$work/guest" "$work"
    if check_capabilities "$work/pmu-fail" "$work/hermit-pass" "$work/guest" "$work" \
        >/dev/null 2>&1; then
        echo "validation capability self-test: missing PMU was accepted" >&2
        return 1
    fi
    if check_capabilities "$work/pmu-broken-overflow" "$work/hermit-pass" "$work/guest" "$work" \
        >/dev/null 2>&1; then
        echo "validation capability self-test: broken PMU overflow evidence was accepted" >&2
        return 1
    fi
    if check_capabilities "$work/pmu-pass" "$work/hermit-wrong-cpuid" "$work/guest" "$work" \
        >/dev/null 2>&1; then
        echo "validation capability self-test: wrong CPUID identity was accepted" >&2
        return 1
    fi
    if check_capabilities "$work/pmu-pass" "$work/hermit-pmu-fallback" "$work/guest" "$work" \
        >/dev/null 2>&1; then
        echo "validation capability self-test: PMU fallback was accepted" >&2
        return 1
    fi
    if check_capabilities "$work/pmu-pass" "$work/hermit-wrong-argv" "$work/guest" "$work" \
        >/dev/null 2>&1; then
        echo "validation capability self-test: prohibited Hermit argv was accepted" >&2
        return 1
    fi
    if check_capabilities "$work/pmu-pass" "$work/hermit-zero-messages" "$work/guest" "$work" \
        >/dev/null 2>&1; then
        echo "validation capability self-test: zero-message canonical evidence was accepted" >&2
        return 1
    fi
    if check_capabilities "$work/pmu-pass" "$work/hermit-malformed-report" "$work/guest" "$work" \
        >/dev/null 2>&1; then
        echo "validation capability self-test: malformed canonical evidence was accepted" >&2
        return 1
    fi
    echo "validation capability self-test: PASS (1 acceptance, 7 refusals)"
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit
fi

[[ $# == 1 ]] || {
    echo "usage: ci/check-validation-capabilities.sh HERMIT_BIN" >&2
    exit 2
}
hermit_bin=$1
[[ -x $hermit_bin ]] || {
    echo "validation capability check: Hermit binary is not executable: $hermit_bin" >&2
    exit 1
}

work=$(mktemp -d "${TMPDIR:-/tmp}/validation-capabilities.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
"${CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror \
    "$ROOT_DIR/tests/backend-parity/fixtures/cpuid_probe.c" -o "$work/cpuid-probe"
"${CC:-cc}" -std=gnu11 -O2 -Wall -Wextra -Werror \
    "$ROOT_DIR/tests/util/pmu_skid.c" -o "$work/pmu-skid"
check_capabilities "$work/pmu-skid" "$hermit_bin" "$work/cpuid-probe" "$work"
echo "validation capability check: PASS (4 PMU overflows observed; synthetic CPUID identity observed)"
