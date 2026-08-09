#!/usr/bin/env bash
# Plant the exact unpaired-workflow shape that twice interrupted global merge
# admission: the PR's merge-gate.yml blob changes while MERGE_GATE_V4_BLOB does
# not. Exercise the real workflow guard and the real ruleset reconciler, with a
# fake GitHub transport so this test cannot authorize or merge anything.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
configure="$root/scripts/configure-merge-gate-ruleset.sh"
workflow="$root/.github/workflows/merge-gate.yml"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

mkdir -p "$tmp/bin" "$tmp/state"
main_blob=1502659be5e773f9bdfa9d6e6bb7346d77f03ad9
changed_blob=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

cat >"$tmp/state/ruleset.json" <<'JSON'
{
  "id": 42,
  "name": "main check gating (admin-bypassable)",
  "target": "branch",
  "enforcement": "active",
  "conditions": {"ref_name": {"exclude": [], "include": ["~DEFAULT_BRANCH"]}},
  "rules": [{
    "type": "required_status_checks",
    "parameters": {"required_status_checks": [
      {"context": "merge-gate-v4", "integration_id": 15368}
    ]}
  }],
  "bypass_actors": [{"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}]
}
JSON
printf '%s\n' "$main_blob" >"$tmp/state/MERGE_GATE_V4_BLOB"
printf 'false\n' >"$tmp/state/MERGE_GATE_LEGACY_CONTEXT"

cat >"$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
state=${FAKE_GH_STATE:?}
main_blob=${FAKE_MAIN_BLOB:?}

if [[ ${1:-} == api && ${2:-} == --paginate ]]; then
    jq '[{id: .id, name: .name}]' "$state/ruleset.json"
    exit 0
fi
if [[ ${1:-} == api && ${2:-} == repos/rrnewton/hermit/rulesets/42 ]]; then
    cat "$state/ruleset.json"
    exit 0
fi
if [[ ${1:-} == api && ${2:-} == --method && ${3:-} == PUT ]]; then
    jq '. + {id: 42}' >"$state/ruleset.next"
    mv "$state/ruleset.next" "$state/ruleset.json"
    exit 0
fi
if [[ ${1:-} == api && ${2:-} == --method && ${3:-} == GET ]]; then
    if [[ ${*: -2:1} == --jq && ${*: -1} == .sha ]]; then
        printf '%s\n' "$main_blob"
    else
        printf 'name: merge-gate-v4\nMERGE_GATE_V4_BLOB\n' | base64 -w0
        printf '\n'
    fi
    exit 0
fi
if [[ ${1:-} == variable && ${2:-} == get ]]; then
    cat "$state/$3"
    exit 0
fi
if [[ ${1:-} == variable && ${2:-} == set ]]; then
    name=$3
    shift 3
    [[ ${1:-} == --repo ]]; shift 2
    [[ ${1:-} == --body ]]
    printf '%s\n' "$2" >"$state/$name"
    exit 0
fi
printf 'unsupported fake gh invocation: %q ' "$@" >&2
printf '\n' >&2
exit 2
STUB
chmod +x "$tmp/bin/gh"

run_configure() {
    PATH="$tmp/bin:/usr/bin:/bin" FAKE_GH_STATE="$tmp/state" \
        FAKE_MAIN_BLOB="$main_blob" "$configure" "$@"
}

# Extract the production guard. A restated predicate could keep passing after
# the real workflow was weakened, so the planted blob mismatch runs the actual
# `run:` block from merge-gate.yml.
python3 - "$workflow" >"$tmp/gate-definition-guard.sh" <<'PY'
import sys

lines = open(sys.argv[1]).read().splitlines()
want = "- name: Require the registered v4 gate definition"
start = next(i for i, line in enumerate(lines) if line.strip() == want)
run_at = next(
    i for i in range(start + 1, len(lines))
    if lines[i].strip() in ("run: |", "run: |-")
)
indent = len(lines[run_at]) - len(lines[run_at].lstrip())
body = []
for line in lines[run_at + 1:]:
    if line.strip() and len(line) - len(line.lstrip()) <= indent:
        break
    body.append(line[indent + 2:] if len(line) > indent + 2 else "")
print("\n".join(body))
PY

cat >"$tmp/bin/gate-gh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "${STUB_GATE_BLOB:?}"
STUB
chmod +x "$tmp/bin/gate-gh"
ln -s gate-gh "$tmp/bin/gate_gh"

guard() {
    local expected=$1 actual=$2
    local gate_bin="$tmp/gate-bin"
    mkdir -p "$gate_bin"
    ln -sf "$tmp/bin/gate-gh" "$gate_bin/gh"
    PATH="$gate_bin:/usr/bin:/bin" EXPECTED_GATE_BLOB="$expected" \
        STUB_GATE_BLOB="$actual" GATE_PATH=.github/workflows/merge-gate.yml \
        GH_TOKEN=stub REPO=rrnewton/hermit \
        SHA=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb \
        bash "$tmp/gate-definition-guard.sh"
}

neg=0
pos=0

if run_configure --check >"$tmp/check.out" 2>&1; then
    echo "FAIL: admin-bypassable ruleset was accepted" >&2
    exit 1
elif grep -Fq 'bypass actors' "$tmp/check.out"; then
    neg=$((neg + 1))
else
    cat "$tmp/check.out" >&2
    echo "FAIL: ruleset refusal did not name bypass authority" >&2
    exit 1
fi

if guard "$main_blob" "$changed_blob" >"$tmp/guard-neg.out" 2>&1; then
    echo "FAIL: unrotated workflow-only blob was accepted" >&2
    exit 1
elif grep -Fq 'Gate definition mismatch' "$tmp/guard-neg.out"; then
    neg=$((neg + 1))
else
    cat "$tmp/guard-neg.out" >&2
    echo "FAIL: unrotated workflow refusal did not come from the production guard" >&2
    exit 1
fi

run_configure --apply >"$tmp/apply.out"
[[ $(jq '.bypass_actors | length' "$tmp/state/ruleset.json") == 0 ]]
run_configure --check >"$tmp/check-fixed.out"
pos=$((pos + 1))

# Model the paired landing boundary: the registered value and main's workflow
# move to the planted blob together. The same real guard must now accept it, so
# removing bypass authority has not made workflow maintenance impossible.
printf '%s\n' "$changed_blob" >"$tmp/state/MERGE_GATE_V4_BLOB"
main_blob=$changed_blob
run_configure --check >"$tmp/check-rotated.out"
guard "$changed_blob" "$changed_blob" >"$tmp/guard-pos.out"
pos=$((pos + 1))

printf 'NEGATIVE refusals: %d/2   POSITIVE acceptances: %d/2\n' "$neg" "$pos"
[[ $neg == 2 && $pos == 2 ]]
echo "PASS: an unrotated workflow blob is refused and no configured actor can bypass that refusal"
