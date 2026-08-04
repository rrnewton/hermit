# Main branch merge queue

Pull requests into `main` land through GitHub's merge queue. The queue creates
a temporary commit against the current `main` tip, preventing a stale pull
request head from bypassing changes that landed ahead of it.

The required status is `merge-gate-v2`. Its job passes when either:

- the latest `.github/workflows/ci-portable.yml` run for the exact pull request
  head completed successfully; or
- the pull request has the `locally-validated` label from a fully green
  `./validate.sh` run.

The workflow removes `locally-validated` whenever the pull request head
changes. It also re-runs the gate after CI completes and on label changes, so a
premature pending-CI failure converges without closing and reopening the pull
request. Every strip records a durable evidence comment (see
"Validation-evidence trail" below) so the record of what was validated is never
lost.

The job first verifies that its workflow file has the exact Git blob registered
in the server-side `MERGE_GATE_V2_BLOB` variable. This rejects accidental drift
that retains the guard. The context name is versioned as well: every semantic
gate tightening must bump it and move the ruleset, so an unmodified
pre-tightening branch cannot emit the context currently required by `main`.

This is not a cryptographic attestation of PR-owned YAML. A deliberate workflow
edit can delete the blob-check step while retaining the v2 job name, and both
runs use the same GitHub Actions integration. User-owned repositories cannot
use GitHub's pinned required-workflow rule, so gate-policy PRs must remain an
escalated adversarial-review class. A dedicated trusted GitHub App signer (or an
organization-owned required workflow) is needed to close that stronger threat.

Add an approved pull request to the queue with:

```bash
with-proxy gh pr merge <number> --repo rrnewton/REPOSITORY --auto --merge
```

Replace `REPOSITORY` with `hermit` or `reverie`.

## Local validation

A full green `./validate.sh` run automatically creates and applies the
`locally-validated` label to the current branch's pull request. Set
`PR_NUMBER=<number>` when branch-based detection is unavailable. GitHub CLI,
authentication, proxy, missing-PR, and label-edit failures are warnings and do
not change validation's exit status.

Use `./validate.sh --no-label-pr` or `VALIDATE_LABEL_PR=0 ./validate.sh`
when a green run must not update GitHub.

The label is an alternate merge admission signal, not a partial-test waiver.
Apply it only through a full green validator run on the exact pull request head.
The privileged workflow remains an independent bonus signal and is not a merge
admission requirement.

## Validation-evidence trail

Stripping `locally-validated` must never silently erase the record of what was
validated. Two symmetric comments preserve it:

- **Add time.** A green `./validate.sh` posts an evidence comment (commit SHA,
  profile, results, host, durable log path) ending in a machine-parseable marker
  `<!-- locally-validated-evidence sha=... -->`. This is the safety net: it
  survives even if a strip path forgets to comment.
- **Strip time.** `scripts/label-strip-evidence.sh` posts a comment recording
  the strip (validated SHA, new head, reason, timestamp) and quotes the matching
  add-time evidence comment. It is best-effort and always exits 0, so it can
  never fail a gate job or block landing.

Known strip paths — all must leave the trail:

1. **Automated on-push strip.** The `invalidate-local-validation` job in
   `.github/workflows/merge-gate.yml` deletes the label on
   `pull_request: synchronize` and then calls `label-strip-evidence.sh`.
2. **Manual agent/tooling strip.** A human or agent removing the label
   (`gh pr edit --remove-label locally-validated`, `gh api DELETE
   .../labels/locally-validated`, or a remove+add re-fire toggle) must run
   `scripts/label-strip-evidence.sh --pr <n> --validated-sha <sha> [--remove]`
   so the evidence is preserved. The `--remove` flag also strips the label.

## Repository settings

The `main` branch ruleset must:

1. require pull requests and linear history;
2. require the current versioned Merge Gate context (`merge-gate-v2`) from the
   GitHub Actions integration, with `MERGE_GATE_V2_BLOB` equal to the workflow
   blob on `main` and `MERGE_GATE_LEGACY_CONTEXT=false`;
3. require GitHub's merge queue; and
4. disallow force pushes and branch deletion.

Verify the live rule without mutating it:

```bash
with-proxy scripts/configure-merge-gate-ruleset.sh --check
```

That checker covers the versioned context, its GitHub Actions integration ID,
the bound main blob, and the disabled transition shim. It does not attest the
repository's separate merge-queue or history-protection settings.

Before landing a gate-version transition, run `--prepare <feature-ref>` to bind
the candidate blob and enable the temporary legacy-context shim. After the
workflow lands, the coordinator runs `--apply`; it binds the `main` blob,
changes only the legacy required context to v2, disables the shim, and verifies
the full resulting ruleset plus all three server-side values. The full-object
PUT is preceded by a fresh equality check, which detects policy drift already
visible before the write. GitHub exposes no conditional PUT for this endpoint,
so a narrow read-to-write TOCTOU window remains; the full post-state check
detects the resulting mismatch but does not make the update atomic. The ordered
transition is fail-safe, not a cross-resource transaction. GitHub
required-workflow rules would avoid this transition, but they are available
only to organization/enterprise rulesets; `rrnewton/hermit` is user-owned.

Enable auto-merge in the repository so `gh pr merge --auto --merge` can queue
eligible pull requests. Do not require the host-dependent CI job separately;
the gate owns the documented CI-or-local-validation policy.
