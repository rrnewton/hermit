---
name: post-facto-review
description: "Current Hermit post-facto human-review protocol: exact trigger set, dual Claude+Codex adversarial review for triggered changes, exact-head validation, and fix-forward human review after landing."
---

# Post-facto human review

Every bot-authored PR description or comment starts with the applicable
`[impl agent, MODEL]`, `[adversarial-reviewer agent, MODEL]`, or
`[coordinator, MODEL]` tag. Human comments use `[Human]`.

Apply `post-facto-human-review` if and only if the PR has one of these triggers:

1. New syscall support, with `AUTONOMOUS-BOT-IMPLEMENTED` at the new dispatch or
   classification entry and `TODO-HUMAN-REVIEW(PR-id)` at the implementation or
   determinization block.
2. A Reverie `Tool`, `Guest`, `Backend`, syscall-interception, or other core API
   abstraction change.
3. A new determinization strategy, not routine implementation of an established
   one.
4. A core DetCore scheduling change affecting how programs are scheduled,
   especially race search.

Routine parity work toward the ptrace reference is not a trigger by itself.
The label routes after-the-fact human review and never waits for human approval
before landing. Never apply `pre-land-human-review`, alter `human-approved`, or
recreate obsolete review labels under the current owner directive.

## Adversarial review and evidence

A triggered PR requires independent exact-head approval from one Claude-family
reviewer and one Codex-family reviewer. Neither is the author. Role-tagged review
comments carrying the full head SHA are authority; numbered review and
`passed-review-*` labels are caches. Any push invalidates both approvals.

Every PR contains `Summary`, `Determinism`, `Linux Semantics`, and `Validation`.
KVM changes also contain `Relationship to gVisor`; a triggered PR contains
`Human Review Required` naming the numbered triggers. A determinism proof
explains the model, not only tests.

Strict verification compares exit status, stdout, stderr, and complete INFO
logs byte-for-byte. Do not strip numbers, addresses, branch counts, virtual-time
values, or durations to obtain equality. First-sample agreement is not proof of
a continuously evolving clock.

## Landing

Inside dev-hermit, the parent `AGENTS.md` and ci-hub executable are canonical:
the exact current Hermit head needs a clean, counted, full-profile receipt
accepted by `ci-hub validate-status`. A `locally-validated` label, command exit,
or comment is only a cache. GitHub checks are supplemental; a genuine failure
they reveal still blocks. A standalone checkout follows its current
repository-defined exact-head authority.

Land only when the task authorizes it, required adversarial review is resolved,
and the semantic verifier accepts the current head. Use the serialized tracked
landing path, never `--admin`, then fetch main and prove ancestry. Human review
happens after landing and corrections fix forward.
