#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
LINT="$ROOT_DIR/scripts/check-compat-ratchet-provenance.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ratchet-provenance-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

pass=0
fail=0

expect_status() {
    local name=$1 expected=$2 fixture=$3
    local status=0
    "$LINT" "$fixture" >/dev/null 2>&1 || status=$?
    if ((status == expected)); then
        printf 'ok - %s\n' "$name"
        pass=$((pass + 1))
    else
        printf 'not ok - %s (expected %d, got %d)\n' "$name" "$expected" "$status" >&2
        fail=$((fail + 1))
    fi
}

cat >"$tmp/known.sh" <<'EOF'
# Ratchet provenance:
#   What: an exact measured test set.
#   Measured: 2026-08-03 at 0123456789abcdef0123456789abcdef01234567.
#   Method: the focused suite passed every selected row.
#   Why: the exact floor fails closed on regression.
readonly SAMPLE_COMPAT_EXPECTED=7
EOF

cat >"$tmp/unknown.sh" <<'EOF'
# Ratchet provenance:
#   What: a legacy exact test set.
#   Measured: origin unknown; re-derive before trusting.
#   Method: origin unknown; no run evidence survived.
#   Why: retain the fail-closed check until the value is re-derived.
readonly SAMPLE_COMPAT_TOTAL=7
EOF

cat >"$tmp/missing.sh" <<'EOF'
# An undocumented number must not pass as a ratchet.
readonly SAMPLE_COMPAT_TOTAL=7
EOF

cat >"$tmp/short-sha.sh" <<'EOF'
# Ratchet provenance:
#   What: an exact measured test set.
#   Measured: 2026-08-03 at 0123456.
#   Method: a claimed focused suite.
#   Why: the exact floor fails closed on regression.
readonly SAMPLE_COMPAT_TOTAL=7
EOF

expect_status 'known provenance passes' 0 "$tmp/known.sh"
expect_status 'explicit unknown provenance passes' 0 "$tmp/unknown.sh"
expect_status 'missing provenance fails' 1 "$tmp/missing.sh"
expect_status 'abbreviated SHA fails' 1 "$tmp/short-sha.sh"
expect_status 'repository ratchets pass' 0 "$ROOT_DIR/validate.sh"

printf 'ratchet-provenance lint self-test: %d passed, %d failed\n' "$pass" "$fail"
((fail == 0))
