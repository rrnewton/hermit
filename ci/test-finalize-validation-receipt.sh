#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Bracket the real Hermit producer handoff with inert local fixtures. The
# fixtures cannot contact GitHub or authorize a label; they only prove that the
# parent finalizer must run successfully before the parent publisher is called.

set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
handoff="$root/ci/finalize-validation-receipt.sh"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
ledger="$tmp/ledger.jsonl"
trace="$tmp/trace"
checkout="$tmp/hermit"
finalizer="$tmp/finalize_receipt.py"
ci_hub="$tmp/ci-hub"
mkdir -p "$checkout"

printf '%s\n' '{"schema_version":3,"commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","profile":"full","selection_mode":"full","result":"pass"}' >"$ledger"

cat >"$ci_hub" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'publisher:%s\n' "$*" >>"$RECEIPT_TEST_TRACE"
[[ $1 == apply-local-label ]]
shift
expected="--pr 17 --repo rrnewton/hermit --ledger $RECEIPT_TEST_LEDGER --hermit-repo $RECEIPT_TEST_CHECKOUT"
[[ $* == "$expected" ]]
[[ $(tail -n 1 "$RECEIPT_TEST_LEDGER" | jq -r .schema_version) == 6 ]]
EOF
chmod +x "$ci_hub"

run_handoff() {
    RECEIPT_TEST_TRACE="$trace" \
    RECEIPT_TEST_LEDGER="$ledger" \
    RECEIPT_TEST_CHECKOUT="$checkout" \
        "$handoff" \
        --repo rrnewton/hermit \
        --sha "$sha" \
        --ledger "$ledger" \
        --hermit-checkout "$checkout" \
        --pr 17 \
        --finalizer "$finalizer" \
        --ci-hub "$ci_hub"
}

# NEGATIVE 1: no finalizer means no publisher invocation.
if run_handoff >/dev/null 2>&1; then
    echo "FAIL: missing finalizer was accepted" >&2
    exit 1
fi
[[ ! -e $trace ]] || { echo "FAIL: publisher ran without a finalizer" >&2; exit 1; }

# NEGATIVE 2: a failing finalizer likewise cannot reach publication.
cat >"$finalizer" <<'EOF'
#!/usr/bin/env python3
raise SystemExit(1)
EOF
if run_handoff >/dev/null 2>&1; then
    echo "FAIL: failed finalizer was accepted" >&2
    exit 1
fi
[[ ! -e $trace ]] || { echo "FAIL: publisher ran after failed finalization" >&2; exit 1; }

# POSITIVE: the finalizer appends schema 6, then the publisher observes it.
cat >"$finalizer" <<'EOF'
#!/usr/bin/env python3
import argparse
import json
import os

parser = argparse.ArgumentParser()
parser.add_argument("--repo", required=True)
parser.add_argument("--sha", required=True)
parser.add_argument("--ledger", required=True)
parser.add_argument("--hermit-checkout", required=True)
args = parser.parse_args()
assert args.repo == "rrnewton/hermit"
assert args.sha == "a" * 40
with open(os.environ["RECEIPT_TEST_TRACE"], "a", encoding="utf-8") as stream:
    stream.write("finalizer\n")
with open(args.ledger, "a", encoding="utf-8") as stream:
    stream.write(json.dumps({"schema_version": 6, "commit": args.sha}) + "\n")
EOF
run_handoff
[[ $(sed -n '1p' "$trace") == finalizer ]]
[[ $(sed -n '2p' "$trace") == publisher:* ]]
[[ $(wc -l <"$trace") -eq 2 ]]

# NEGATIVE 3: repository identity is explicit, not selected from receipt bytes.
if "$handoff" --repo attacker/hermit --sha "$sha" --ledger "$ledger" \
    --hermit-checkout "$checkout" --pr 17 --finalizer "$finalizer" \
    --ci-hub "$ci_hub" >/dev/null 2>&1; then
    echo "FAIL: mismatched target repository was accepted" >&2
    exit 1
fi
[[ $(wc -l <"$trace") -eq 2 ]] || {
    echo "FAIL: repository mismatch reached the publisher" >&2
    exit 1
}

echo "PASS: 1 schema-6 handoff published; missing/failed finalizer and repository mismatch refused before publication"
