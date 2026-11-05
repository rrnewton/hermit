# Agent Instructions

## Publication Policy

Routine changes to this repository go directly to `rrnewton/agent-utils:main`.
Do not open a pull request merely out of habit: a PR queue is an unowned wait
state, and this repository must remain linear and current for its consumers.

**Use ordinary git.** There is no wrapper, no publication script, and no
repository-installed hook standing between you and `main`:

```bash
git pull --rebase origin main
git push origin main
```

If the push is rejected as non-fast-forward, someone landed while you were
working: `git pull --rebase` and push again. Never force-push, never rewind,
never amend shared history, and never reach for `--no-verify` to get past a
refusal.

**Agents are authorized to use networked `git` and `gh` through Herdr.** If the
current process cannot reach GitHub, run them in a Herdr command pane:

```bash
herdr pane run <pane-id> "with-proxy git pull --rebase origin main"
herdr pane run <pane-id> "with-proxy gh pr list"
```

**Keep `main` linear.** Rebase; do not merge. `git pull --rebase` (not a bare
`git pull`) is the habit that makes this automatic — a merge commit on `main`
is rejected by the server, so a bare pull will simply waste your time.

### Protection lives on GitHub, not in this checkout

`main` is protected by a repository **ruleset** — `main history protection`,
active on the default branch with no bypass actors — enforcing:

| Rule | Effect |
|---|---|
| `deletion` | `main` cannot be deleted |
| `non_fast_forward` | no force-push, no rewind |
| `required_linear_history` | no merge commits |

That is server-side, so it holds on **every** path — every clone, every host,
every agent, the web UI, and the REST API alike. This is deliberately different
from the client-side guard that used to live here: a hook only binds checkouts
that installed it, so it gave the appearance of enforcement while leaving the
web UI, the API, a fresh clone, and `--no-verify` wide open. Real protection
belongs where it cannot be opted out of.

There is deliberately **no** `required_status_checks` rule, because that would
block the direct-to-main workflow this repository is built around. CI is a
signal for a human, not a gate.

### Validate before you push

Direct-to-main does not mean unvalidated-to-main. Before every push:

```bash
make validate
```

**Do not pipe it.** `make validate | tail` reports *tail's* exit status, not the
validator's, so a red run reads as green. That has happened twice, and the
second time it put an unformatted commit on `main`. Run it bare, or redirect to
a file and check `$?` — never through a pipe.

That runs **only the checks your change can actually affect**, and prints the
rest as skipped, by name, with the reason. A change confined to `vibe-talk/`
runs the vibe-talk suite plus the always-on repository hygiene; it skips the
unrelated workspace build, cross-language differential, and packaging smoke
tests because those checks cannot observe that edit.

Broad path routing lives in `scripts/validate.py`; tool ownership and reverse
dependencies live in `validation/components.json`. Both are checked by
`make check`, including fail-closed inventory tests:

- **An unclassified path selects everything.** A new top-level directory cannot
  silently opt out of validation.
- **A change spanning two areas is the union**, never the cheaper of the two.

Use `make validate-all` for the entire portable, current-toolchain contract
regardless of what changed. That is what a release path should use, and what to
fall back on the moment you are unsure the selection is right — selection is a
convenience for the edit-run loop, not a new definition of "validated".

Compatibility checks that inherently require a different compiler version, and
artifact checks that inherently require a container runtime or deployment
environment, remain focused CI jobs. They supplement this portable contract;
they must not duplicate its ordinary build, test, or smoke commands.

Both selected and full validation load `validation.dag.yaml`. Its strict
compile-time includes flatten the component fragments into one graph, so one
scheduler can overlap independent Python, Rust, cross-language, packaging,
browser, and vibe-talk work while enforcing shared resource caps. The graph is
the source of truth for portable checks; do not recreate its command list in
this document or in a second CI workflow.

Inspect the plan without running it with:

```bash
python3 scripts/validate.py --list
common/bin/dagrun list --dag validation.dag.yaml
```

### When a PR is warranted

A PR is allowed only for one of these explicit reasons:

1. a genuinely high-risk change needs pre-main review
   (`Exception-Reason: high-risk-preland-review`); or
2. the change must be reviewed atomically with an in-flight consumer change
   (`Exception-Reason: atomic-consumer-change`).

Record that reason on the PR **as its own line** in the description, using the
exact slug above — prose that merely mentions the reason is not a recorded
reason, because nothing can read it. Keep at most one exceptional PR open, and
land or close it before opening another.

## Repository Boundary

Everything above describes **this repository** and nothing else.

Other repositories have their own rules, and several of them are stricter than
this one — a protected branch, a required review, a required check, a merge
queue. Do not carry this repository's direct-to-main habit across a boundary
into one of those. Read that repository's own guide and follow it, and never
use an admin bypass, a force-push, or a direct push to reinterpret a policy you
found here as permission to skip one you found there.

The converse holds too: a stricter policy encountered elsewhere is not a reason
to start opening pull requests here.

### Never name a consuming project

`agent-utils` is a reusable collection of libraries. Other repositories
submodule it and use it — they are its **clients**, not its subject. Their names
do not belong anywhere in this tree, including comments, tests, fixtures, and
dated records in `ai_docs/` and `reviews/`.

`make validate` enforces this on every change, whatever the paths, via
`scripts/check_client_names.py`.

When you need to explain *why* code behaves the way it does, describe the
behaviour rather than naming the client — "the originating validator", "the
fully-censored case". The reason is the useful part; the name is the part a
reader cannot resolve. Where a record genuinely needs to distinguish two
consumers, use neutral labels (`consumer-a`, `consumer-b`) and say in the
document that they are redactions.

Do not resolve this by rewriting history silently. If you redact a dated record,
**change any note that claims the record is unmodified** — a provenance note
that promises "kept as written" beside text you have edited is worse than either
the name or the edit.

## Naming: every feature gets a slug

Every issue and pull request names its subject with a **slug**: lowercase
letters, digits, and hyphens, nothing else. No spaces, no underscores, no
capitals, typically two to four words.

    voice-page-safe-area
    elevenlabs-mock
    container-lifecycle
    launcher-preflight

**Put the slug first in the title**, then a plain sentence saying what is
wanted:

    voice-page-safe-area: honour the notch inset on a physical phone
    elevenlabs-mock: a local agent that really calls the MCP tools

**One slug per feature, and it does not change.** The issue, the branch, the
pull request, and any follow-up issue all reuse the slug it was given first.
That is the whole point: the slug, not the number, is what a human recognises
months later, and it is what makes two references to the same work visibly the
same work.

### Always say the number AND the slug

When writing to the owner, in a comment, or in a commit body, refer to work as
`#<number> <slug>` — both, every time:

    #123 voice-page-safe-area

Not `#123` on its own, not `PR 123`, not `pr123`, and not "the hover issue".
A bare number is unreadable without opening it, and a bare description cannot
be looked up. The pair is short enough that there is no reason to abbreviate
it, and it survives being quoted out of context — which is exactly where the
reference is most needed and least recoverable.

This applies to chat with the owner as much as to anything written down. An
agent that reports "landed the fix for 123" has told the reader nothing they
can act on without a round trip.

### What this does not change

Commit **titles** keep the existing `<project>: <sentence>` form —
`vibe-talk: make /voice read as an application`. They are not slugged, because
the project prefix already does that job and commit titles should stay
readable as prose. When a commit closes or advances an issue, name it in the
**body** as `#<number> <slug>`, alongside the agent identifier required by
"Say who is speaking" below.

## Say who is speaking

**Prefix every message you publish externally with an identifier saying it came
from an agent**, and which one:

    [opus 5] the assistant is reading the channel again; the tool list was stale

"Publish externally" means anything that leaves this conversation and is read
later by someone who was not in it:

- GitHub issue and pull-request comments
- pull-request descriptions
- **commit message bodies**

It does **not** apply to commit or pull-request titles, which should stay clean.
Put the identifier in the commit body or pull-request description instead. It
also does not apply to what you say to the owner in conversation — he knows who
he is talking to.

The owner sometimes prefixes his own messages with `[human]`. The point is that
anyone reading a thread later can tell at a glance which voices are people and
which are agents, without inferring it from tone. That distinction matters most
in exactly the places it is easiest to lose — a long issue thread months later,
or a commit body quoted out of context.

Use the model you actually are, not a generic label.

## Close what you finish

If work is delivered, **close the issue**. Do not leave it open with a comment
saying it is done, waiting for someone to notice.

If part of it is delivered, say precisely what remains and leave it open for
that reason — not as a way of avoiding the decision. "Substantially complete"
on an open issue is not a status; it is a deferral.

If something is delivered but unverified in some respect, close it and say what
is unverified. A new problem found later is a new issue, not evidence that the
old one should have stayed open.

## Documentation

`common/docs/` is for settled, user-facing documentation. Put working designs,
proposals, and other temporary research under `ai_docs/transient/` in dated files
or dated directories. Do not present an unimplemented interface as a shipped tool.

Every top-level tool needs a `RELATED_WORK.md` comparing relevant public
open-source systems. Keep private organizational sources and deployment details
out of that comparison.

Introduce tools by explaining the user's problem, the useful outcome, and the
dependencies. The primary operator documentation belongs in CLI `quickstart` and
`userguide` commands, plus top-level and per-subcommand help. CLI help must describe
every argument, including relevant defaults, units, and command applicability,
and include useful examples. Generated implementation/API documentation is a
separate reference.
