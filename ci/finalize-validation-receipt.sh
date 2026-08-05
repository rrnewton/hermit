#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Thin producer-to-authority handoff for a completed local validation. This
# script deliberately owns no receipt predicate: the parent finalizer appends a
# schema-6 row, then the parent ci-hub verifier independently selects that row
# before it publishes a receipt, comment, or cache label.

set -euo pipefail

repo=
sha=
ledger=
hermit_checkout=
pr=
finalizer=
ci_hub=

usage() {
    cat >&2 <<'EOF'
Usage: ci/finalize-validation-receipt.sh \
  --repo rrnewton/hermit --sha FULL40 --ledger FILE \
  --hermit-checkout DIR --pr N --finalizer FILE --ci-hub FILE

The finalizer must succeed before apply-local-label is invoked. This adapter
does not parse, qualify, or rewrite receipt data itself.
EOF
}

while (($# > 0)); do
    case "$1" in
        --repo) repo=${2:-}; shift 2 ;;
        --sha) sha=${2:-}; shift 2 ;;
        --ledger) ledger=${2:-}; shift 2 ;;
        --hermit-checkout) hermit_checkout=${2:-}; shift 2 ;;
        --pr) pr=${2:-}; shift 2 ;;
        --finalizer) finalizer=${2:-}; shift 2 ;;
        --ci-hub) ci_hub=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'finalize-validation-receipt: unknown argument: %s\n' "$1" >&2; usage; exit 2 ;;
    esac
done

if [[ $repo != rrnewton/hermit ]]; then
    printf 'finalize-validation-receipt: untrusted target repository: %s\n' "${repo:-<empty>}" >&2
    exit 2
fi
if [[ ! $sha =~ ^[0-9a-f]{40}$ || ! $pr =~ ^[1-9][0-9]*$ ]]; then
    printf 'finalize-validation-receipt: --sha must be full lowercase hex and --pr must be positive\n' >&2
    exit 2
fi
if [[ ! -f $ledger || ! -d $hermit_checkout || ! -r $finalizer || ! -x $ci_hub ]]; then
    printf 'finalize-validation-receipt: ledger, checkout, finalizer, or ci-hub is unavailable\n' >&2
    exit 2
fi

python3 "$finalizer" \
    --repo "$repo" \
    --sha "$sha" \
    --ledger "$ledger" \
    --hermit-checkout "$hermit_checkout"

"$ci_hub" apply-local-label \
    --pr "$pr" \
    --repo "$repo" \
    --ledger "$ledger" \
    --hermit-repo "$hermit_checkout"
