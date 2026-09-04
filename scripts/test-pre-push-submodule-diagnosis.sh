#!/usr/bin/env bash
# Both directions of the pre-push hook's "could not check" vs "does not compile"
# distinction, with no cargo run: the checker is stubbed so only the hook's own
# branch selection is under test.
#
# The defect, measured 2026-09-04 on devbig014: in a fresh detached worktree the
# submodules are unpopulated, Cargo cannot resolve a path dependency whose
# directory is absent, and the hook announced "the working tree does not compile
# in the default feature configuration" pointing at `cargo clippy`. It fired on a
# diff touching no Rust at all and never named the submodule. A clean detached
# worktree is the landing procedure CLAUDE.md prescribes, so the documented safe
# path reliably produced a misleading failure.
#
# ⚠️ BOTH DIRECTIONS. A hook that always blamed the submodule would pass the
# first case and hide every real compile failure, which is worse than the bug.
set -uo pipefail

HOOK=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/.githooks/pre-push
[[ -f $HOOK ]] || { echo "FAIL: hook not found at $HOOK" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

repo="$tmp/repo"
mkdir -p "$repo/scripts"
git -C "$repo" init -q
git -C "$repo" config user.email t@example.invalid
git -C "$repo" config user.name test

# A checker that always fails, so the hook always reaches its diagnosis branch.
# Which message it then chooses is the whole subject of this test.
cat > "$repo/scripts/check-default-build-warnings.sh" <<'STUB'
#!/usr/bin/env bash
echo "stub checker: deliberate failure" >&2
exit 1
STUB
chmod +x "$repo/scripts/check-default-build-warnings.sh"
printf 'seed\n' > "$repo/seed.txt"
git -C "$repo" add -A
git -C "$repo" commit -qm seed
head=$(git -C "$repo" rev-parse HEAD)
stdin_line="refs/heads/x $head refs/heads/x 0000000000000000000000000000000000000000"

run_hook() {
    ( cd "$repo" && printf '%s\n' "$stdin_line" | bash "$HOOK" origin https://example.invalid ) 2>&1
}

fail() { echo "FAIL: $1" >&2; exit 1; }

# ---- direction 2 first: no submodules at all, so a stub failure is a genuine
# ---- compile failure and must be reported as one.
out=$(run_hook)
[[ $out == *"does not compile in the default feature"* ]] ||
    fail "a genuine checker failure must still report as a compile failure; got: $out"
[[ $out != *"COULD NOT BE CHECKED"* ]] ||
    fail "a genuine compile failure was relabelled as could-not-check"

# ---- direction 1: an UNINITIALISED submodule recorded in the index. This is
# ---- exactly what `git submodule status` prefixes with '-' in a fresh worktree.
cat > "$repo/.gitmodules" <<'MODULES'
[submodule "agent-utils"]
	path = agent-utils
	url = https://example.invalid/agent-utils.git
MODULES
git -C "$repo" add .gitmodules
git -C "$repo" update-index --add --cacheinfo "160000,$head,agent-utils"
git -C "$repo" commit -qm "record an uninitialised submodule"

[[ $(git -C "$repo" submodule status | grep -c '^-') -eq 1 ]] ||
    fail "fixture did not produce an uninitialised submodule"

out=$(run_hook)
[[ $out == *"COULD NOT BE CHECKED"* ]] ||
    fail "an unpopulated submodule must not be reported as a compile failure; got: $out"
[[ $out == *"agent-utils"* ]] ||
    fail "the message must name the submodule"
[[ $out == *"git submodule update --init agent-utils"* ]] ||
    fail "the message must give the exact remedy"
[[ $out != *"does not compile in the default feature"* ]] ||
    fail "the misleading compile-failure message must be suppressed"

# ---- and it still refuses: the tree was not checked, so the push must not pass.
( cd "$repo" && printf '%s\n' "$stdin_line" | bash "$HOOK" origin https://example.invalid ) >/dev/null 2>&1
[[ $? -ne 0 ]] || fail "could-not-check must still refuse the push, not allow it"

echo "PASS: pre-push names an unpopulated submodule, and still reports a real compile failure as one"
