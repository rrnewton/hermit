# Deciding whether an open pull request is already landed

Read this before sweeping the open-pull-request queue. Across six sweeps of
three repositories on 2026-08-24/25, several reached a WRONG VERDICT FIRST and
were corrected only by a later check — including two that were about to be
published as closes. The checks below are therefore ordered, and none of them is
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

### 5. Effect, because a correct verdict about the past can be the wrong action

**Checks 1 to 4 all ask what already happened. This one asks what landing would
DO.** It is the only forward-looking check, and it catches cases the other four
cannot: the content is genuinely absent, the mechanism is genuinely absent, so
every earlier check says "pending, land it" — and landing would make current
behaviour WORSE while reading as progress.

Ask: **has main improved this code since the branch left it?**

```console
BASE=$(git merge-base origin/main pr1234)
for f in $(git diff --name-only "$BASE"...pr1234); do
  printf '%s  main-since-base %s  pr-since-base %s\n' "$f" \
    "$(git diff --numstat "$BASE" origin/main -- "$f" | awk '{print "+"$1"/-"$2}')" \
    "$(git diff --numstat "$BASE" pr1234    -- "$f" | awk '{print "+"$1"/-"$2}')"
done
git log --oneline "$BASE"..origin/main -- <the-directory-it-touches>
```

Two signals, both measured:

- **Main moved further than the branch in the same files.** One draft changed
  `detcore-dbt/src/lib.rs` by +248/-138 while main had moved it +373/-51. The
  branch DELETES 138 lines that main has since built on. Porting it is a
  regression wearing the shape of a feature.
- **A commit on main already does the branch's stated job.** The same draft was
  titled "route DBT verification through canonical logs"; `a811f33684`
  "detcore-dbt: route canonical logs through protected evidence" had landed 28
  hours earlier and more completely — 9 mentions of `canonical` in main's
  detcore-dbt against the draft's 2.

This check has already reversed a published verdict. A draft classified
"genuinely pending" from the symbol rule alone was closed by its reviewer
because porting it would have REGRESSED current accounting. The symbol verdict
was right about the past fact and wrong as an action.

Two closed drafts make the failure mode concrete:

- [Hermit #2308](https://github.com/rrnewton/hermit/pull/2308) had exact patch
  content and symbols absent from `main`, but its keep-going mechanism had been
  implemented more strongly. The mechanism check therefore found supersession,
  not a genuinely pending change. The effect check rejected the remaining
  cumulative `by_tag` closure because it would have replaced newer
  latest-attempt and unreported-node accounting.
- [Hermit #2410](https://github.com/rrnewton/hermit/pull/2410) deleted five
  unchanged demo files and merged cleanly. While the draft sat, commit
  `be5cc1a79f` added a workflow that executes `demos/05-qemu-busybox.sh` and
  watches that file, `demos/boot_qemu.sh`, and the two files under
  `demos/qemu-busybox/**`. Landing the deletion would therefore have left a
  tracked workflow pointing at missing runtime inputs. The unchanged blobs were
  not dead weight; they had become load-bearing without changing themselves.

#### Deletions invert the marker test

For an additions-only change, content present on `main` can support an
already-landed verdict. For a deletions-only change, that same observation
proves the literal deletion did not land; it does not prove the deletion remains
safe or valid to land now.

Before accepting a deletion, scan current callers of every removed path or
interface, including files added after the pull request's base:

```console
git fetch origin main
git grep -n -F '<removed path or interface>' origin/main
git log --oneline "$BASE"..origin/main -- <its callers and containing directory>
```

A clean merge says Git can perform the deletion. It does not say current code
can run afterward.

#### A stated prerequisite can become false while the draft waits

Treat every phrase such as "migrated first", "landed separately", "now lives
at", or "no callers remain" as a live precondition, not historical evidence.
Verify it against the current remote tree before landing:

```console
git -C <repository> fetch origin main
git -C <repository> cat-file -e origin/main:<required-path>
git -C <repository> show origin/main:<required-path>
```

Two drafts have already failed this check:

- [Hermit #2410](https://github.com/rrnewton/hermit/pull/2410) named four
  replacement paths in `dev-hermit` and said they had migrated first. All four
  were absent from current `dev-hermit/main`. That independently invalidated
  the deletion even before the new Hermit workflow caller was considered.
- [Hermit #2162](https://github.com/rrnewton/hermit/pull/2162) named coordinated
  Reverie and LiteInst heads as part of a five-repository warnings program.
  After fetching both repositories, neither named SHA was an ancestor of its
  current `main`. The mechanisms had also diverged: LiteInst carried the
  eight-root warning policy under different history, while Reverie carried none
  of those source-level warning attributes. The stale coordination claim could
  no longer authorize landing the Hermit snapshot; its remaining Python policy
  was parked for an owner decision instead.

A prerequisite verified when the draft was written can disappear, be renamed,
land under different history, or never land at all. Re-read it at the landing
tip, and compare the mechanism when ancestry differs.

### 6. A symbol absent from BOTH sides means the check cannot speak

⚠️ **A marker absent at the merge base AND absent on main does not mean unlanded.
It means the check could not decide.** Classifying those as absent manufactures
confident wrong verdicts at scale, because check 2 above is exactly the case that
produces them: the mechanism landed under a different symbol name, so no
identifier is shared and both counts read zero.

This is not hypothetical, and the abstract rule will not stop anyone. The example:

    hermit#2422 added   detcore/src/consts.rs:45         pub const DET_PIPE_CAPACITY: i32 = 8192;
    main already had    detcore/src/syscalls/files.rs:78 const DETERMINISTIC_PIPE_CAPACITY_BYTES: i32 = 8 * 1024;

Same value. Same pipe-creation handler. Same `F_SETPIPE_SZ` on the read end. Same
stated rationale, down to the same cause — Linux sizes a new pipe from the host's
per-user `pipe-user-pages-soft` accounting, so a parallel validate crosses the
threshold using only its own concurrent guests. **Both symbols count zero at the
merge base and zero on main.** A pure symbol rule reads that as "genuinely
unlanded, land candidate". It was already landed, and landing the pull request
would have added a second constant and a second pin site for one property.
hermit#2331 was the same shape against main's `sabre_backend_evidence_line`.

So route both-sides-absent to **indeterminate**, never to *genuinely pending*, and
resolve it by hand with a search for the **value and the rationale rather than the
identifier**: grep main for the constant's value, the syscall it configures, and a
distinctive phrase from its comment.

Measured cost of getting this wrong: in the 2026-08-25 sweep of hermit's 46 open
`[SALVAGE]` drafts, **21 of 46 — the largest bucket — landed in this state.** Had
they been auto-classified as absent, 21 verdicts would have been wrong in the
direction that wastes a receipt and re-lands existing work.

### 7. Backend presence, before you blame a red on the pull request

**A red you see while assessing may belong to your box, not to the branch.**
Backend absence is not reported uniformly, and one form MANUFACTURES failures:

- **Missing DynamoRIO makes the DBT tests FAIL.** On a box without the backend
  this produces roughly **20 reds that have nothing to do with any pull
  request**.
- **Missing SaBRe makes its tests SKIP.** Same condition, opposite reporting.

So before attributing a DBT red to the branch under assessment, establish
whether the backend is actually present:

```console
ls target/install_pkg/rsrcs/            # dynamorio, sabre, e9patch, liteinst runtimes
./target/debug/hermit run --backend=dbt -- /bin/true
```

**Distinguish two different causes that look identical.** A build without
`--features third-party-backends` reports `backend 'dbt' is unavailable: DBT
support was not included in this build` — that is a MISSING FEATURE FLAG in your
binary, not a missing backend on the host, and not a defect in the branch.
Neither is the pull request's fault, and neither should be recorded against it.

This is the only check here about the ENVIRONMENT you are assessing in rather
than about the branch or about main.

## Verdict vocabulary

| verdict | meaning | action |
|---|---|---|
| **genuinely pending** | absent by content, mechanism absent, no residual question | land it |
| **already landed** | mechanism present on main AND no residual content | close, citing the landing commit |
| **partial** | mechanism landed, unique content remains | extract the residual; do NOT close |
| **indeterminate** | rule cannot decide — no extractable symbols, or every symbol pre-existed | human read; do NOT guess |
| **blocked on a cross-repo dependency** | content and mechanism both absent, but the capability its tests assert has not landed in `reverie` or `agent-utils`, or hermit's pin does not include it | LEAVE OPEN and NAME THE SUCCESSOR; landing it creates a red |
| **supersede-and-regress** | landing would weaken newer invariants, remove a current dependency, or rely on a prerequisite that is now false | do NOT port; extract any residual, then close citing the newer mechanism or dependency |

Docs-only and config-only pull requests add no symbols and land in
**indeterminate** by construction. That is a correct verdict, not a failure of
the check, and those rows should be collected for one batched decision rather
than guessed at individually.

## Three dispositions, not two

A sweep that can only close or land will record the third outcome as nothing,
and **four supersession chains dead-ended that way in one night** — each one a
pull request whose real status was known by somebody and written down nowhere.

| disposition | when | what to record |
|---|---|---|
| **close** | already landed, or abandoned and unsafe to port | the superseding commit, or why porting is unsafe |
| **land** | genuinely pending and landing improves main | the usual receipt |
| **correctly parked, with a NAMED SUCCESSOR** | real work, correctly blocked — a cross-repo dependency, an unlanded capability, a decision it waits on | **WHO or WHAT unblocks it**, by name |

The third is not a failure to decide. It is a decision, and it is only useful if
the successor is named: "waiting on reverie#430 to land and hermit's pin to
advance" is a disposition; "still open" is not. A parked pull request with no
named successor is indistinguishable from a forgotten one, which is exactly how
the four chains were lost.

## What sweeps have actually returned

**Measured, not assumed: six sweeps across `hermit`, `reverie` and `dev-hermit`
found ZERO clean already-landed closes.**

A reader arriving at a queue of 88 open pull requests will reasonably assume it
is full of duplicates. It is not, and this table exists so nobody spends another
night establishing that:

| repository | outcome |
|---|---|
| `dev-hermit` | reached zero by **merging**, not closing |
| `reverie` | all 23 open classified; **zero** already-landed, 16 genuinely pending, 2 indeterminate |
| `hermit` | sweeps found **zero** closable; several land candidates |

**Two near-misses, both caught by the residual check (check 3), both PARTIAL
rather than closable.** One had its mechanism landed, both test files present on
main, and its target manifest deleted in a format migration — every signal
saying close — and still carried 173 non-comment lines of a distinct test
scenario. The other was superseded by a commit 28 hours older and would have
regressed main, yet still held two standalone C guests absent from main.

The queue is **unlanded work, not duplicates**, and the shortage is **rebases,
not candidates**. Plan sweeps on that basis: budget for rebasing, extracting
residuals and naming successors, not for closing.
