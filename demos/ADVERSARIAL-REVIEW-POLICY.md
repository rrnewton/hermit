# Demos: mandatory adversarial green-demo review

**Policy (owner, P1, 2026-08-01).** Every change that touches a runnable demo
(`demos/**`) must **mechanically** receive at least one **adversarial review that
verifies the demo still runs GREEN** — *independent* of whether the change is also
tagged `post-facto-human-review`. Code-review alone is not sufficient; the reviewer
must actually run the demo.

**Why.** An earlier change flipped `demos/05-qemu-boot.py` from
`--max-timeslice 2000000000` to `--no-rcb-time --max-timeslice disabled` and
landed **without anyone running the demo** — silently wedging demo5 for several
days. A path-filtered mechanical gate prevents a repeat.

## The attestation

An adversarial reviewer who has **run the touched demo(s) to a GREEN result**
records a commit-message trailer (in any commit of the PR / landing commit):

```
Demo-Green-Review: reviewer=<agent-id> demo=<demos/path[,demos/path...]|all> result=GREEN evidence=<url|path|sha>
```

- `reviewer=` — the reviewing agent, which **must differ from the implementer**
  named by the commit's `role=impl` disclosure (independence; the review is
  adversarial). A trailer cannot turn the implementer's own run into an
  independent review.
- `demo=` — which demo was run (or `all`).
- `result=GREEN` — the demo reached its success state (e.g. demo5 boots to the
  serial shell and exits rc=0). Anything other than GREEN does not satisfy the gate.
- `evidence=` — a link/path/SHA to the run log or artifact.

A `result=GREEN` trailer is invalid when the same commit body reports a non-green
mechanical result such as `PARTIAL` or `FAILURE` for a demo covered by that
trailer. Deliberate failing checks remain compatible with a later reported
successful real run; the checker does not treat the presence of a negative
control as a failed review. "Later" is read literally: the **last** result the
body reports for a demo is the one that counts, so a green followed by a real
failure is still a contradiction.

Only a line that begins `Demo <number>` (optionally behind `===`) and contains a
colon is examined at all, and on such a line only a **result field** can be a
result: the first token after a `:` or a `,`, or the last token of the label
before the first colon. So a second colon does not hide the result —
`=== Demo 3: Chaos Concurrency Testing: FAILURE (exit 1) ===` is a failure, and
`Demo 07 failed: <detail>` is a failure — while ordinary prose on such a line is
not a result. That second half matters as much as the first: this repository's
own `demo08: <subject>` commit-subject convention must not read as a demo-8
failure, and a sentence like `Demo 08: the calibration pass is unchanged` must
not read as a green that cancels a real failure banner above it.

Within one line, a non-green field beats a green field, so
`FAILURE — 1 demo(s) failed, 7 passed` is a failure.

The suite-level aggregate `demos/run-all.sh` emits, such as
`=== Demo suite: FAILURE — 1 demo(s) failed, ... ===`, contradicts a `demo=all`
trailer. It is deliberately not attributed to any individual demo: the aggregate
reports that something failed without reporting which, so a trailer naming one
exact path is not contradicted by it.

## Enforcement (mechanical)

1. **git commit-msg hook** — `.githooks/commit-msg` (install: `scripts/setup-hooks.sh`)
   blocks a local demo-touching commit lacking the trailer. Because the adversarial
   review is normally performed *after* the implementer commits, a pre-review WIP
   commit may set `HERMIT_DEMO_REVIEW_OVERRIDE=1` — but the lander still blocks the
   merge until the attestation exists.
2. **Lander** — the landing agent must run
   `scripts/check-demo-review.sh --range <base>..<head>` (exit 0 required) before
   merging a demo-touching PR, in addition to the normal gates.

`.github/workflows/demo-review-gate.yml` runs the same range check automatically
only for pushes to `integration`, or when an investigator dispatches it. It is
supplemental evidence, not an automatic pull-request or `main` trigger and not a
required GitHub check.

## Scope

`demos/**` **except** `*.md` (docs) and `demos/**/ignored/` (scratch) — those
cannot change a demo's runtime green-ness. Widen `demo_touched()` in
`scripts/check-demo-review.sh` if stricter coverage is wanted.
