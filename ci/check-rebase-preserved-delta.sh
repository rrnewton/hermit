#!/usr/bin/env bash
# Confirm that a rebase moved the base and nothing else.
#
# WHY THIS IS A SEPARATE CHECK. `ci/check-merge-driver-hygiene.sh` refuses the
# CONFIGURATION that lets a merge driver rewrite committed content. This checks
# the EVENT: given the range you had before a rebase and the range you have
# after, it proves the delta is the same one. It is the check that actually
# caught the 2026-08-08 incident, and it holds for any cause -- a merge driver, a
# smudge/clean filter, a bad conflict resolution, or a mis-picked commit.
#
#   ci/check-rebase-preserved-delta.sh <old-base> <old-head> <new-base> <new-head>
#
# It compares `git diff <base> <head>` on both sides, ignoring the two things
# that legitimately move when the base moves:
#   * `index <blob>..<blob> <mode>` lines -- blob hashes change with content
#     that came from the new base;
#   * `@@ -a,b +c,d @@` hunk headers -- line offsets shift when the new base
#     added or removed lines above your change.
# Every other line, including the file list and every +/- content line, must
# match exactly.
#
# Exit status:
#   0  the delta is unchanged
#   1  the delta changed -- inspect before pushing
#   2  usage / internal error

set -uo pipefail

if [[ $# -ne 4 ]]; then
    cat >&2 <<'EOF'
usage: ci/check-rebase-preserved-delta.sh <old-base> <old-head> <new-base> <new-head>

  Record <old-base> and <old-head> BEFORE you rebase:
      old_base=$(git merge-base origin/main HEAD)   old_head=$(git rev-parse HEAD)
  then rebase, then:
      ci/check-rebase-preserved-delta.sh "$old_base" "$old_head" <new-base> $(git rev-parse HEAD)
EOF
    exit 2
fi

old_base=$1 old_head=$2 new_base=$3 new_head=$4

for rev in "$old_base" "$old_head" "$new_base" "$new_head"; do
    git rev-parse --verify --quiet "${rev}^{commit}" >/dev/null || {
        echo "check-rebase-preserved-delta.sh: not a commit: $rev" >&2
        exit 2
    }
done

tmp=$(mktemp -d) || exit 2
trap 'rm -rf "$tmp"' EXIT

# `index` and `@@` lines legitimately move when the base moves; nothing else may.
normalize() {
    grep -v -E '^index [0-9a-f]+\.\.[0-9a-f]+|^@@ '
}

git diff "$old_base" "$old_head" | normalize > "$tmp/before"
git diff "$new_base" "$new_head" | normalize > "$tmp/after"

before_bytes=$(git diff "$old_base" "$old_head" | wc -c)
after_bytes=$(git diff "$new_base" "$new_head" | wc -c)

echo "before: $(git rev-parse --short "$old_base")..$(git rev-parse --short "$old_head")  ${before_bytes} bytes, $(git diff --name-only "$old_base" "$old_head" | wc -l) file(s)"
echo "after:  $(git rev-parse --short "$new_base")..$(git rev-parse --short "$new_head")  ${after_bytes} bytes, $(git diff --name-only "$new_base" "$new_head" | wc -l) file(s)"

if cmp -s "$tmp/before" "$tmp/after"; then
    echo "check-rebase-preserved-delta.sh: OK — every content line matches; only blob hashes and hunk offsets moved"
    exit 0
fi

echo
echo "check-rebase-preserved-delta.sh: FAIL — the rebase changed your delta"
echo
comm -13 <(sort "$tmp/before") <(sort "$tmp/after") | head -20 |
    sed 's/^/  only after:  /'
comm -23 <(sort "$tmp/before") <(sort "$tmp/after") | head -20 |
    sed 's/^/  only before: /'
echo
echo "  Lines only in one side: after=$(comm -13 <(sort "$tmp/before") <(sort "$tmp/after") | wc -l) before=$(comm -23 <(sort "$tmp/before") <(sort "$tmp/after") | wc -l)"
echo
echo "A silent whole-file re-serialization looks exactly like this: the rebase"
echo "reports success, git status is clean, and the delta is many times larger."
echo "Run ci/check-merge-driver-hygiene.sh, and see git rebase --apply, which"
echo "replays with git apply instead of a per-file 3-way merge."
exit 1
