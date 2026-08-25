# Deciding whether an open pull request is already landed

Read this before sweeping the open-pull-request queue. Three independent sweeps
on 2026-08-24/25 each reached a wrong verdict first and were only corrected by a
second or third check, so the checks below are ordered and none of them is
sufficient alone.

**The one outcome that loses something is a wrong "already landed".** Closing a
pull request whose work is not on main destroys it. Everything here is arranged
to make that error expensive to commit and cheap to catch. When the checks
disagree, the safe answer is "needs a human read", not "close it".

## Why absence is so often reported wrongly: landing rewrites the SHA

`rrnewton/hermit` has **merge commits disabled** — `allow_merge_commit=false`,
squash and rebase only. Confirm with:

```console
gh api repos/rrnewton/hermit --jq '.allow_merge_commit'
```

So **every landing rewrites the commit SHA.** The bytes that reach `main` are not
the bytes that were on the branch. A later reader comparing by SHA, by ancestry,
or by tree finds no match and concludes the work is absent — when in fact it
landed under a different identity.

This is a property of how the project lands, not a mistake by whoever wrote the
branch. It explains a whole population of "genuinely lost" rows that were not
lost at all, and it is why the checks below anchor on content and behaviour
rather than on identity.

## The checks, in order

### 1. Content, anchored at the pull request's OWN merge base

A symbol counts as **already landed** only if it is:

- **ABSENT** at the pull request's own merge base, **and**
- **PRESENT** on current `main`.

A symbol that already existed at the merge base proves nothing in either
direction and must be excluded as indeterminate, not scored.

```console
BASE=$(git merge-base origin/main pr1234)
git diff "$BASE"...pr1234 -- '*.rs' | grep -E '^\+' \
  | grep -oE '\b(fn|struct|enum|const|static|trait)\s+[A-Za-z_][A-Za-z0-9_]*' \
  | awk '{print $2}' | sort -u
# then for each symbol: present at $BASE?  present at origin/main?
```

Anchoring matters and is not ceremony. Measured on real rows: one pull request
added 7 symbols of which only **1** was decisive; another added 24 with 11
pre-existing; another added 76 with 22 pre-existing. Scoring those without the
anchor produces confident wrong verdicts.

`git cherry origin/main <branch>` is a useful patch-id signal and is **never
proof of absence** — a squash-landed change has a different patch id.

### 2. Mechanism, because content re-lands under different bytes

Content is frequently re-implemented rather than merged. Take the pull request's
distinctive behaviour — an error string, a required rule, a constant, a defining
symbol — and ask whether **current main does that thing**, under any spelling.

Two measured examples:

- A pull request introducing a `ci = false` reason requirement scored "absent" by
  content. Main enforced the identical rule, in two directions, under a
  different function name, and refused a probe with the exact message.
- A pull request adding `DETERMINISTIC_PIPE_CAPACITY` scored "absent". Main pins
  the same thing as `DETERMINISTIC_PIPE_CAPACITY_BYTES` via `F_SETPIPE_SZ`.

**Grep for the mechanism, then PROVE it by exercising the behaviour.** A grep can
match a comment describing a defect rather than the code implementing the fix.

### 3. Residual, because a superseded mechanism can still carry unique content

This is the check that is skipped most often, and skipping it is how a wrong
close happens. A pull request whose mechanism has landed may still hold content
that has not.

The cheapest version takes seconds: for every file present on **both** sides,
compare sizes.

```console
for f in $(git diff --name-only "$BASE"...pr1234); do
  printf '%s pr=%s main=%s\n' "$f" \
    "$(git show pr1234:$f 2>/dev/null | wc -l)" \
    "$(git show origin/main:$f 2>/dev/null | wc -l)"
done
```

Measured: a pull request whose pipe-capacity mechanism had landed, whose test
files both existed on main, and whose target manifest had been deleted in a
format migration — every signal saying "close it" — turned out to carry **173
non-comment lines** of a distinct test scenario absent from main. The file sizes
were 669 vs 462 and 300 vs 167. Ten seconds of checking reversed a conclusion
that was about to be published as a close.

Same file name and same test-function names do **not** imply same content.

### 4. The dependency-pin hazard, in one direction only

`hermit` **pins** `reverie` and `agent-utils`. A mechanism can therefore arrive in
hermit's *build* through a pin bump while being absent from hermit's *tree*.
Such a change is landed-elsewhere, not absent.

This runs hermit-ward only. For reverie's or agent-utils' own pull requests the
authority is that repository's own `main`, and the hazard does not apply.

## Verdict vocabulary

| verdict | meaning | action |
|---|---|---|
| **genuinely pending** | absent by content, mechanism absent, no residual question | land it |
| **already landed** | mechanism present on main AND no residual content | close, citing the landing commit |
| **partial** | mechanism landed, unique content remains | extract the residual; do NOT close |
| **indeterminate** | rule cannot decide — no extractable symbols, or every symbol pre-existed | human read; do NOT guess |

Docs-only and config-only pull requests add no symbols and land in
**indeterminate** by construction. That is a correct verdict, not a failure of
the check, and those rows should be collected for one batched decision rather
than guessed at individually.

## What sweeps have actually returned

Across `hermit`, `reverie` and `dev-hermit` on 2026-08-24/25, **no sweep found a
safe close.** dev-hermit reached zero by *merging*; reverie's 23 open pull
requests classified with zero already-landed; hermit's sweeps found zero
closable and several land candidates. One near-miss reached the third check
before being caught.

The queue is **unlanded work, not duplicates**, and the shortage is **rebases,
not candidates**. Plan sweeps on that basis: budget for rebasing and landing,
not for closing.
