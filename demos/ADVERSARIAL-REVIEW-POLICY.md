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
Demo-Green-Review: reviewer=<agent-id> demo=<demos/path|all> result=GREEN evidence=<url|path|sha>
```

- `reviewer=` — the reviewing agent, which **should differ from the implementer**
  (independence; the review is adversarial).
- `demo=` — which demo was run (or `all`).
- `result=GREEN` — the demo reached its success state (e.g. demo5 boots to the
  serial shell and exits rc=0). Anything other than GREEN does not satisfy the gate.
- `evidence=` — a link/path/SHA to the run log or artifact.

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
