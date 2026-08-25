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

⚠️ **AND IT IS NOT A REASON TO ABSTAIN — IT IS ONE FLAG.** This paragraph
previously stopped at "nobody's fault", and that half-truth cost a night of
unnecessary abstentions: several agents, and the coordinator, told each other to
decline DBT results because a default build reports the backend unavailable.

`third-party-backends = ["dbt", "sabre", "e9patch"]` is declared in
`hermit-cli/Cargo.toml`, and the workspace manifest documents the invocation:

```console
cargo build -p hermit --features third-party-backends
```

**DBT and SaBRe are verifiable on these boxes.** "Not included in this build"
describes a build default you can change, not a capability the host lacks. Build
with the flag and measure, rather than parking the verdict — and state which
binary and which commit produced the number, because a DBT measurement from a
default build is not a measurement at all.

The genuinely unavailable thing here is narrower and unrelated: `api.github.com`
for `gh` **writes**. Reads go through `ci-hub/bin/gh-api`, and landings go by
pushing directly.

This is the only check here about the ENVIRONMENT you are assessing in rather
than about the branch or about main.

#### A PRESENT backend does not make a red the branch's either

Establishing that the backend exists closes only half of this. The reds can still
belong to the box, to the binary, or to a standing defect — and then they look
exactly like reds the branch caused, because the backend ran.

**Run the control: the same cases, the same binary, on unmodified `main`.** Only
the difference between the two runs is attributable to the branch.

```console
python3 tests/backend-parity/run_matrix.py --backend dbt --verify --hermit <dbt-capable-hermit>
git stash && python3 tests/backend-parity/run_matrix.py --backend dbt --verify --hermit <same-binary>
```

Measured on hermit#2342: DynamoRIO was present and the binary genuinely ran under
it, and the ported change produced **25 FAIL**. The control on unmodified `main`
produced **the same 25 FAIL**. The change was responsible for none of them.
Without the control the sweep would have published "this pull request turns 25
DBT cases red" — a wrong verdict reached with the backend present, which check 7
as written above would not have caught.

Note also that a DBT-capable build may exist elsewhere on the box even when the
one in your worktree reports `DBT support was not included in this build`. Probe
the binaries you have before concluding a backend is unmeasurable; state which
binary and which commit produced any number you report.

#### The feature-flag message is NOT DBT-specific, and it is nobody's fault

The same wording appears for every gated backend, so do not read it as the
manufactured-reds case. Measured 2026-08-25 on one box, two builds of the same
repository:

```
hermit/target/debug/hermit   --backend sabre  ->  WARN hermit::sabre: :: Backend:
                                                  sabre static rewriting + ptrace runtime
hermit/target/release/hermit --backend sabre  ->  Error: backend `sabre` is
                                                  unavailable: SaBRe support was
                                                  not included in this build
```

**Same host, same source, opposite answers** — the difference is the feature flag
the binary was built with, nothing else.

This is the *inverse* of the trap above and needs saying because the next reader
will meet the message and reach for the manufactured-reds explanation. Four wrong
verdicts in one night came from an absent backend being read as failure; this is
an absence with a **benign cause**, correctly attributable to **nobody** — not the
branch, not `main`, not the host.

Two consequences worth acting on:

- **`unavailable: … not included in this build` is a statement about your binary.**
  It is not evidence about the pull request and must never be recorded against it.
- **Probe every hermit binary on the box before declaring a backend unmeasurable.**
  The build that lacks the backend is often not the only one present.

### 8. Look for a policy that declares the thing intentional, before calling it a defect

⚠️ **A line read in isolation cannot tell you whether it is an oversight or a
stated decision.** Check whether something nearby declares it on purpose. If it
does, the pull request is not a fix — it is a policy change, and it carries the
obligations of one.

The case, and it cost a wrong recommendation to three agents before it was
caught:

`tests/backend-parity/run_matrix.py` invokes the matrix with
`("--verify", "--verify-allow", "both")` and no `--verify-strict`. Read alone,
that is plainly a defect: bare `--verify` is the lossy Stripped comparator and
cannot establish L2, so the whole backend-parity matrix appears to be comparing
under a comparator that cannot support the claim the matrix exists to make.
hermit#2342 adds the flag in one line, and it looks like the cheapest possible
win.

Twenty lines above, `DEFAULT_VERIFY_POLICY` says:

```python
DEFAULT_VERIFY_POLICY = VerifyPolicy.checked(
    hermit_flags=("--verify", "--verify-allow", "both"),
    expected_non_kvm_tier="stripped",
    comparison_claim=(
        "Stripped DETLOG comparison "
        "(numbers/addresses/paths normalized; NOT bitwise)"
    ),
)
```

The limitation is **declared**, in those words, including *NOT bitwise*. And
`VerifyPolicy.checked` refuses the one-line patch outright:

```python
if requests_canonical != expects_bitwise:
    raise ValueError("--verify-strict and the bitwise evidence tier must move together")
```

Applying hermit#2342's single line to main and importing the module produces
exactly that `ValueError`. The real change is three coupled edits — the flag, the
tier from `stripped` to `bitwise`, and the claim text — which flips
`assurance_label()` from *below L2* to **L2** for the entire matrix, and per
`AGENTS.md` an L2 claim must be established with `bitwise_parity: true` rather
than asserted.

**The generalisation.** A declaration next to the code is evidence about intent
that the code alone does not carry. Before filing a line as a defect, grep its
neighbourhood for a policy object, a named constant, a stated tier, or a comment
that owns the choice. Where one exists, the honest verdict is *policy change,
needs measurement*, not *one-line fix* — and the difference is the whole scope of
the work.

A well-built guard like the one above is worth noticing for its own sake: it
makes the flag and the claim it licenses move together, so no one can quietly
upgrade a comparator without upgrading the assurance claim. That is the opposite
of a gate that passes without looking.

### 9. Read the MERGE BASE, not only the diff — a draft can revert what it never touched

⚠️ **The other eight checks read the diff. This one reads the base, and it is the
only one that catches a draft reverting work its diff never mentions.**

What a merge writes is not the branch's diff. For a file both sides changed and
git cannot auto-merge, the resolution decides which side survives — and if the
branch's base predates a fix that landed in that file, taking the branch's side
silently reverts it. **Nothing in the diff shows this.** The reviewer reads a
change about one subject and lands a revert of another.

The case:

hermit#2426 is a compat-envelope draft. Asked what it does with a first-verify
run that produced no output, the answer is *nothing* — the only first-run
language anywhere in its diff is an unrelated fixture string,
`collect2: error: ld returned 1 exit status`. By diff, it is irrelevant to that
subject.

But its merge base is `770b95c505`, before the typed `no_result` /
`guest_exit_code` / `guest_signal` reporting landed. Counting those symbols:

| file | main | the draft |
| --- | --- | --- |
| `scripts/validate.rs` | **65** | 7 |
| `scripts/lib/validate_runtime.rs` | **2** | **0** |

and **both files are in that draft's conflict set.** A take-the-branch resolution
drops 58 matching lines from one and all of them from the other, undoing the
change that stopped a zero-byte first run being silent — a result reached only
after eight candidates were ruled out.

**The precise hazard, because "old base" alone is not it.** A rebase or
cherry-pick replays only the branch's changes, so it cannot revert content the
branch never touched. The danger is specifically **a conflicted file resolved
toward the branch**. So the test is not "is the base old" but:

> Does this draft **conflict** in a file that has gained something since its base?

**What to do.** Rebase rather than merge, so conflicts surface hunk by hunk
instead of as one take-a-side. Then assert the survivors mechanically, against
the merged tree rather than the intent:

```console
git grep -c -E 'guest_exit_code|no_result' -- scripts/validate.rs scripts/lib/validate_runtime.rs
# must not fall below main's counts
```

**Why this is acute right now.** Every draft in the queue was written before
tonight, and the typed no-result reason, the capture fix, the KVM comparator fix,
the RNG determinism fix and the format gate all landed in the last hours. Any of
them can be reverted by a draft whose diff looks unrelated. Pick the symbols that
matter for the files in the conflict set, and count them before and after.

### 10. File-identity alone establishes NEITHER "absent" NOR "no residual"

Counting files where `main` matches the pull request's head answers one narrow
question and is routinely mistaken for two others. Use the **three-way** split:

| bucket | test | means |
|---|---|---|
| **arrived** | `main` blob == PR blob | that file's content is on `main` |
| **absent** | `main` blob == MERGE-BASE blob | `main` never moved; definitely not landed |
| **moved on** | matches neither | undecidable from blobs; `main` went somewhere else |

**`absent == 0` is the close test, not `arrived == all`.** A branch far behind
`main` will show few arrivals and many moved-on files while being entirely
superseded, because `main` built past it.

Measured on real rows:

- `#2420` and `#2398`, both closed as already-landed: **arrived == all,
  absent 0, moved-on 0.** Every changed file byte-identical, corroborated by
  matching per-file `numstat` from the same base.
- `#2436`: **arrived 1 of 24, absent 0, moved-on 23.** A file-identity read of
  "1 of 24 identical" was published as evidence it was PARTIAL and needed
  residual extraction. That was wrong. `absent 0` plus 41 of 54 markers landed
  and **zero** both-absent made it `supersede-and-regress`, and `main` had moved
  two to nine times further in every substantive source file.

And "no residual" is a separate question that file-identity cannot answer at
all. On `#2405` a close was published with *"residual: none"* after comparing
the headline constant and its call site; the descriptor-cleanup path in the
**error branch** was absent from `main` and was missed. Diff the error paths,
and check distinctive string literals as well as symbols — on `#2436` that meant
210 literals grepped against `main`, of which 0 were absent.

### 11. What the change DEPENDS ON, not only what it changes

Checks 1 to 3 ask what a change contains and whether `main` already has it. This
one asks the opposite question about the residual you decided to extract: **does
the piece you are lifting out still work once separated from the piece you left
behind?**

A salvage branch is usually assessed file by file, and extraction is proposed the
same way — take the good test, drop the superseded code. That is exactly where
this fails, because **a test and the change that makes it pass are one unit even
when they sit in different files.**

Measured on [hermit#2339](https://github.com/rrnewton/hermit/pull/2339). Its
residual contained a genuinely valuable test strengthening: `main` already asserts
*exactly one* `scheduler-empty` and *exactly one* `fallback-completed`, and the
branch adds the same for `SCHEDULER_FIZZLE` and `BACKEND_EVIDENCE` plus a four-way
ordering assertion. Clean, self-contained, 13 lines, an obvious extract.

**It is not extractable alone.** The exactly-once-fizzle assertion is true only
because of the branch's *other* change — relocating the empty-system diagnostic
out of `step2_process_blocked` to the scheduler loop's single terminal exit. That
code change is superseded (`main` resolved the same double-emission by keeping the
`step2` site instead) and should NOT be ported. Lift the test out on its own and
you assert a property whose cause you deliberately left behind.

The failure mode is quiet: the extracted piece reviews well, merges cleanly, and
reds the suite on a claim nobody connected to the omitted half.

So before extracting a residual, ask:

- **Does this assert or rely on behaviour the SAME BRANCH introduced elsewhere?**
  Grep the branch's other files for the symbol, log string, or constant the piece
  keys on. A test naming `SCHEDULER_FIZZLE` wants whatever emits it.
- **Is the thing it depends on part of what you are dropping?** If yes, the
  residual is not two items, it is one item with a decision attached.
- **Can you establish the dependency holds on `main` as-is?** If you cannot
  measure it, say so rather than extracting hopefully. On #2339 that question was
  left open honestly: `/bin/true` under SaBRe never reaches the fizzle path, so
  settling it needs the test's own example guest.

This is check 8's cousin. Check 8 stops you calling an intentional line a defect;
this stops you lifting a correct line away from the thing that makes it correct.

Record the coupling in the disposition. "Extract the test" is not a disposition;
"extract the test **once the emission-site question is settled**" is.

### 12. A comparison over nothing is not agreement

Checks 1 to 11 ask whether a change is present. This one asks whether the
EVIDENCE a change relies on can fail at all.

**A zero-record comparison reads as agreement having observed nothing.** Two runs
that each produced no records compare equal. Nothing in the verdict distinguishes
"these agreed" from "there was nothing to disagree about", and the second is not
a result.

This is the shape behind most of what tonight's sweeps actually found, across
parts of the system that share no code:

- **Heap DETLOG under DBT.** The kernel labels `[heap]` only for
  `[mm->start_brk, mm->brk)`. Under DBT the guest's heap is an unlabelled
  anonymous mapping, so the heap comparison had no records to compare and
  reported agreement. Nothing was wrong and nothing had been checked.
- **`no_result` as a verdict.** `--verify-json` pre-writes
  `verdict: "no_result"` at invocation. A run that dies before comparison leaves
  exactly that, and it reads as an outcome rather than as "never reached
  comparison".
- **Backend-parity fixtures that returned 0 unconditionally.** A fixture counted
  its checks, printed the tally, and exited 0 whatever the tally reached. Under
  `--verify` both runs lower the number identically, the comparison matches, and
  the cell stays green having verified nothing.
- **A test that skipped silently.** An integration test returned early when its
  backend artifacts were absent, reporting `ok` in 0.00s having executed nothing.

**The check.** For any claim a pull request makes about evidence, ask what the
evidence looks like when the mechanism is ABSENT rather than merely broken. If
absent and correct produce the same output, the evidence cannot support the
claim.

Three ways to make it able to fail, in decreasing order of strength:

1. **Count what was compared, and refuse zero.** A comparison that comes back
   empty should say so rather than pass.
2. **Assert the mechanism is reachable.** A positive control — one record you
   know must be present — turns a silent zero into a failure.
3. **Say what was skipped, out loud.** If a check cannot run, it must print why.
   A skip that prints nothing is indistinguishable from a pass.

⚠️ AND THIS APPLIES TO THE SWEEP'S OWN CHECKS. A symbol grep that matches nothing
on both sides (check 6) is the same defect wearing sweep clothing: it reports no
difference because it looked at nothing. Check 6 exists because that happened.

## Verdict vocabulary

| verdict | meaning | action |
|---|---|---|
| **genuinely pending** | absent by content, mechanism absent, no residual question | land it |
| **already landed** | mechanism present on main AND no residual content | close, citing the landing commit |
| **partial** | mechanism landed, unique content remains | extract the residual; do NOT close |
| **indeterminate** | rule cannot decide — no extractable symbols, or every symbol pre-existed | human read; do NOT guess |
| **blocked on a cross-repo dependency** | content and mechanism both absent, but the capability its tests assert has not landed in `reverie` or `agent-utils`, or hermit's pin does not include it | LEAVE OPEN and NAME THE SUCCESSOR; landing it creates a red |
| **blocked on a defect** | genuinely pending and wanted, and part of its claim is VERIFIED, but landing would make a gate ASSERT something a defect elsewhere makes measurably false | LEAVE OPEN and NAME THE DEFECT with its smallest reproduction; record which part of the claim is verified and which is not |
| **supersede-and-regress** | landing would weaken newer invariants, remove a current dependency, or rely on a prerequisite that is now false | do NOT port; extract any residual, then close citing the newer mechanism or dependency |

Docs-only and config-only pull requests add no symbols and land in
**indeterminate** by construction. That is a correct verdict, not a failure of
the check, and those rows should be collected for one batched decision rather
than guessed at individually.

## Four dispositions, not two

A sweep that can only close or land will record the other outcomes as nothing,
and **four supersession chains dead-ended that way in one night** — each one a
pull request whose real status was known by somebody and written down nowhere.

| disposition | when | what to record |
|---|---|---|
| **close** | already landed, or abandoned and unsafe to port | the superseding commit, or why porting is unsafe |
| **land** | genuinely pending and landing improves main | the usual receipt |
| **correctly parked, with a NAMED SUCCESSOR** | real work, correctly blocked — a cross-repo dependency, an unlanded capability, a decision it waits on | **WHO or WHAT unblocks it**, by name |
| **blocked on a NAMED DEFECT** | real work, correctly blocked by something BROKEN rather than something pending | **the defect, reduced to its smallest reproduction**, plus which part of the claim is already verified |

The third is not a failure to decide. It is a decision, and it is only useful if
the successor is named: "waiting on reverie#430 to land and hermit's pin to
advance" is a disposition; "still open" is not. A parked pull request with no
named successor is indistinguishable from a forgotten one, which is exactly how
the four chains were lost.

### The fourth: blocked on a defect, which is not the same as parked

Parking waits for something **pending**. This waits for something **broken**, and
the difference decides who can act. A parked row has a successor to watch. A
defect-blocked row has nobody watching anything until the defect is written down,
so the reproduction IS the disposition — without it the row is indistinguishable
from "the author gave up".

It is also not **supersede-and-regress**. There the port is wrong and should be
abandoned. Here the port is RIGHT and wanted; it simply cannot be asserted yet,
because a gate would state as true something that is measurably false.

The distinguishing question: **would landing make a check CLAIM something?** A
change that only alters behaviour can be judged on behaviour. A change that
alters what a gate ASSERTS must be judged against whether the assertion holds on
every backend or configuration it will now speak for.

Measured, on [hermit#2342](https://github.com/rrnewton/hermit/pull/2342), which
flips the backend-parity matrix from a `stripped` DETLOG comparison to canonical
`--verify-strict` / `bitwise`:

- **Verified for ptrace:** 28/28 cases reached bitwise, exit 0. The claim is true
  where it could be measured.
- **False for DBT, and not because of the pull request:** all 25 DBT cases failed
  — *identically, with and without the change*. The two DBT logs were
  byte-identical once large integers were normalised; the only difference was the
  thread id. DBT emits the **host TID** where ptrace emits a virtualized
  `DetPid` (`dettid 1163782` vs `1163891` across two runs of `/bin/true`, against
  `dettid 3` twice under ptrace), so a DBT DETLOG can never match across runs
  under *either* policy.
- **Why that blocks landing:** `expected_non_kvm_tier` is one value for all
  non-KVM backends, so landing puts "requested policy is L2" in the runner's own
  banner for a backend measured unable to deliver it.

Recording that as "still open" would have lost it. Recording it as
`dbt-pid-virtualization`, reduced to a single-threaded `/bin/true`, makes it
actionable by someone who never read the pull request.

**Name the defect, not the symptom.** "DBT parity fails" is a symptom that
invites a rebase. "DBT emits the host TID instead of a virtualized DetPid" is a
defect somebody can fix.

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
