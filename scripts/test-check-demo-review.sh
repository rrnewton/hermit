#!/usr/bin/env bash
# Tests for scripts/check-demo-review.sh.
#
# Each case builds a throwaway git repository, so the tests do not depend on
# this repository's history.

set -uo pipefail

GATE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/check-demo-review.sh"
[ -x "$GATE" ] || { echo "error: not executable: $GATE" >&2; exit 1; }

pass=0
fail=0

check() {  # $1 = description, $2 = expected exit, $3 = actual exit, $4 = output
    if [ "$2" = "$3" ]; then
        printf 'ok   %s (exit %s)\n' "$1" "$3"
        pass=$((pass + 1))
    else
        printf 'FAIL %s: expected exit %s, got %s\n' "$1" "$2" "$3"
        printf '%s\n' "$4" | sed 's/^/       | /'
        fail=$((fail + 1))
    fi
}

new_repo() {
    local dir
    dir=$(mktemp -d)
    git -C "$dir" init -q
    git -C "$dir" config user.email t@example.com
    git -C "$dir" config user.name t
    git -C "$dir" commit -q --allow-empty -m "base"
    printf '%s' "$dir"
}

commit_demo() {  # $1 repo, $2 path, $3 content, $4 message
    mkdir -p "$(dirname -- "$1/$2")"
    printf '%s\n' "$3" >"$1/$2"
    git -C "$1" add -A
    git -C "$1" commit -q -m "$4"
}

# Runs the gate over the last $2 commits of repo $1. Sets RC and OUT in the
# caller's scope; a command substitution would run in a subshell and lose RC.
RC=0
OUT=""
run_range() {  # $1 repo, $2 depth
    local base
    base=$(git -C "$1" rev-parse "HEAD~$2")
    OUT=$(cd "$1" && "$GATE" --range "$base..HEAD" 2>&1)
    RC=$?
}

TRAILER_ALL='Demo-Green-Review: reviewer=other demo=all result=GREEN evidence=log.txt'

# 1. Invalid revisions refuse with the gate's distinct inspection-error status.
r=$(new_repo)
OUT=$(cd "$r" && "$GATE" --range not-a-revision..HEAD 2>&1); RC=$?
check "invalid range is refused" 2 "$RC" "$OUT"
rm -rf "$r"

# 2. No demo touched -> pass.
r=$(new_repo)
commit_demo "$r" README.md hello "docs only"
run_range "$r" 1; check "no demo touched passes" 0 "$RC" "$OUT"
rm -rf "$r"

# 3. Demo touched, no trailer at all -> fail.
r=$(new_repo)
commit_demo "$r" demos/01-a.sh v1 "touch demo 1"
run_range "$r" 1; check "demo touched without trailer fails" 1 "$RC" "$OUT"
rm -rf "$r"

# 4. Demo touched, trailer naming that exact demo -> pass.
r=$(new_repo)
commit_demo "$r" demos/01-a.sh v1 "touch demo 1

Demo-Green-Review: reviewer=other demo=demos/01-a.sh result=GREEN evidence=log.txt"
run_range "$r" 1; check "trailer naming the touched demo passes" 0 "$RC" "$OUT"
rm -rf "$r"

# 5. Demo touched, trailer names demo=all -> pass.
r=$(new_repo)
commit_demo "$r" demos/01-a.sh v1 "touch demo 1

$TRAILER_ALL"
run_range "$r" 1; check "demo=all covers any touched demo" 0 "$RC" "$OUT"
rm -rf "$r"

# 6. A trailer naming a different demo must not cover the touched demo.
r=$(new_repo)
commit_demo "$r" demos/09-b.sh v1 "touch demo 9

Demo-Green-Review: reviewer=other demo=demos/01-a.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "trailer for a different demo does not cover this one" 1 "$RC" "$OUT"
printf '%s' "$OUT" | grep -q 'demos/09-b.sh' \
    || { echo "FAIL: message should name the uncovered path"; fail=$((fail + 1)); }
rm -rf "$r"

# 7. A trailer recorded before a later edit to the same demo is stale.
r=$(new_repo)
commit_demo "$r" demos/01-a.sh v1 "touch demo 1

$TRAILER_ALL"
commit_demo "$r" demos/01-a.sh v2 "edit demo 1 again after the attestation"
run_range "$r" 2
check "attestation superseded by a later edit is stale" 1 "$RC" "$OUT"
printf '%s' "$OUT" | grep -qi 'before the last' \
    || { echo "FAIL: message should explain the trailer predates the change"; fail=$((fail + 1)); }
rm -rf "$r"

# 8. Trailer recorded after the last edit -> pass.
r=$(new_repo)
commit_demo "$r" demos/01-a.sh v1 "touch demo 1"
git -C "$r" commit -q --allow-empty -m "attest after the edit

$TRAILER_ALL"
run_range "$r" 2; check "attestation after the last edit passes" 0 "$RC" "$OUT"
rm -rf "$r"

# 9. Directory-scoped trailer covers files beneath it.
r=$(new_repo)
commit_demo "$r" demos/qemu-busybox/init v1 "touch busybox init

Demo-Green-Review: reviewer=other demo=demos/qemu-busybox result=GREEN evidence=log.txt"
run_range "$r" 1; check "directory-scoped trailer covers files beneath it" 0 "$RC" "$OUT"
rm -rf "$r"

# 10. A bare filename must not cover a nested path.
r=$(new_repo)
commit_demo "$r" demos/qemu-busybox/init v1 "touch busybox init

Demo-Green-Review: reviewer=other demo=init result=GREEN evidence=log.txt"
run_range "$r" 1; check "bare filename does not cover a nested path" 1 "$RC" "$OUT"
rm -rf "$r"

# 11. Markdown-only demo change needs no attestation.
r=$(new_repo)
commit_demo "$r" demos/01-a.md doc "docs only under demos/"
run_range "$r" 1; check "demos/*.md needs no attestation" 0 "$RC" "$OUT"
rm -rf "$r"

# 12. Two demos touched, trailer covers only one -> fail.
r=$(new_repo)
mkdir -p "$r/demos"
printf 'v1\n' >"$r/demos/01-a.sh"
printf 'v1\n' >"$r/demos/09-b.sh"
git -C "$r" add -A
git -C "$r" commit -q -m "touch two demos

Demo-Green-Review: reviewer=other demo=demos/01-a.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "partial coverage of two touched demos fails" 1 "$RC" "$OUT"
rm -rf "$r"

# 13. result must be GREEN.
r=$(new_repo)
commit_demo "$r" demos/01-a.sh v1 "touch demo 1

Demo-Green-Review: reviewer=other demo=all result=RED evidence=log.txt"
run_range "$r" 1; check "result=RED does not satisfy the gate" 1 "$RC" "$OUT"
rm -rf "$r"

# 14. The local override never applies to --range.
r=$(new_repo)
commit_demo "$r" demos/01-a.sh v1 "touch demo 1"
base=$(git -C "$r" rev-parse HEAD~1)
OUT=$(cd "$r" && HERMIT_DEMO_REVIEW_OVERRIDE=1 "$GATE" --range "$base..HEAD" 2>&1); RC=$?
check "override does not apply to --range" 1 "$RC" "$OUT"
rm -rf "$r"

# 15. The existing demos 1-7 review form must not cover Demo 8.
r=$(new_repo)
mkdir -p "$r/demos"
printf 'v1\n' >"$r/demos/01-a.sh"
printf 'v1\n' >"$r/demos/08-h.sh"
git -C "$r" add -A
git -C "$r" commit -q -m "touch demos 1 and 8

Demo-Green-Review: reviewer=other demo=demos/01-a.sh,demos/02-b.sh,demos/03-c.sh,demos/04-d.sh,demos/05-e.py,demos/06-f.py,demos/07-g.sh result=GREEN evidence=one-through-seven.log"
run_range "$r" 1
check "demos 1-7 trailer does not cover Demo 8" 1 "$RC" "$OUT"
printf '%s' "$OUT" | grep -q 'demos/08-h.sh' \
    || { echo "FAIL: message should name uncovered Demo 8"; fail=$((fail + 1)); }
rm -rf "$r"

# 16. A reviewer cannot attest its own implementation.
r=$(new_repo)
commit_demo "$r" demos/06-f.py v1 "[hermit2, directives, unresolved, host, role=impl] touch demo 6

Demo-Green-Review: reviewer=directives demo=demos/06-f.py result=GREEN evidence=log.txt"
run_range "$r" 1
check "reviewer equal to role=impl identity fails" 1 "$RC" "$OUT"
printf '%s' "$OUT" | grep -q 'reviewer=directives is also the role=impl identity' \
    || { echo "FAIL: message should identify the self-review"; fail=$((fail + 1)); }
rm -rf "$r"

# 17. A GREEN trailer cannot cover a demo whose only reported result is PARTIAL.
r=$(new_repo)
commit_demo "$r" demos/05-e.py v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 5

demo 5 wired: FIRST RUN SAVED, PARTIAL
demo 5 unwired: FIRST RUN SAVED, PARTIAL

Demo-Green-Review: reviewer=other demo=all result=GREEN evidence=log.txt"
run_range "$r" 1
check "GREEN contradicted by body PARTIAL result fails" 1 "$RC" "$OUT"
printf '%s' "$OUT" | grep -q "contradicts the body's reported result" \
    || { echo "FAIL: message should identify the contradictory result"; fail=$((fail + 1)); }
rm -rf "$r"

# 18. A deliberate failing check does not contradict a later successful real run.
r=$(new_repo)
commit_demo "$r" demos/06-f.py v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 6

demo 6 forced cap: FAILURE
demo 6 wired: FIRST RUN SAVED, SUCCESS, SUCCESS

Demo-Green-Review: reviewer=other demo=demos/06-f.py result=GREEN evidence=log.txt"
run_range "$r" 1
check "distinct reviewer with successful real run passes" 0 "$RC" "$OUT"
rm -rf "$r"

# 19. A non-green result for another demo does not invalidate scoped evidence.
r=$(new_repo)
commit_demo "$r" demos/06-f.py v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 6

demo 5 wired: FIRST RUN SAVED, PARTIAL

Demo-Green-Review: reviewer=other demo=demos/06-f.py result=GREEN evidence=log.txt"
run_range "$r" 1
check "another demo's non-green result does not invalidate scoped evidence" 0 "$RC" "$OUT"
rm -rf "$r"

# 20. A bad historical trailer does not poison a later independent review.
r=$(new_repo)
commit_demo "$r" demos/06-f.py v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 6

Demo-Green-Review: reviewer=implementer demo=demos/06-f.py result=GREEN evidence=self.log"
git -C "$r" commit -q --allow-empty -m "independent review

Demo-Green-Review: reviewer=other demo=demos/06-f.py result=GREEN evidence=review.log"
run_range "$r" 2
check "later independent review supersedes bad historical trailer" 0 "$RC" "$OUT"
rm -rf "$r"

# 21. The banner emitted by a demo is a mechanical result line too.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 8

=== Demo 08: FAILURE: guest exited 125 ===

Demo-Green-Review: reviewer=other demo=all result=GREEN evidence=log.txt"
run_range "$r" 1
check "GREEN contradicted by the emitted FAILURE banner fails" 1 "$RC" "$OUT"
printf '%s' "$OUT" | grep -q "contradicts the body's reported result" \
    || { echo "FAIL: message should identify the contradictory banner"; fail=$((fail + 1)); }
rm -rf "$r"

# 22. The aggregate runner's compact FAIL line is a non-green result too.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 8

=== demo8: FAIL (exit 1) ===

Demo-Green-Review: reviewer=other demo=demos/08-h.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "GREEN contradicted by the aggregate FAIL result fails" 1 "$RC" "$OUT"
rm -rf "$r"

# 23. Repeating reviewer= cannot conceal the implementing identity.
r=$(new_repo)
commit_demo "$r" demos/06-f.py v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 6

Demo-Green-Review: reviewer=other reviewer=implementer demo=demos/06-f.py result=GREEN evidence=log.txt"
run_range "$r" 1
check "duplicate reviewer fields fail" 1 "$RC" "$OUT"
printf '%s' "$OUT" | grep -q 'reviewer= must occur exactly once' \
    || { echo "FAIL: message should identify duplicate reviewer fields"; fail=$((fail + 1)); }
rm -rf "$r"

# 24. A real success after a deliberate failing banner remains valid evidence.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 8

=== Demo 08: FAILURE: forced control ===
=== Demo 08: SUCCESS ===

Demo-Green-Review: reviewer=other demo=demos/08-h.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "successful banner after a deliberate failure passes" 0 "$RC" "$OUT"
rm -rf "$r"

# 25. A SECOND COLON BEFORE THE RESULT WORD must not hide the failure.
# `demos/03-chaos-concurrency.sh` emits this exact shape. Reading only as far
# as the first colon captured "Chaos Concurrency" as the result, so the banner
# classified as neither green nor non-green and the GREEN claim stood.
r=$(new_repo)
commit_demo "$r" demos/03-chaos-concurrency.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 3

=== Demo 03: Chaos Concurrency: FAILURE ===

Demo-Green-Review: reviewer=other demo=demos/03-chaos-concurrency.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "two-colon failure banner still contradicts GREEN" 1 "$RC" "$OUT"
rm -rf "$r"

# 26. Control for 25: the same two-colon shape reporting SUCCESS must still
# accept, so the fix refuses the failure rather than refusing the shape.
r=$(new_repo)
commit_demo "$r" demos/03-chaos-concurrency.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 3

=== Demo 03: Chaos Concurrency: SUCCESS ===

Demo-Green-Review: reviewer=other demo=demos/03-chaos-concurrency.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "two-colon success banner still accepts GREEN" 0 "$RC" "$OUT"
rm -rf "$r"

# 27. THE SUITE-LEVEL AGGREGATE carries no demo number, so the numbered scan
# never saw it. `demos/run-all.sh` emits this line verbatim.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 8

=== Demo suite: FAILURE — 1 demo(s) failed, 7 passed, 0 skipped ===

Demo-Green-Review: reviewer=other demo=all result=GREEN evidence=log.txt"
run_range "$r" 1
check "suite aggregate FAILURE contradicts a demo=all GREEN claim" 1 "$RC" "$OUT"
rm -rf "$r"

# 28. Control for 27: the suite's own success line must still accept `all`.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 8

=== Demo suite: SUCCESS — all 8 requested demos passed ===

Demo-Green-Review: reviewer=other demo=all result=GREEN evidence=log.txt"
run_range "$r" 1
check "suite aggregate SUCCESS still accepts a demo=all GREEN claim" 0 "$RC" "$OUT"
rm -rf "$r"

# 29. Negative control for 27: the aggregate says SOMETHING failed but not
# which demo, so it must not be attributed to a narrower claim.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 8

=== Demo suite: FAILURE — 1 demo(s) failed, 7 passed, 0 skipped ===

Demo-Green-Review: reviewer=other demo=demos/08-h.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "suite aggregate FAILURE is not attributed to one named demo" 0 "$RC" "$OUT"
rm -rf "$r"

# 30. A REAL RUN THAT FAILED AFTER an earlier green must refuse. The policy
# sentence is that a deliberate failing check stays compatible with a LATER
# successful run; membership alone had no notion of later and accepted this.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 8

=== Demo 08: GREEN ===
=== Demo 08: FAILURE (the real run, after the control) ===

Demo-Green-Review: reviewer=other demo=demos/08-h.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "a failure reported after a green still contradicts GREEN" 1 "$RC" "$OUT"
rm -rf "$r"

# Stage a demo path and run the commit-msg hook's own mode. Every case above
# drives `--range`; `.githooks/commit-msg` drives `--staged --message-file`,
# which is the path a local author actually hits, so it gets its own controls.
run_staged() {  # $1 repo, $2 demo path, $3 message
    mkdir -p "$(dirname -- "$1/$2")"
    printf 'v1\n' >"$1/$2"
    chmod +x "$1/$2"
    git -C "$1" add -A
    printf '%s\n' "$3" >"$1/.msg"
    OUT=$(cd "$1" && "$GATE" --staged --message-file .msg 2>&1)
    RC=$?
}

# 31. PROSE MUST NOT CANCEL A REAL FAILURE. Searching the whole line for a
# result word made the ordinary sentence below read as GREEN and cancel the
# banner above it, accepting a body the previous head correctly refused.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 8

=== Demo 08: FAILURE — required asset is missing: /assets/btrfs-convert ===
Demo 08: the calibration pass is unchanged by this commit

Demo-Green-Review: reviewer=other demo=demos/08-h.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "prose after a failure banner does not cancel it" 1 "$RC" "$OUT"
rm -rf "$r"

# 32. AND THE OTHER DIRECTION: this repository's own `demo08: <subject>`
# convention is prose, not a result. Whole-line scanning read "fail" out of the
# subject and refused a legitimate commit that carries a valid attestation.
r=$(new_repo)
commit_demo "$r" demos/08-h.sh v1 "demo08: make the engagement sweep runnable, and prove it can fail

[hermit2, implementer, unresolved, host, role=impl]

Demo-Green-Review: reviewer=other demo=demos/08-h.sh result=GREEN evidence=log.txt"
run_range "$r" 1
check "a demo08: commit subject is not a mechanical result" 0 "$RC" "$OUT"
rm -rf "$r"

# 33. The same subject through the commit-msg hook's own mode, because that is
# where a local author meets it first.
r=$(new_repo)
run_staged "$r" demos/08-h.sh "demo08: count path engagement per seed so zero engagement is NO-RESULT, not clean

[hermit2, implementer, unresolved, host, role=impl]

Demo-Green-Review: reviewer=other demo=demos/08-h.sh result=GREEN evidence=log.txt"
check "staged: a demo08: commit subject is not a mechanical result" 0 "$RC" "$OUT"
rm -rf "$r"

# 34. Staged control in the refusing direction, so case 33 cannot pass merely
# because the staged path stopped inspecting anything.
r=$(new_repo)
run_staged "$r" demos/08-h.sh "demo08: repair the sweep

[hermit2, implementer, unresolved, host, role=impl]

=== Demo 08: FAILURE (exit 1) ===

Demo-Green-Review: reviewer=other demo=demos/08-h.sh result=GREEN evidence=log.txt"
check "staged: a real failure banner still contradicts GREEN" 1 "$RC" "$OUT"
rm -rf "$r"

# 35. The label's last token is a result, so a demo that reports its failure
# before the colon is still seen. demos/07-drgn-kernel.py emits this shape.
r=$(new_repo)
commit_demo "$r" demos/07-g.py v1 "[hermit2, implementer, unresolved, host, role=impl] touch demo 7

Demo 07 failed: missing kernel debuginfo

Demo-Green-Review: reviewer=other demo=demos/07-g.py result=GREEN evidence=log.txt"
run_range "$r" 1
check "a failure reported before the colon still contradicts GREEN" 1 "$RC" "$OUT"
rm -rf "$r"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
