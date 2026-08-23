#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
SHELL_CLASSIFIER="$ROOT_DIR/scripts/classify-required-check.sh"
PYTHON_CLASSIFIER="$ROOT_DIR/scripts/check_outcome_adapter.py"

check() {
    local expected=$1 status=$2 conclusion=$3 python_result shell_result
    python_result=$("$PYTHON_CLASSIFIER" --status "$status" --conclusion "$conclusion")
    shell_result=$("$SHELL_CLASSIFIER" "$status" "$conclusion")
    [[ $python_result == "$expected" && $shell_result == "$expected" ]] || {
        echo "mismatch: $status/$conclusion expected=$expected python=$python_result shell=$shell_result" >&2
        exit 1
    }
}

check PASSED completed success
check PASSED "" success
for conclusion in failure timed_out error startup_failure; do
    check FAILED completed "$conclusion"
done
while IFS=: read -r status conclusion; do
    check NO_RESULT "$status" "$conclusion"
done <<'EOF'
completed:cancelled
completed:skipped
completed:neutral
completed:stale
completed:action_required
queued:
in_progress:
waiting:
requested:
pending:
missing:
completed:future_state
EOF

fixture='[{"statusCheckRollup":[{"status":"COMPLETED","conclusion":"CANCELLED"},{"state":"SUCCESS"}]}]'
annotated=$(printf '%s' "$fixture" | "$PYTHON_CLASSIFIER" --annotate-rollups)
[[ $(jq -r '.[0].statusCheckRollup[0]._checkOutcome' <<<"$annotated") == NO_RESULT ]]
[[ $(jq -r '.[0].statusCheckRollup[1]._checkOutcome' <<<"$annotated") == PASSED ]]

# Plant the #1597 shape: two opposite gate conclusions at one exact head.
# Both input orders must select the later run, and a different-head run must
# never enter the verdict.
head_sha=01e5653f2a59fdf5ce090c12aa45e944f7237c3f
older='{"name":"merge-gate","headSha":"'$head_sha'","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-08-04T15:12:05Z","detailsUrl":"https://github.com/o/r/actions/runs/30922888575/job/1"}'
newer='{"name":"merge-gate","headSha":"'$head_sha'","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-08-04T15:24:36Z","detailsUrl":"https://github.com/o/r/actions/runs/30923975433/job/2"}'
wrong_head='{"name":"merge-gate","headSha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-08-04T15:25:00Z","detailsUrl":"https://github.com/o/r/actions/runs/30924000000/job/3"}'
for rollup in "[$older,$newer,$wrong_head]" "[$wrong_head,$newer,$older]"; do
    selected=$(printf '%s' "$rollup" | "$PYTHON_CLASSIFIER" \
        --select-latest-rollup --head-sha "$head_sha")
    [[ $(jq 'length' <<<"$selected") -eq 1 ]]
    [[ $(jq -r '.[0].conclusion' <<<"$selected") == SUCCESS ]]
done


# Verify that a mismatched local authority is never imported. The adapter may
# obtain the reviewed bytes from another local checkout or the immutable URL,
# but both paths must produce the pinned digest before Python executes them.
ROOT_DIR="$ROOT_DIR" python3 - <<'PY'
import hashlib
import os
from pathlib import Path
import sys
import tempfile

root = Path(os.environ["ROOT_DIR"])
sys.path.insert(0, str(root / "scripts"))
import check_outcome_adapter as adapter

source = adapter._verified_source()
assert hashlib.sha256(source).hexdigest() == adapter.AUTHORITY_SHA256

class Response:
    def __init__(self, payload: bytes) -> None:
        self.payload = payload

    def __enter__(self) -> "Response":
        return self

    def __exit__(self, *args: object) -> None:
        return None

    def read(self) -> bytes:
        return self.payload

with tempfile.TemporaryDirectory() as directory:
    parent = Path(directory)
    authority = parent / adapter.AUTHORITY_RELATIVE_PATH
    authority.parent.mkdir(parents=True)
    authority.write_text("raise RuntimeError('unreviewed local authority executed')\n")
    os.environ["DEV_HERMIT_PARENT"] = str(parent)
    adapter.urlopen = lambda *_args, **_kwargs: Response(source)
    assert adapter._verified_source() == source

    adapter.urlopen = lambda *_args, **_kwargs: Response(source + b"# changed\n")
    try:
        adapter._verified_source()
    except RuntimeError as error:
        assert "digest mismatch" in str(error)
    else:
        raise AssertionError("changed remote authority passed its content pin")
PY

# Execute the two scripts that consume the adapter rather than merely importing
# the adapter in isolation. A small gh fixture makes both paths deterministic
# and keeps this test offline.
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
mkdir -p "$tmp/bin"
tee "$tmp/bin/with-proxy" >/dev/null <<'EOF_PROXY'
#!/usr/bin/env bash
set -euo pipefail
exec "$@"
EOF_PROXY
tee "$tmp/bin/gh" >/dev/null <<'EOF_GH'
#!/usr/bin/env bash
set -euo pipefail

main_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
case "$1:$2" in
    pr:list)
        printf '%s\n' '[{"number":2363,"title":"fixture","url":"https://github.com/rrnewton/hermit/pull/2363","headRefName":"fixture","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","baseRefName":"main","labels":[],"isDraft":false,"author":{"login":"fixture"},"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","statusCheckRollup":[{"name":"merge-gate-v4","headSha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/o/r/actions/runs/2/job/1"}]}]'
        ;;
    api:repos/rrnewton/hermit/commits/main)
        printf '%s\n' "$main_sha"
        ;;
    api:repos/rrnewton/hermit/commits/main/check-runs)
        printf '%s\n' '{"check_runs":[{"name":"Regular tests (GitHub-managed portable)","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","status":"completed","conclusion":"success","details_url":"https://github.com/o/r/actions/runs/3/job/1"}]}'
        ;;
    *)
        printf 'unexpected gh fixture command:' >&2
        printf ' %q' "$@" >&2
        printf '\n' >&2
        exit 2
        ;;
esac
EOF_GH
chmod +x "$tmp/bin/with-proxy" "$tmp/bin/gh"

PATH="$tmp/bin:$PATH"
export PATH
pr_status=$(
    python3 "$ROOT_DIR/scripts/pr_status.py" --repo rrnewton/hermit --no-main-ci
)
grep -Fq 'rrnewton/hermit#2363' <<<"$pr_status"
grep -Fq 'ci=green' <<<"$pr_status"

dag_health=$(
    PR_DAG_PROXY='' "$ROOT_DIR/scripts/pr-dag-health.sh" --repo rrnewton/hermit --format json --no-commute
)
jq -e '
    .prs[0].number == 2363 and
    .prs[0].ci.overall == "PASSED" and
    .main.head == "bbbbbbbbbbbb" and
    .main.outcome == "PASSED" and
    .main.green == true
' <<<"$dag_health" >/dev/null

echo "PASS: content pin and real classify-required-check, pr_status, and pr-dag-health consumers"
