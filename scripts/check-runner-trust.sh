#!/usr/bin/env bash
# check-runner-trust.sh — self-hosted runner trust guard.
#
# Why bash, not a rust-script: this runs as the step immediately after
# actions/checkout and before any build/toolchain step on a self-hosted
# runner. At that point rust-script is not guaranteed to be installed, so
# the guard must depend only on bash + git, which checkout itself requires.
# The pre-checkout actor guard cannot use this file at all (no repo is
# checked out yet); it is inlined in each workflow YAML. This script is
# the post-checkout half.
#
# Trust rule (owner directive, 2026-10-31):
#   * The GitHub actor who triggered the run must be allow-listed.
#   * Every commit in base..HEAD must be AUTHORED by an allow-listed
#     identity. The author identity decides; the committer is logged for
#     audit but does not decide (a trusted rebase/cherry-pick of an
#     outside author's commit still refuses, and an outside committer
#     applying a trusted author's patch is noted, not refused).
#   * Merge commits are commits: their author is checked like any other.
#     There is no merge exemption.
#   * If the commit range cannot be established, refuse. An empty range
#     (HEAD is the base tip, e.g. a schedule run on the landing branch)
#     passes.
#
# Base-range rule:
#   * --base-from-ref: the base is origin/$GITHUB_REF_NAME, the branch the
#     workflow run is on (GitHub sets GITHUB_REF_NAME for schedule and
#     dispatch runs alike; a schedule always runs on the default branch). An
#     empty GITHUB_REF_NAME refuses; it never falls back to the landing
#     setting. Workflows whose canonical branch is the default branch use
#     this, because the landing setting can lag a default-branch change.
#   * --base <rev> / $RUNNER_TRUST_BASE if given, else origin/<landing>,
#     else <landing>, where <landing> is Hermit's landing branch
#     (HERMIT_LANDING_BRANCH, ci/landing-branches.env; main today). The
#     range is merge-base(base, HEAD)..HEAD, so a dispatched feature branch
#     is judged on exactly the commits it adds on top of the landing branch,
#     and a workflow_dispatch on the landing branch itself yields an empty
#     range.
#   * If origin/<landing> is absent the script tries `git fetch origin
#     <landing>` once; if the setting cannot be read, the base still cannot
#     be resolved, or merge-base fails, it refuses rather than guessing.
#
# Usage:
#   scripts/check-runner-trust.sh [--actor NAME] [--base REV | --base-from-ref] [--head REV]
#   scripts/check-runner-trust.sh --self-test
# Actor defaults to $GITHUB_ACTOR. Exit 0 = trusted, 1 = refused.
set -euo pipefail

# shellcheck source=scripts/lib/landing_branch.sh
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib/landing_branch.sh"

ALLOWED_ACTORS=("hermit-bot3" "rrnewton")
# Author identities are matched by email, not display name: names are
# free text and spoofable, while the email is the recorded identity.
# Defaults are the author emails observed on origin/main: the bot
# account (github-usage skill, 2026-10-18) and the repository owner's
# direct and GitHub-noreply addresses. Operators can extend the list
# without editing this file via $RUNNER_TRUST_ALLOWED_AUTHOR_EMAILS
# (comma- or space-separated); workflow jobs may source it from a
# repository variable so no personal address is hard-coded here.
ALLOWED_AUTHOR_EMAILS=(
  "344587749+hermit-bot3@users.noreply.github.com"
  "rrnewton@gmail.com"
  "rrnewton@users.noreply.github.com"
)
if [ -n "${RUNNER_TRUST_ALLOWED_AUTHOR_EMAILS:-}" ]; then
  extra="${RUNNER_TRUST_ALLOWED_AUTHOR_EMAILS//,/ }"
  # shellcheck disable=SC2206 # deliberate word splitting of the override list
  ALLOWED_AUTHOR_EMAILS+=($extra)
fi

actor="${GITHUB_ACTOR:-}"
base_arg="${RUNNER_TRUST_BASE:-}"
base_from_ref=0
head_arg="HEAD"

usage() { echo "usage: $0 [--actor NAME] [--base REV | --base-from-ref] [--head REV] | --self-test" >&2; }

in_list() { local needle="$1"; shift; local x; for x in "$@"; do [ "$x" = "$needle" ] && return 0; done; return 1; }

check_actor() {
  if [ -z "$actor" ]; then
    echo "::error::runner-trust: no actor supplied (--actor or GITHUB_ACTOR); refusing" >&2; return 1
  fi
  if in_list "$actor" "${ALLOWED_ACTORS[@]}"; then
    echo "runner-trust: actor '$actor' is allow-listed"
  else
    echo "::error::runner-trust: actor '$actor' is not allow-listed for self-hosted runners (allowlist: ${ALLOWED_ACTORS[*]})" >&2; return 1
  fi
}

check_authors() {
  local head base merge_base line author committer sha landing
  head="$(git rev-parse --verify "${head_arg}^{commit}" 2>/dev/null)" || {
    echo "::error::runner-trust: cannot resolve HEAD rev '$head_arg'; refusing" >&2; return 1; }
  base=""
  if [ "$base_from_ref" -eq 1 ]; then
    [ -n "${GITHUB_REF_NAME:-}" ] || {
      echo "::error::runner-trust: --base-from-ref but GITHUB_REF_NAME is empty; refusing" >&2; return 1; }
    base_arg="origin/${GITHUB_REF_NAME}"
    echo "runner-trust: base from the run's ref: $base_arg"
  fi
  if [ -n "$base_arg" ]; then
    base="$(git rev-parse --verify "${base_arg}^{commit}" 2>/dev/null)" || {
      echo "::error::runner-trust: cannot resolve --base '$base_arg'; refusing" >&2; return 1; }
  else
    landing="$(landing_branch HERMIT_LANDING_BRANCH)" || {
      echo "::error::runner-trust: cannot read Hermit's landing branch setting; refusing" >&2; return 1; }
    if ! git rev-parse --verify --quiet "origin/${landing}^{commit}" >/dev/null; then
      git fetch --no-tags origin "$landing" >/dev/null 2>&1 || true
    fi
    for cand in "origin/$landing" "$landing"; do
      if git rev-parse --verify --quiet "${cand}^{commit}" >/dev/null; then
        base="$(git rev-parse "${cand}^{commit}")"; echo "runner-trust: base is $cand ($base)"; break
      fi
    done
    [ -n "$base" ] || { echo "::error::runner-trust: cannot establish base (no origin/$landing or $landing); refusing" >&2; return 1; }
  fi
  merge_base="$(git merge-base "$base" "$head")" || {
    echo "::error::runner-trust: no merge-base between base $base and HEAD $head; refusing" >&2; return 1; }
  echo "runner-trust: checking authors in ${merge_base}..${head} (author decides; committer is audit-only)"
  local failures=0 count=0 log_out
  # Captured first: a failing git log inside a process substitution would go
  # unnoticed and read as an empty (passing) range.
  log_out="$(git log --format='%H %an <%ae>|%cn <%ce>' "${merge_base}..${head}")" || {
    echo "::error::runner-trust: git log failed for ${merge_base}..${head}; refusing" >&2; return 1; }
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    count=$((count + 1))
    sha="${line%% *}"; author="${line#* }"; author="${author%%|*}"; committer="${line##*|}"
    author_email="${author##*<}"; author_email="${author_email%%>*}"
    if in_list "$author_email" "${ALLOWED_AUTHOR_EMAILS[@]}"; then
      echo "runner-trust: ok $sha author=$author committer=$committer"
    else
      echo "::error::runner-trust: commit $sha authored by '$author' is not allow-listed (committer=$committer)" >&2
      failures=$((failures + 1))
    fi
  done <<< "$log_out"
  if [ "$count" -eq 0 ]; then echo "runner-trust: empty commit range (HEAD is at the base tip); nothing new to execute"; fi
  [ "$failures" -eq 0 ] || { echo "::error::runner-trust: $failures untrusted author(s) in range; refusing to run on a self-hosted runner" >&2; return 1; }
  echo "runner-trust: all $count commit author(s) in range are allow-listed"
}

SCRIPT_PATH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/$(basename -- "${BASH_SOURCE[0]}")"

self_test() {
  local tmp failures=0
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/runner-trust-test.XXXXXX")"
  trap 'rm -rf "$tmp"' RETURN
  local repo="$tmp/repo"
  git init -q -b main "$repo"
  commit_as() { # name email msg [committer-name committer-email]
    local an="$1" ae="$2" msg="$3" cn="${4:-$1}" ce="${5:-$2}"
    echo "$msg $(date +%s%N)" >> "$repo/f.txt"
    git -C "$repo" add f.txt
    GIT_AUTHOR_NAME="$an" GIT_AUTHOR_EMAIL="$ae" GIT_COMMITTER_NAME="$cn" GIT_COMMITTER_EMAIL="$ce" \
      git -C "$repo" commit -q -m "$msg"
  }
  git -C "$repo" config user.name "hermit-bot3"
  git -C "$repo" config user.email "344587749+hermit-bot3@users.noreply.github.com"
  commit_as "hermit-bot3" "344587749+hermit-bot3@users.noreply.github.com" "base"
  git -C "$repo" branch base-tip
  guard() { bash "$SCRIPT_PATH" --actor "$1" --base base-tip --head HEAD; }
  expect() { # name want-exit actual-exit
    if [ "$2" = "$3" ]; then echo "self-test PASS: $1"; else echo "self-test FAIL: $1 (want exit $2, got $3)" >&2; failures=$((failures + 1)); fi
  }
  # The cases run in a subshell (they cd into the scratch repo); it exits with
  # its own failure count, which is added here, so a FAIL makes the self-test
  # exit non-zero. Any command that fails under set -e also ends the subshell
  # non-zero and is counted.
  local sub_rc=0
  (
    cd "$repo"
    # 1. allow-listed actor + allow-listed authors passes
    commit_as "Repo Owner" "rrnewton@gmail.com" "owner work"
    guard hermit-bot3 >/dev/null 2>&1 && rc=0 || rc=$?; expect "allowlisted actor+authors passes" 0 "$rc"
    # 2. outside author in range refuses (even with trusted committer)
    commit_as "Outside Dev" "outside@example.com" "outside work" "hermit-bot3" "344587749+hermit-bot3@users.noreply.github.com"
    guard hermit-bot3 >/dev/null 2>&1 && rc=0 || rc=$?; expect "outside author refuses" 1 "$rc"
    git reset -q --hard HEAD~1
    # 3. non-allowlisted actor refuses
    guard outside-dev >/dev/null 2>&1 && rc=0 || rc=$?; expect "non-allowlisted actor refuses" 1 "$rc"
    # 4. empty range (schedule on main: HEAD == base tip) passes
    git checkout -q base-tip
    guard rrnewton >/dev/null 2>&1 && rc=0 || rc=$?; expect "empty range passes with allowlisted actor" 0 "$rc"
    # 5-10. --base-from-ref, after a default-branch change: the run's ref is
    # `integration`, which already carries an outside author from before the
    # trust rule; `main` is a mirror whose history diverged at base-tip. Each
    # case also checks the guard's own output line, not only its exit code.
    git checkout -q -b integration base-tip
    commit_as "Outside Dev" "outside@example.com" "landed before the rule"
    commit_as "hermit-bot3" "344587749+hermit-bot3@users.noreply.github.com" "integration tip"
    git update-ref refs/remotes/origin/integration HEAD
    git checkout -q -b mirror base-tip
    commit_as "Mirror Export" "export@example.com" "mirror-only history"
    git update-ref refs/remotes/origin/main HEAD
    git checkout -q integration
    out="$tmp/guard-out.txt"
    from_ref() { # event ref-name
      env -u RUNNER_TRUST_BASE GITHUB_EVENT_NAME="$1" GITHUB_REF_NAME="$2" \
        bash "$SCRIPT_PATH" --actor hermit-bot3 --base-from-ref --head HEAD > "$out" 2>&1
    }
    saw() { grep -qF -- "$1" "$out"; }
    # 5. schedule: the base resolves from the run's ref, and the run passes
    from_ref schedule integration && rc=0 || rc=$?; saw "base from the run's ref: origin/integration" || rc="$rc(no base line)"
    expect "schedule: base origin/\$GITHUB_REF_NAME resolves and passes" 0 "$rc"
    # 6. dispatch on the default branch: the same base resolves, and it passes
    from_ref workflow_dispatch integration && rc=0 || rc=$?; saw "base from the run's ref: origin/integration" || rc="$rc(no base line)"
    expect "dispatch: base origin/\$GITHUB_REF_NAME resolves and passes" 0 "$rc"
    # 7. empty GITHUB_REF_NAME refuses, at a head whose range would otherwise pass
    from_ref schedule "" && rc=0 || rc=$?; saw "GITHUB_REF_NAME is empty; refusing" || rc="$rc(no refusal line)"
    expect "empty GITHUB_REF_NAME refuses (isolated)" 1 "$rc"
    # 8. an unresolvable ref refuses, at the same head
    from_ref schedule no-such-branch && rc=0 || rc=$?; saw "cannot resolve --base 'origin/no-such-branch'" || rc="$rc(no refusal line)"
    expect "unresolvable ref refuses (isolated)" 1 "$rc"
    # 9. the same run judged against origin/main refuses: the failure --base-from-ref fixes
    env -u RUNNER_TRUST_BASE bash "$SCRIPT_PATH" --actor hermit-bot3 --base origin/main --head HEAD > "$out" 2>&1 && rc=0 || rc=$?
    saw "untrusted author(s) in range" || rc="$rc(no refusal line)"
    expect "base origin/main refuses on the diverged history" 1 "$rc"
    # 10. a new outside-author commit above the run's ref still refuses
    commit_as "Outside Dev" "outside@example.com" "new outside work above origin/integration"
    from_ref schedule integration && rc=0 || rc=$?; saw "authored by 'Outside Dev <outside@example.com>' is not allow-listed" || rc="$rc(no refusal line)"
    expect "new outside-author commit above origin/\$GITHUB_REF_NAME refuses" 1 "$rc"
    exit "$failures"
  ) || sub_rc=$?
  failures=$((failures + sub_rc))
  if [ "$failures" -eq 0 ]; then echo "runner-trust self-test: all cases passed"; else echo "runner-trust self-test: $failures failure(s)" >&2; return 1; fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --actor) actor="$2"; shift 2 ;;
    --base) base_arg="$2"; shift 2 ;;
    --base-from-ref) base_from_ref=1; shift ;;
    --head) head_arg="$2"; shift 2 ;;
    --self-test) self_test; exit $? ;;
    -h|--help) usage; exit 0 ;;
    *) usage; echo "::error::runner-trust: unknown argument '$1'" >&2; exit 1 ;;
  esac
done

check_actor
check_authors
echo "runner-trust: trusted; proceeding on self-hosted runner"
