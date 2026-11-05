# pr-landing-planner user guide

`pr-landing-planner` fuses pull-request metadata, exact fetched heads, merge
conflicts, CI evidence, holds, freshness, and semantic overlaps into an
advisory landing plan. Its outputs are deterministic for a fixed snapshot and
it performs no repository-host mutations.

{{DISTRIBUTION}}

## Start with an offline fixture

```sh
pr-landing-planner quickstart --emit-demo > demo.yaml
pr-landing-planner plan --fixture demo.yaml
pr-landing-planner graph --fixture demo.yaml
pr-landing-planner status --fixture demo.yaml
```

The emitted demo is a complete JSON/YAML fixture with pull requests, checks,
heads, changed files, conflicts, and ancestry. Fixture mode is deterministic,
network-free, and does not archive a plan unless `--archive-dir` is explicit.
Duplicate mapping keys, nonpositive or overflowing identifiers, duplicate PR
identities, negative numeric counters, and relations with self/unknown endpoints
are rejected instead of being silently normalized.
When `review_evidence_unavailable` is present on a fixture PR, its value must be
a boolean; malformed values are rejected.


Use fixture mode for evaluation, tests, saved snapshots, and integrations that
already collect repository-host data.

## Plan a live repository

```sh
pr-landing-planner plan \
  --repo OWNER/NAME \
  --base main \
  --git-dir /path/to/clone
```

`--repo` is required for live runs. The host adapter reads PR metadata with
`gh`, fetches exact base and PR heads into private local refs, and uses `git
merge-tree` to detect real conflicts. It does not check out, rebase, push,
label, refire, or merge anything.
The live PR list must be a complete JSON array. An empty successful response,
a non-object array entry, or a response that reaches the 500-item request limit
is refused because the planner cannot prove that every open pull request was
listed.


Common collection flags include:

| Flag | Meaning |
|---|---|
| `--base BRANCH` | Target branch; default `main`. |
| `--prs N,N,...` | Restrict collection to selected PR numbers. |
| `--git-dir PATH` | Local clone used for fetch and merge analysis. |
| `--remote NAME` | Remote to fetch; default `origin`. |
| `--conflict-detector merge-tree` | Exact merge conflict detection; the default. |
| `--conflict-detector file-overlap` | Faster conservative fallback. |
| `--net-wrapper PREFIX` | Optional command prefix for host commands. |
| `--gh-cmd COMMAND` | Alternate `gh` executable or wrapper. |

Content-identity checks fail closed if the fetched head differs from host
metadata or caller evidence names a different fetched head. A recorded base
that differs from the fetched base is accepted only with explicit `soft-green`
authority supplied by the consuming workspace.

## What the graph means

The planner distinguishes several relationships:

- **conflict edges** mean two exact PR heads cannot merge cleanly together;
- **ordering edges** encode an explicit stack/dependency direction;
- **file-overlap edges** are informative changed-path intersections;
- **mechanism-overlap edges** flag changes to the same recognized operational
  mechanism even when their files differ; and
- **unclassified mechanism candidates** stay visible instead of being silently
  forced into the wrong category.

Parallel-safe groups contain PRs that do not conflict under the chosen model.
Clusters retain connected conflict sets so a coordinator can reason about
stacking and avoid needless repeated rebases.

## CI classification

The required gate name is caller-configurable with `--gate-check`. Red or
apparently red states are classified before actions are chosen:

| Class | Interpretation | Typical recommendation |
|---|---|---|
| `real` | A genuine check failure. | `hold-fix` |
| `flaky` | Matches a supplied check/message signature. | `refire-ci` |
| `stale-required-check` | Underlying checks passed but the gate is stale. | `refire-stale-gate` |
| `evaluate-once-race` | The gate evaluated while required work was queued. | `wait` |
| `runner-outage` | The gate job did not actually execute. | `escalate-runner-outage` |

Supply flaky signatures as strict JSON or YAML:

```yaml
- name_regex: '^integration$'
  text_regex: 'temporary connection reset'
```

```sh
pr-landing-planner plan \
  --repo OWNER/NAME \
  --git-dir /path/to/clone \
  --flaky-signatures flaky.yaml
```

`--outage-min-prs` controls how many simultaneous outage signatures establish
a systemic outage.

## Exact-head landing context and validation authority

Some facts belong to the caller rather than the repository host: an assigned
agent, the exact head and base tested by local validation, the consuming
workspace's hard/soft-green decision, exact-head adversarial-review receipts,
and whether a PR changes gate policy.
Provide them with `--landing-context`:

```yaml
prs:
  - pr: 42
    head_sha: 0123456789abcdef0123456789abcdef01234567
    base_sha: fedcba9876543210fedcba9876543210fedcba98
    assigned_agent: release-coordinator
    validation_evidence: clean-validate-record
    validation_authority: soft-green
    review_pass_heads:
      codex: 0123456789abcdef0123456789abcdef01234567
      claude: 0123456789abcdef0123456789abcdef01234567
    review_objections_resolved: true
    review_evidence_digest: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    policy_class: ci-hygiene
```

Accepted validation evidence is `none`, `authoritative-ci`,
`locally-validated`, or `clean-validate-record`. Both local evidence values must
come from caller-supplied dereferenced records naming the exact fetched head and
base SHAs. A bare `locally-validated` label is only a cache hint and maps to
`none`; a head-only record is incomplete and is rejected. Every
`clean-validate-record` requires an explicit `validation_authority` of
`hard-green` or `soft-green`, and either authority is refused without that clean
record. The
recorded head must always equal the fetched PR head. When the recorded base no
longer equals the fetched base, only explicit `soft-green` authority permits the
planner to retain that evidence; absent authority and `hard-green` both refuse.
The generic planner does not reconstruct ancestry or landability from mutable
repository state. It accepts the consuming workspace's decision. Accepted policy classes are `unclassified`,
`ci-hygiene`, and `gate-policy`; a gate-policy change is escalated rather than
treated like routine hygiene. Unknown PRs, duplicate entries, and head drift
are errors.

`assigned_agent` and `agent:` labels are optional routing metadata, not
authority. One `agent:` label is displayed as the assignment. Multiple distinct
agent labels leave the assignment empty and do not stop planning.

`review_pass_heads` maps each required review lane (`codex` and `claude`) to the
exact 40-character lowercase head SHA that reviewer passed. Review labels are
cache hints: when the review protocol is active, each lane needs both its
`passed-review-LANE` label and an exact-head receipt. A missing receipt is
`unbound`; a different head is `stale`. Any rebase, including one with an
identical patch-id, therefore requires a bounded delta re-check and a new
exact-head receipt. The JSON node reports `review_binding` and
`review_pass_heads`; stale or incomplete bindings are held.

`review_objections_resolved: true` is caller authority that the consuming
repository checked every recorded objection and found an exact resolution
record for each one. The human or review-authority producer decides that
substance; the digest only binds its decision to the snapshot and is not proof
that an objection was resolved. The assertion requires both the exact
`head_sha` and the 64-character lowercase `review_evidence_digest` emitted as
`nodes[].review_evidence_digest` by an uncontexted exact-head JSON plan. The
producer must copy that field mechanically into its generated context after
assessing that snapshot; nobody should hand-compute it. The follow-up plan
refetches the evidence and refuses if either the head or digest differs.
A canonical `CHANGES-REQUESTED-AT` marker in an issue or inline review comment
also produces the `changes-requested` hold when GitHub supplies no native review
and an empty aggregate `reviewDecision`. The uncontexted plan keeps that hold;
only `review_objections_resolved: true` bound to the matching exact head and
snapshot digest can clear it. Missing or false context cannot make that pull
request landable.

If any promised review source cannot be fetched completely, paginated, parsed,
or assigned a stable event identity, the node reports
`review_evidence_unavailable: true` and remains held with
`review-evidence-unavailable`. This state is distinct from a complete snapshot
containing no events. It always produces `wait`, including when a base conflict
would otherwise recommend `rebase-then-land`. A clean exact validation record or
an aggregate approval cannot clear it; review evidence must be fetched later.

The authority decision does not require a `RETIRES` line. For example, an older
refusal can be discharged by a later valid withdrawal followed by a current-head
approval. The snapshot preserves their timestamps and substantive body text so
the authority can evaluate that chronology.


The digest covers the snapshot head, aggregate review decision, and the sorted
set of explicitly paginated native reviews, issue comments, and inline review
comments.
Each event contributes its source kind, stable event identity, state, available
event head (or the documented empty value for comment sources without one),
creation/submission timestamp, current version timestamp, native-review
last-edit timestamp, and body. The digest removes only a fleet disclosure on the
first nonblank line, an optional `BY` value on an exact review marker, and an
exact standalone `Unverified --who metadata: NAME` prose line. These optional
identity fields use the canonical ASCII-only `[A-Za-z0-9_.-]+` agent-token
grammar and ASCII space or tab separators. Non-ASCII whitespace is substantive,
not syntax. Identity metadata is normalized for native reviews, issue comments,
and inline review comments; it remains in the displayed comment for readers but
does not decide authority.

Review-marker recognition does not depend on the optional `BY` value. A missing,
alternate, or malformed `BY` value therefore cannot hide an underlying refusal
or withdrawal. Only a canonical value is removed from the digest; malformed
text, near-matches to the standalone metadata line, quoted or code examples, and
every other body byte remain digest input. Appending an unresolved objection
therefore changes the digest. Inline location fields are part of state, so an
outdated/retired comment also changes the digest even when GitHub gives the
change the same second-resolution timestamp.

When an exact `RETIRES` record is present, its target comment id, review lane,
reviewed head, all other body text, and repository-permission result remain in
the digest. GitHub's immutable event author is the account whose current
permission is checked; `triage`, `write`, `maintain`, or `admin` is authority. A
verified permission and that immutable author are included in the digest, so a
later permission change invalidates an existing context.

When the event author is unavailable, the permission lookup fails, or the
author has only read access, the event remains in the snapshot with no
retirement authority. It cannot by itself discharge the objection, and it does
not remove unrelated review events or abort the plan. Ordinary event authors
are optional metadata and are excluded from the digest. A missing stable event
identity or another promised source field still makes the entire snapshot
unavailable instead of dropping that event. The field suppresses only the
`changes-requested` hold from the repository host's aggregate decision or a
canonical comment refusal in that exact snapshot. It does not grant an approval,
satisfy `review_pass_heads`, or override `REVIEW_REQUIRED`; absent or false
retains the fail-closed hold.

A machine-oriented two-pass flow is therefore: run `plan --format json`
without a resolution assertion; select the desired PR's exact `head` and
`review_evidence_digest`; let the review authority assess that same snapshot;
then emit both values with `review_objections_resolved: true` and rerun. A
missing or stale digest reports this remedy and fails closed.

## Freshness, holds, priority, and batching

Being behind the fetched base is NOT by itself a reason to rebase before
landing, so the default applies no freshness reroute. Follow the consuming
workspace's own validation-authority rule, which states it with its reasoning:
"A lagging ancestor is legitimate; requiring the tip made the verdict a property
of WHEN you looked rather than of the tree." `--freshness-max-behind N` remains
available for a caller that genuinely wants a bound. A real conflict is a
separate question, decided by the conflict graph and by mergeability rather than
by this counter. A clean validation record with caller-supplied
soft-green authority survives as validation evidence. When a caller HAS set a
freshness bound and a pull request exceeds it, the planner recommends the rebase
and landing without pre-landing revalidation, while post-facto validation remains
due; with no bound set, such a pull request is simply landable. The consuming workspace remains the sole
authority for that landability decision. Draft state, missing approvals,
conflicts, ordering constraints, CI state, and policy escalation can hold a PR.
Unavailable review evidence still produces `wait` when either local merge-tree
evidence or the repository host also reports a base conflict.


Priority defaults to deterministic size and age ordering. `--priority-source
labels` reads a numeric label matching `--priority-label-pattern`; a configured
`--priority-source command` with `--priority-cmd` can supply an integer priority.
The command is required and must print exactly one signed 64-bit ASCII integer;
malformed priority configuration or output stops planning. Ties remain deterministic.

`--batch` additionally proposes one green batch containing only dependency-root
PRs that do not conflict with one another. Ordered children wait for a later
snapshot. The result is still advisory and never arms a queue.

## Output formats and archives

```sh
pr-landing-planner plan --fixture demo.yaml --format human
pr-landing-planner plan --fixture demo.yaml --format json
pr-landing-planner plan --fixture demo.yaml --format actions
```

`human` is for terminals. `json` is the complete stable machine schema.
`actions` emits one line per recommendation for simple automation. Graph,
cluster, and status views support human and JSON formats.

Live plans are archived as canonical JSON in an operating-system state
directory by default. Override it with `--archive-dir DIR` or
`PR_LANDING_PLANNER_ARCHIVE_DIR`; disable it with `--no-archive`. Archive notes
go to standard error so standard output remains pure.

## Command summary

| Command | Purpose |
|---|---|
| `plan` | Build the fused graph and recommended per-PR actions. |
| `graph` | Show only conflict and ordering structure. |
| `clusters` | Group shared conflict sets into stack-landing lanes. |
| `status` | Show per-PR CI and label health. |
| `quickstart` | Print an introduction or emit a demo fixture. |

All commands support `--help`; the top level supports `--version` and
`--userguide`.

## Safety model

Treat every recommendation as a decision aid. The collector's network and Git
operations are read-only with respect to the repository host, but live mode
does update private refs in the supplied clone and may write the local plan
archive. A separate, authorized actor must perform any refire, rebase, label,
landing, or merge.

## License

MIT
