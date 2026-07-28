#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$ROOT_DIR"

categories=(applications data-handling determinism-stress language-runtimes system-utils)
for category in "${categories[@]}"; do
    test -d "tests/e2e/$category"
done

test -x tests/e2e/applications/run_all.sh
test -x tests/e2e/data-handling/run.sh
test -x tests/e2e/determinism-stress/run.sh
test -x tests/e2e/language-runtimes/run.sh
test -x tests/e2e/system-utils/run.sh

while IFS= read -r script; do
    test -x "$script" || {
        echo "e2e shell test is not executable: $script" >&2
        exit 1
    }
    bash -n "$script"
done < <(find tests/e2e -type f -name '*.sh' -print | LC_ALL=C sort)

if grep -R -n -E --include='*.sh' --exclude='check-contract.sh' -- \
    '--help|--version|/usr/bin/\[|/bin/true' tests/e2e; then
    echo "trivial help/version/no-op command found in substantive e2e tests" >&2
    exit 1
fi

mapfile -t scripts < <(find tests/e2e -type f -name '*.sh' -print | LC_ALL=C sort)
if perl -0ne '
    if (/--log(?:=|\s+)off(?:(?!--log).){0,240}--verify/s) {
        print "$ARGV: low-log verification contradiction\n";
        $bad = 1;
    }
    END { exit($bad ? 1 : 0) }
' "${scripts[@]}"; then
    :
else
    exit 1
fi

grep -Fq 'APPLICATION_BACKEND=ptrace' tests/e2e/applications/common.sh
grep -Fq 'DATA_HANDLING_BACKEND=ptrace' tests/e2e/data-handling/lib/common.bash
grep -Fq 'determinism_stress_backend=ptrace' tests/e2e/determinism-stress/common.sh
grep -Fq 'BACKEND_ALLOWLIST=(ptrace)' tests/e2e/language-runtimes/run.sh
grep -Fq 'ptrace)' tests/e2e/system-utils/run.sh
grep -Fq 'kvm)' tests/e2e/system-utils/run.sh

printf 'PASS: five e2e categories have explicit lane/backend contracts and no trivial tests\n'
