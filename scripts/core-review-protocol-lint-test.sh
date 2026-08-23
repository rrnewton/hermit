#!/usr/bin/env bash
# Self-test for core-review-protocol-lint.sh.
#
# Feeds the linter a set of fixture PRs (labels + body + KVM flag) and asserts
# the exit status. Run locally or in CI:
#
#     scripts/core-review-protocol-lint-test.sh
#
# Exits 0 when every case matches its expected status, 1 otherwise.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly LINT="$SCRIPT_DIR/core-review-protocol-lint.sh"

# A complete, valid non-KVM PR body containing every required section.
readonly FULL_BODY='## Summary
Adds a thing.

## Determinism
Deterministic because reasons and an informal proof.

## Linux Semantics
Matches the kernel behavior described here.

## Validation
`cargo test -p hermit-detcore` passed at L2 (ptrace).

## Human Review Required
Trigger 4: core DetCore scheduling change.'

# The label set for a fully reviewed + approved PR (round 1).
readonly FULL_LABELS='post-facto-human-review
adversarial-review-codex1
adversarial-review-claude1
passed-review-codex
passed-review-claude'

# INERT FIXTURES. These two 40-hex strings are synthetic and name no commit in
# any repository, and every case below drives the linter purely through
# environment variables. Nothing here reads or writes GitHub, so no approval,
# label, review, or merge is ever planted to test a refusal.
readonly HEAD_SHA='0123456789abcdef0123456789abcdef01234567'
readonly OLD_SHA='fedcba9876543210fedcba9876543210fedcba98'

# Both lanes bound to the current head: the approval state the gate must accept.
readonly FULL_APPROVALS="[
  {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
  {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}
]"

pass=0
fail=0

# run_case NAME EXPECTED_EXIT LABELS BODY [IS_KVM] [COMMENTS_JSON] [HEAD_SHA]
#
# Comments and head default to a valid dual-lane exact-head approval, so the
# pre-existing label and body cases keep testing exactly what they always did.
# Give every comment fixture an authorized author unless it names one itself.
#
# The cases below are about the verdict GRAMMAR — markdown wrappers, lane case,
# supersession — not about who may approve. Since 2026-08-23 an approval must
# also be authenticated, and spelling a login into each of those fixtures would
# bury the thing each is actually testing. So the default is a reviewer with
# write access who is not the PR author, and the authentication cases further
# down state their identities explicitly. Deliberately non-destructive: a
# fixture that sets `user` or `author_association` keeps its own, and a
# fixture that is not a JSON array (the input-validation cases) passes through
# untouched rather than being silently repaired into something valid.
readonly DEFAULT_ROLE_TAG='[hermit2, reviewer-default, unresolved, devbig014, role=reviewer]'
readonly PR_AUTHOR_FIXTURE='pr-author-fixture'
authenticate_comments() {
    local json=$1 out
    # Prefix a role tag onto any comment body that does not already open with a
    # bracketed tag, so the pre-existing GRAMMAR cases keep testing grammar. A
    # fixture that states its own tag, or deliberately omits one, is untouched:
    # `test("^\\s*\\[")` looks only at what is there. A non-array payload (the
    # input-validation cases) passes through so it can still be refused.
    out=$(jq -c --arg tag "$DEFAULT_ROLE_TAG" '
        if type == "array" then
            map(if type == "object" and (.body // "" | test("^\\s*\\[") | not)
                then .body = ($tag + "\n" + (.body // ""))
                else . end)
        else . end' <<<"$json" 2>/dev/null) || { printf '%s' "$json"; return; }
    printf '%s' "$out"
}

run_case() {
    local name=$1 expected=$2 labels=$3 body=$4 is_kvm=${5:-false}
    local comments=${6-$FULL_APPROVALS} head=${7-$HEAD_SHA}
    local actual=0
    comments=$(authenticate_comments "$comments")
    PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM="$is_kvm" PR_NUMBER="test" \
        PR_HEAD_SHA="$head" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        echo "ok   - ${name} (exit ${actual})"
        pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"
        fail=$((fail + 1))
    fi
}

# run_message_case NAME EXPECTED_EXIT EXPECTED_SUBSTRING LABELS BODY COMMENTS_JSON HEAD
#
# Exit status alone cannot tell these cases apart: the input-validation checks
# are deliberately redundant with the binding check that follows them, so
# deleting one still yields exit 1 — via a MISLEADING diagnosis ("superseded
# approval ... current head is") instead of the true one ("PR_HEAD_SHA is
# missing"). Asserting the message is what makes those checks discriminable;
# without it a mutation that removes them leaves the suite green.
run_message_case() {
    local name=$1 expected=$2 needle=$3 labels=$4 body=$5 comments=$6 head=$7
    local actual=0 output
    comments=$(authenticate_comments "$comments")
    output=$(PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM=false PR_NUMBER="test" \
        PR_HEAD_SHA="$head" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" 2>&1) || actual=$?
    if [ "$actual" -eq "$expected" ] && [[ $output == *"$needle"* ]]; then
        echo "ok   - ${name} (exit ${actual}, diagnosed)"
        pass=$((pass + 1))
    elif [ "$actual" -ne "$expected" ]; then
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"
        fail=$((fail + 1))
    else
        echo "FAIL - ${name}: exit ${expected} but missing diagnosis '${needle}'"
        fail=$((fail + 1))
    fi
}

# --- Not applicable: no post-facto-human-review label always passes. ----------
run_case "unlabeled PR passes even with empty body" 0 \
    $'random-label\nlocally-validated' ""
run_case "unlabeled PR passes even missing everything the protocol wants" 0 \
    "" ""

# --- Happy paths -------------------------------------------------------------
run_case "labeled + full labels + all sections (non-KVM) passes" 0 \
    "$FULL_LABELS" "$FULL_BODY"

run_case "later review round (round 2 labels) still passes" 0 \
    $'post-facto-human-review\nadversarial-review-codex2\nadversarial-review-claude3\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "KVM PR with Relationship to gVisor section passes" 0 \
    "$FULL_LABELS" "${FULL_BODY}"$'\n\n## Relationship to gVisor\nN/A: no gVisor analog.' \
    true

run_case "bold-style headings are accepted" 0 \
    "$FULL_LABELS" \
    $'**Summary** foo\n**Determinism** bar\n**Linux Semantics** baz\n**Validation** qux\n**Human Review Required** trigger 4'

# --- Missing review labels blocks --------------------------------------------
run_case "missing adversarial-review-codex blocks" 1 \
    $'post-facto-human-review\nadversarial-review-claude1\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing adversarial-review-claude blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing passed-review-codex blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1\npassed-review-claude' \
    "$FULL_BODY"

run_case "missing passed-review-claude blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1\npassed-review-codex' \
    "$FULL_BODY"

run_case "adversarial review present but not approved blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex1\nadversarial-review-claude1' \
    "$FULL_BODY"

run_case "round label out of range (round 5) does not count, blocks" 1 \
    $'post-facto-human-review\nadversarial-review-codex5\nadversarial-review-claude5\npassed-review-codex\npassed-review-claude' \
    "$FULL_BODY"

# --- Missing body sections blocks --------------------------------------------
run_case "missing Summary section blocks" 1 \
    "$FULL_LABELS" \
    $'## Determinism\nx\n## Linux Semantics\ny\n## Validation\nz\n## Human Review Required\nt'

run_case "missing Linux Semantics section blocks" 1 \
    "$FULL_LABELS" \
    $'## Summary\nx\n## Determinism\ny\n## Validation\nz\n## Human Review Required\nt'

run_case "missing Human Review Required section blocks" 1 \
    "$FULL_LABELS" \
    $'## Summary\nx\n## Determinism\ny\n## Linux Semantics\nl\n## Validation\nz'

run_case "empty body blocks a labeled PR" 1 \
    "$FULL_LABELS" ""

# --- KVM-specific section -----------------------------------------------------
run_case "KVM PR without Relationship to gVisor section blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" true

run_case "non-KVM PR does not require Relationship to gVisor" 0 \
    "$FULL_LABELS" "$FULL_BODY" false

# --- Prose must not satisfy a section ----------------------------------------
run_case "prose mention of a section keyword does not satisfy it" 1 \
    "$FULL_LABELS" \
    $'## Summary\nIn summary, this changes determinism and validation broadly.\n## Determinism\nd\n## Validation\nv\n## Human Review Required\nt'

# --- Exact-head approval binding, BOTH DIRECTIONS ----------------------------
#
# The gate must accept a genuine approval at the current head and refuse
# everything else. The positive half is not optional: a check that refuses
# everything is as useless as one that accepts everything, and the defect this
# section exists to catch — labels present, no lane binding the head — was live
# on PR #2176 with the newest claude verdict at that head a REJECTION.

# POSITIVE: the gate must FIRE, not sit inert.
run_case "both lanes bound to the exact head passes" 0 \
    "$FULL_LABELS" "$FULL_BODY" false "$FULL_APPROVALS" "$HEAD_SHA"

run_case "whole-line markdown emphasis around a binding still passes" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"**APPROVED-AT: codex ${HEAD_SHA}**\"},
      {\"body\": \"\`APPROVED-AT: claude ${HEAD_SHA}\`\"}]" "$HEAD_SHA"

run_case "uppercase lane binds; an upper-case SHA does NOT (mirrors the reference) blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: CODEX ${HEAD_SHA}\"},
      {\"body\": \"approved-at: claude $(printf '%s' "$HEAD_SHA" | tr 'a-f' 'A-F')\"}]" \
    "$HEAD_SHA"

run_case "a rejection followed by a later approval at the head passes" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${OLD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# NEGATIVE: the exact defect — labels present, nothing binding the head.
run_case "THE DEFECT: both labels present but NO binding at all blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "[]" "$HEAD_SHA"

run_case "THE DEFECT: both labels present but bindings are at an OLD head blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${OLD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${OLD_SHA}\"}]" "$HEAD_SHA"

run_case "one lane bound, the other stale, blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${OLD_SHA}\"}]" "$HEAD_SHA"

run_case "a later rejection at the head revokes an earlier approval, blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "the historical lane-less rejection revokes both lanes, blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"REQUEST CHANGES AT ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "an approval quoted inside a rejection comment does not bind, blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\nSupersedes:\nAPPROVED-AT: claude ${HEAD_SHA}\"}]" \
    "$HEAD_SHA"

# NEGATIVE: unparseable verdict shapes must block, never be skipped. A heading
# prefix is the real case — one Claude-lane rejection was invisible to the
# reference parser for exactly this reason, leaving a stale approval standing.
run_case "a heading-prefixed verdict line is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"## APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a lane-less APPROVED-AT is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a prose verdict headline is unparseable and blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED AT EXACT HEAD ${HEAD_SHA} -- claude review.\"}]" "$HEAD_SHA"

# The three cases above would ALSO block for a second reason — the affected lane
# ends up with no binding — so on their own they do not prove the unparseable-line
# check does anything. These two isolate it: BOTH lanes are validly bound at the
# head, so the only thing that can block is the unparseable line itself. Deleting
# the malformed check leaves the cases above green and fails only these.
run_case "ISOLATES malformed: unparseable line blocks though both lanes bind" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"LGTM ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "ISOLATES malformed: heading-prefixed line blocks though both lanes bind" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"## APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# THE PR #2176 CASE, and the reason the leading-marker class exists. A lane
# approves at the head, then posts a rejection at the SAME head as a markdown
# heading. Matching the reference exactly would read the lane as approved and
# PASS the gate, because a heading-prefixed line matches neither the verdict
# grammar nor a suspect pattern without that class. It must refuse.
run_case "a heading-masked rejection at the head must not read as approved" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"## CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

# Recovery must work, or the gate is unsatisfiable after any masked rejection.
# Before the rejection grammar learned the prefix class, this case BLOCKED even
# after a clean re-approval: the masked line was merely flagged as unparseable,
# and the OTHER lane had not spoken past it, so the union kept reporting it.
# Honouring the rejection instead of flagging it is what makes recovery clean —
# and it is what the reference verifier does, so the two now agree here.
run_case "a masked rejection followed by a fresh approval passes again" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"## CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a masked claude rejection does not revoke the codex lane" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"## CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a blockquote-masked rejection at the head must not read as approved" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"> CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a list-marker-masked rejection at the head must not read as approved" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"- REQUEST CHANGES AT ${HEAD_SHA}\"}]" "$HEAD_SHA"

# A well-formed rejection for the OTHER lane opens with a verdict keyword and
# must NOT be mistaken for an unparseable line. Without this, tightening the
# malformed check into a false positive would go unnoticed.
run_case "a valid other-lane rejection is not treated as unparseable" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"CHANGES-REQUESTED-AT: claude ${OLD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"}]" "$HEAD_SHA"

# NEGATIVE: the binding inputs themselves must fail closed, or the whole check
# goes quietly inert the first time a caller forgets to pass them.
run_case "missing PR_HEAD_SHA blocks (does not fall back to label presence)" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "$FULL_APPROVALS" ""

run_case "a non-40-hex PR_HEAD_SHA blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "$FULL_APPROVALS" "not-a-sha"

run_case "a truncated PR_HEAD_SHA blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "$FULL_APPROVALS" "${HEAD_SHA:0:12}"

run_case "missing PR_COMMENTS_JSON blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false "" "$HEAD_SHA"

run_case "malformed (non-array) PR_COMMENTS_JSON blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false '{"body": "not an array"}' "$HEAD_SHA"

run_case "unparseable PR_COMMENTS_JSON blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false 'this is not json' "$HEAD_SHA"

# ...and they must be blocked FOR THE RIGHT REASON. See run_message_case.
run_message_case "missing PR_HEAD_SHA is diagnosed as a missing head, not a stale approval" \
    1 "PR_HEAD_SHA is missing" "$FULL_LABELS" "$FULL_BODY" "$FULL_APPROVALS" ""

run_message_case "a non-40-hex PR_HEAD_SHA is diagnosed as a bad head" \
    1 "PR_HEAD_SHA is missing" "$FULL_LABELS" "$FULL_BODY" "$FULL_APPROVALS" "not-a-sha"

run_message_case "missing PR_COMMENTS_JSON is diagnosed as missing comments, not missing approval" \
    1 "PR_COMMENTS_JSON is missing" "$FULL_LABELS" "$FULL_BODY" "" "$HEAD_SHA"

run_message_case "non-array PR_COMMENTS_JSON is diagnosed as a bad comment payload" \
    1 "PR_COMMENTS_JSON is missing" "$FULL_LABELS" "$FULL_BODY" '{"body": "x"}' "$HEAD_SHA"

# The positive direction for the diagnosis too: a genuinely stale approval must
# be reported as stale and must name both SHAs, so an operator can act on it.
run_message_case "a stale approval is diagnosed as superseded and names both SHAs" \
    1 "superseded approval from claude" "$FULL_LABELS" "$FULL_BODY" \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${OLD_SHA}\"}]" "$HEAD_SHA"

run_message_case "a missing binding is diagnosed as absent, not as stale" \
    1 "no exact-head approval from codex" "$FULL_LABELS" "$FULL_BODY" "[]" "$HEAD_SHA"

# An unlabeled PR is still out of scope even with no binding inputs at all: this
# lint never second-guesses whether the label should have been applied.
run_case "unlabeled PR still passes with no binding inputs" 0 \
    'some-other-label' "" false "" ""

# --- Malformed lines: superseded history vs an open question -----------------
#
# A prose verdict headline that predates the lane's current binding is history.
# One that comes after it could be the rejection the parser could not read.
# Blocking on both would make the gate unsatisfiable for #2176 and #2172, which
# already carry four such lines; blocking on neither would restore the hole.

run_case "an OLD malformed line before a valid re-approval does not block" 0 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED AT EXACT HEAD ${OLD_SHA} -- prose headline, unparseable\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "a malformed line AFTER the newest binding still blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED AT EXACT HEAD ${HEAD_SHA} -- prose headline, unparseable\"}]" \
    "$HEAD_SHA"

run_case "a malformed line in the SAME comment as the newest binding blocks" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\nLGTM ${HEAD_SHA}\"}]" "$HEAD_SHA"

run_case "an old malformed line still blocks when that lane never re-bound" 1 \
    "$FULL_LABELS" "$FULL_BODY" false \
    "[{\"body\": \"APPROVED AT EXACT HEAD ${OLD_SHA} -- prose headline\"},
      {\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"}]" "$HEAD_SHA"

# --- WHO MAY APPROVE ----------------------------------------------------------
#
# These fixtures bypass authenticate_comments and state every byte themselves,
# because what they test IS the tagging. Measured against the pre-fix script,
# every "blocks" case below exited 0.
run_raw_case() {
    local name=$1 expected=$2 comments=$3 actual=0
    PR_LABELS="$FULL_LABELS" PR_BODY="$FULL_BODY" PR_IS_KVM=false PR_NUMBER=test \
        PR_HEAD_SHA="$HEAD_SHA" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" >/dev/null 2>&1 || actual=$?
    if [ "$actual" -eq "$expected" ]; then
        echo "ok   - ${name} (exit ${actual})"; pass=$((pass + 1))
    else
        echo "FAIL - ${name}: expected exit ${expected}, got ${actual}"; fail=$((fail + 1))
    fi
}
run_raw_message_case() {
    local name=$1 expected=$2 needle=$3 comments=$4 actual=0 output
    output=$(PR_LABELS="$FULL_LABELS" PR_BODY="$FULL_BODY" PR_IS_KVM=false PR_NUMBER=test \
        PR_HEAD_SHA="$HEAD_SHA" PR_COMMENTS_JSON="$comments" \
        bash "$LINT" 2>&1) || actual=$?
    if [ "$actual" -eq "$expected" ] && [[ $output == *"$needle"* ]]; then
        echo "ok   - ${name} (exit ${actual}, diagnosed)"; pass=$((pass + 1))
    else
        echo "FAIL - ${name}: exit ${actual} (wanted ${expected}), diagnosis '${needle}' $([[ $output == *"$needle"* ]] && echo present || echo MISSING)"
        fail=$((fail + 1))
    fi
}
readonly TAG_REVIEWER='[hermit2, hermit-902, unresolved, devbig030, role=reviewer]'
readonly TAG_RELAY='[hermit2, hermit-bpf-post, unresolved, devbig030, role=relay]'
readonly TAG_LEGACY='[adversarial-reviewer agent, gpt-5.6-sol]'

# THE HOLE. A bare binding, no role tag, posted by the repository OWNER who is
# also the pull request's author. This is the case the previous revision still
# admitted, and it is the one the whole change exists to refuse.
run_raw_case "THE DEFECT: a BARE binding with no role tag does not approve" 1 \
    "[{\"user\": {\"login\": \"owner-and-author\"}, \"author_association\": \"OWNER\",
       \"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"user\": {\"login\": \"owner-and-author\"}, \"author_association\": \"OWNER\",
       \"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]"

# ... and it stays refused however high the permission goes.
run_raw_case "a bare binding from an OWNER still does not approve" 1 \
    "[{\"author_association\": \"OWNER\", \"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"author_association\": \"OWNER\", \"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]"

# THE REAL SHAPES, which the association model refused. On this very PR both
# approvals were posted by the owner, who was also the author, and one is a
# relay for a reviewer with no route to api.github.com.
run_raw_case "a role-tagged approval posted by the PR AUTHOR is accepted" 0 \
    "[{\"user\": {\"login\": \"owner-and-author\"}, \"author_association\": \"OWNER\",
       \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"user\": {\"login\": \"owner-and-author\"}, \"author_association\": \"OWNER\",
       \"body\": \"${TAG_RELAY}\nAPPROVED-AT: claude ${HEAD_SHA}\nPROVENANCE: transcribed verbatim.\"}]"

# Association is NOT the authority: a role-tagged review binds without it.
run_raw_case "a role-tagged approval with association NONE is accepted" 0 \
    "[{\"user\": {\"login\": \"relay-bot\"}, \"author_association\": \"NONE\",
       \"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"user\": {\"login\": \"relay-bot\"}, \"author_association\": \"NONE\",
       \"body\": \"${TAG_RELAY}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

# The fleet's tag shapes vary and the interior is deliberately not parsed;
# approval_binding.py measured that reading it produces wrong verdicts.
run_raw_case "a legacy-shaped role tag is accepted (interior is not parsed)" 0 \
    "[{\"body\": \"${TAG_LEGACY}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${TAG_LEGACY}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

run_raw_case "a tag below an opening heading still counts as a review comment" 0 \
    "[{\"body\": \"## Adversarial review\n${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"## Adversarial review\n${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"}]"

# A REJECTION IS NEVER REQUIRED TO BE TAGGED: authority may only be removed,
# never granted, by this check.
run_raw_case "an UNTAGGED rejection still supersedes a tagged approval" 1 \
    "[{\"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]"

# MALFORMED PAYLOAD MUST FAIL CLOSED. A non-object element makes jq raise and
# ABORT, so rows after it are dropped. Placed between the approval and the
# rejection that supersedes it, that deletes the rejection and the stale
# approval stands: a fail-OPEN reachable from comment content.
run_raw_case "THE DEFECT: a non-object element hiding a later rejection blocks" 1 \
    "[{\"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: claude ${HEAD_SHA}\"},
      \"not-an-object\",
      {\"body\": \"CHANGES-REQUESTED-AT: claude ${HEAD_SHA}\"}]"

run_raw_message_case "a malformed element is diagnosed as a refusal to read a truncated stream" \
    1 "not a JSON object" \
    "[{\"body\": \"${TAG_REVIEWER}\nAPPROVED-AT: codex ${HEAD_SHA}\"}, 42]"

run_raw_message_case "a bare binding is diagnosed as not-a-review-comment, not as absent" \
    1 "not a review comment" \
    "[{\"body\": \"APPROVED-AT: codex ${HEAD_SHA}\"},
      {\"body\": \"APPROVED-AT: claude ${HEAD_SHA}\"}]"

# --- PR_COMMENTS_FILE, the form the workflow actually uses --------------------
comments_tmp=$(mktemp)
authenticate_comments "$FULL_APPROVALS" > "$comments_tmp"
actual=0
PR_LABELS="$FULL_LABELS" PR_BODY="$FULL_BODY" PR_IS_KVM=false PR_NUMBER=test \
    PR_HEAD_SHA="$HEAD_SHA" PR_COMMENTS_FILE="$comments_tmp" \
    PR_AUTHOR="$PR_AUTHOR_FIXTURE" \
    bash "$LINT" >/dev/null 2>&1 || actual=$?
if [ "$actual" -eq 0 ]; then
    echo "ok   - PR_COMMENTS_FILE is read and a valid binding passes (exit 0)"
    pass=$((pass + 1))
else
    echo "FAIL - PR_COMMENTS_FILE is read and a valid binding passes: got ${actual}"
    fail=$((fail + 1))
fi
rm -f "$comments_tmp"

# PR_COMMENTS_FILE must WIN over PR_COMMENTS_JSON, or the workflow's real input
# could be silently shadowed by a stale inline value.
comments_tmp=$(mktemp)
authenticate_comments "$(printf '[{"body": "APPROVED-AT: codex %s"}]' "$OLD_SHA")" > "$comments_tmp"
actual=0
PR_LABELS="$FULL_LABELS" PR_BODY="$FULL_BODY" PR_IS_KVM=false PR_NUMBER=test \
    PR_HEAD_SHA="$HEAD_SHA" PR_COMMENTS_FILE="$comments_tmp" \
    PR_COMMENTS_JSON="$FULL_APPROVALS" PR_AUTHOR="$PR_AUTHOR_FIXTURE" \
    bash "$LINT" >/dev/null 2>&1 || actual=$?
if [ "$actual" -eq 1 ]; then
    echo "ok   - PR_COMMENTS_FILE takes precedence over PR_COMMENTS_JSON (exit 1)"
    pass=$((pass + 1))
else
    echo "FAIL - PR_COMMENTS_FILE takes precedence over PR_COMMENTS_JSON: got ${actual}"
    fail=$((fail + 1))
fi
rm -f "$comments_tmp"

PR_COMMENTS_FILE=/nonexistent/path/comments.json \
  run_message_case "an unreadable PR_COMMENTS_FILE is diagnosed as a plumbing fault" \
    1 "is not readable" "$FULL_LABELS" "$FULL_BODY" "" "$HEAD_SHA"


echo
echo "core-review-protocol-lint self-test: ${pass} passed, ${fail} failed."
[ "$fail" -eq 0 ]
