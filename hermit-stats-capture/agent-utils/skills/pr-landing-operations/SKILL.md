---
name: pr-landing-operations
description: Validate and execute an authorized PR landing plan with exact-revision evidence, serialized publication, and ancestry proof. Use after pr-landing-planner emits a plan and only within the consuming repository's rules.
---

# PR landing operations

The [PR landing planner](../pr-landing-planner/SKILL.md) emits an advisory plan and never mutates a
PR. This skill describes the execution discipline after the consuming repository has authorized the
work. Its repository rules remain authoritative for required checks, reviews, merge methods, branch
protection, and who may publish.

## Bind evidence to the final revision

Fetch the target branch before choosing a wave. Rebase or otherwise finalize each candidate first,
then validate and review that candidate. Review remains exact-revision;
validation identity and preservation across a rebase follow the consuming
workspace's own validation-authority skill.

Apply the existing validation rule: "A lagging ancestor is legitimate;
requiring the tip made the verdict a property of WHEN you looked rather than of
the tree." Use the consuming workspace's explicit `hard-green` or `soft-green`
decision; do not infer landability from the live base tip.

Classify validation from named checks and their recorded outcomes. An aggregate pass count or label
does not prove coverage unless repository policy says it does. Likewise, an approval of an older head
is not approval of the current head.

## Choose a landing shape

Use serial landing unless the repository explicitly permits a coalesced staging branch. For an
authorized coalesced landing:

1. Fetch and create staging from the current target branch.
2. Screen the constituent list before combining anything. Ready and conflict-free say nothing about
   whether a change was DELIBERATELY REFUSED, and a wave's constituents are usually listed by hand,
   so a closed-on-the-merits change folded into staging lands under the wave's single approval.
   Refuse any constituent whose pull request was closed without naming a successor that carries the
   work. Treat an unannotated close as REFUSED, not as permission: record why it was closed, then
   re-screen. Closed alone is not the predicate — after a wave lands, its own constituents are
   normally closed, so refusing every closed pull request would block the routine case and teach
   operators to skip the check.
3. Combine only ready, conflict-free heads; return conflicts for individual resolution and review.
4. Validate and review the exact combined staging head.
5. Publish through the repository's normal protection and authorization path.

If the target branch moves, refresh the candidate and repeat revision-bound checks. Do not claim an
older receipt for a new head.

## Serialize writers and prove publication

Hold the repository's publication lock, if any, and allow one writer at a time. Fetch immediately
before publishing. Never force-push, bypass branch protection, or use an administrative override
unless the repository rules explicitly require and authorize that exact action.

After publication, fetch again and prove ancestry against the named target:

```sh
git merge-base --is-ancestor <landed-revision> refs/remotes/origin/<target-branch>
```

Exit status zero proves ancestry; a successful merge command, UI state, or clean dry run does not.
Close or mark constituent work landed only when its revision, or the repository's documented
equivalent, is proven present on the target branch.

Use `pr-landing-planner --help` and `pr-landing-planner --userguide` for planner inputs and evidence
classes. Re-read the consuming repository's rules immediately before any mutation.
