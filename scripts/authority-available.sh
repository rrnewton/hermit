#!/usr/bin/env bash
# Probe whether the pinned check-status authority can be consulted at all.
#
# Exit 0  the authority is reachable; the caller may run its cases.
# Exit 3  it is NOT reachable. The caller has learned nothing about any check
#         and must not report a verdict.
#
# WHY THIS EXISTS AS ONE FILE. Four checkers in `make lint-checks` consult the
# authority, and each needs the same narrow rule. Four copies of it is exactly
# the shape where one gets updated and three do not.
#
# ⚠️ ONLY EXIT 3 MEANS UNREACHABLE, and that narrowness is the whole safety
# argument. check_outcome_adapter.py reserves 3 for AuthorityUnavailable alone:
# 0 is a classification, 1 an ordinary error, 2 argparse's usage error, and a
# digest mismatch raises AuthorityIntegrityError and does NOT come back as 3.
# So a caller that skips on 3 cannot be skipping a refusal, a bug, or a
# tampered authority -- only an outage.
set -uo pipefail

# Usage: authority-available.sh [ADAPTER [PROBE-ARG...]]
# Default probes the check-status authority. Both pinned-authority adapters
# spell "could not obtain it" as exit 3, so one probe serves both.
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
adapter=${1:-"$root/scripts/check_outcome_adapter.py"}
if [ "$#" -gt 0 ]; then shift; fi
if [ "$#" -eq 0 ]; then
    set -- --status completed --conclusion success
fi
"$adapter" "$@" >/dev/null 2>&1
rc=$?
if [ "$rc" -eq 3 ]; then
    exit 3
fi
exit 0
