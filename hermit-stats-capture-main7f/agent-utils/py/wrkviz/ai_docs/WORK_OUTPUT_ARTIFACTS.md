# Work-output artifacts and site identity

> **Provenance.** This is a dated investigation record. It was produced against a private
> downstream workspace, so it refers to repositories, hosts and services outside this one
> that cannot be resolved from here. Projects that consume `agent-utils` appear under
> neutral labels — `consumer-a`, `consumer-b` — and those are REDACTIONS, not real names:
> `agent-utils` is a reusable library and has to read standalone, without naming whoever
> happens to use it. **The measurements, dates and findings are unchanged**; only names
> were replaced. Nothing here describes `agent-utils` itself.
> See `#86 scrub-client-names` and `#67 standalone-repo`.

## Decision

Treat work outputs as a provider-neutral, evidence-backed catalog produced during
ingest, not as URLs invented by the summarizer. A summary may select artifact IDs
from that catalog, but it may never create an ID or URL. This gives the UI stable
links, makes daily and larger rollups a deterministic union, and lets artifact
extraction improve without spending tokens to rewrite otherwise-good summaries.

Run extraction while the normalized `TeamData` still contains tool inputs and
outputs. The current pipeline deliberately clears both fields in `_archive_team()`
before writing `raw/team.json`; the ignored source snapshots remain authoritative,
but reparsing them later is provider-specific and unnecessarily expensive.

The first useful implementation should be conservative. Missing a link leaves the
transcript available for inspection; a confidently wrong link can attribute work to
the wrong repository or expose private data.

## What the current data can establish

Evidence should be ranked, rather than collapsed into one regex result:

1. A hosting API or remote VCS query confirms a canonical object. This is the
   strongest evidence that an artifact exists and is reachable, but enrichment must
   remain optional and cached.
2. A successful tool result returns a canonical URL or full commit ID from a
   recognized creation/publish command. Examples include `gh pr create`,
   `gh issue create`, `gh gist create`, a Phabricator submit command, and an upload
   command that returns its destination URL.
3. A paired command and successful result establish a local output. A successful
   `git commit` plus a full `git rev-parse HEAD` can establish a commit; a successful
   `git push --porcelain` can establish publication of refs. These are distinct
   facts: an unpushed commit should not receive a forge URL.
4. Provider session metadata establishes repository context. It is especially useful
   for resolving qualified references and selecting the site's primary project.
5. An explicit URL in a user/agent message proves that the object was *mentioned*,
   not that this agent created it. A model claim such as "opened #123" is supporting
   evidence only. A naked `#123` is not evidence of either an object kind or a
   repository.

An audit of the current test archives supports this order:

- Codex records root `cwd`, launch commit, branch, and repository URL in
  `session_meta.git`. Its current logs route shell work through `exec` JavaScript
  wrappers, with a call ID and corresponding output. A deliberately broad candidate
  scan found 76 command bodies mentioning `git commit`, 27 mentioning `git push`,
  and 12 mentioning `gh pr create`; these are candidates, not 115 proven outputs.
- Claude records `cwd` and branch on transcript records and has structured `Bash`
  tool-use/tool-result pairs. The sample has hundreds of Bash calls and four
  Phabricator-submit candidates, but its session header does not identify a remote
  repository.
- Orc stores directly paired `code_input`, `code_output`, exit code, agent/session,
  and timestamp in SQLite. In the selected first-day window, a broad scan finds four
  `git push` and two issue-create candidates, all with zero exit status. Orc's session
  name is project-like but is neither a repository URL nor a source-host identity.

These counts also show why substring matches cannot be called artifacts: commands
may contain examples, heredocs, comments, quoted scripts, retries, or several shell
operations, and an exit status may belong to a wrapper rather than the operation of
interest.

## Provider-neutral records

Keep discovery records separate from generated timeline JSON. Suggested durable
files are `teams/<team>/raw/projects.json`, `raw/hosts.json`,
`raw/work-artifacts.json`, and `raw/artifact-evidence.json`. Derived phase and rollup
links belong under `summary_data/artifact_links/`.

### Project

A project is a linkable code or work-management namespace, usually a Git repository:

```text
project_id           stable hash of normalized kind + canonical URL
kind                 git_repository | work_tracker | artifact_store
canonical_url        credential-free HTTPS URL, when known
display_name         normally owner/name or the repository leaf
role                 primary | secondary | submodule | unknown
parent_project_id    optional submodule/monorepo relationship
remote_aliases       other normalized remote URLs
evidence_ids         why this project is in the catalog
```

Do not publish observed local paths in the website. They can be retained in a
private provenance record when needed to resolve `git -C`, tool `workdir`, or a
leading `cd` to a project.

### Source host

A source host is the machine where the recorded team ran, not the ingest machine and
not `github.com`. Store a user-facing alias plus its provenance. Imported backups
must accept explicit metadata such as `--source-host devbig014`; guessing from a
backup-directory name is not reliable. Provider metadata may supply a host when it
actually records one. Never substitute `socket.gethostname()` while importing a
snapshot from another machine.

### Artifact

```text
artifact_id          hash of kind + provider/instance + canonical external key
kind                 git_commit | branch | tag | release | pull_request |
                     merge_request | issue | phabricator_diff | phabricator_task |
                     gist | paste | build | ci_run | package | container_image |
                     uploaded_file | lfs_object | documentation | other
provider             github | gitlab | phabricator | git | generic | ...
project_id           optional owning project
external_key         full SHA, number, D123/T123, digest, or provider object ID
canonical_url        optional; absent for local/unresolved outputs
visibility           external | authenticated | archive_local | unresolved
first_seen_ms        earliest evidence time
last_seen_ms         latest evidence time
evidence_ids         sorted provenance links
```

Titles, state, author, and descriptions are mutable enrichment metadata and should
live in a separate ETag/timestamped cache. They are not part of artifact identity.
A GitHub pull request and issue with the same number are different artifacts. A
commit key requires a full object ID and repository identity; short SHAs alone are
unresolved candidates.

### Evidence

Evidence is immutable and retains attribution without retaining a whole command or
its output:

```text
evidence_id          hash of source digest/ref, extractor version, and matched fact
artifact_id          optional until the candidate is resolved
at_ms                source timestamp
agent_id             agent that emitted the source event/tool call
turn_id              optional provider-neutral turn
tool_call_id         optional exact tool invocation
event_id             optional transcript event
relation             created | committed | published | opened | uploaded |
                     mentioned | updated | consumed | verified
source_kind          provider_metadata | tool_input | tool_output |
                     transcript_url | user_config | remote_verification
outcome              succeeded | failed | unknown
confidence           verified | strong | candidate
source_ref           snapshot-relative file plus stable line/row/call identifier
sanitized_fact       optional bounded non-secret fact, never raw command output
```

One tool call can yield multiple evidence records, and one artifact can have evidence
from several agents. Preserve those facts instead of forcing one "owner". For
example, one agent may commit, another may push, and the coordinator may merely
report the pull request.

## Extraction and validation

Normalize each provider's raw tool record first into one or more command executions
with agent, turn, timestamp, working directory, command text/argv, result, and exit
status. Never execute recorded commands.

- Codex needs a decoder for the known `exec` wrapper and older direct tool-call
  shapes. JavaScript input can contain multiple nested calls, so parse known literal
  call forms rather than evaluating JavaScript. If one composite output cannot be
  paired to one nested command, retain `outcome: unknown`.
- Claude's structured Bash input and tool-result IDs provide the cleanest pairing.
  Its per-record `cwd` supplies repository-path context.
- Orc's code-execution block already provides the pair and exit code. Its database
  row/block ID is a stable source reference.

Use a shell AST or a deliberately narrow command parser. `shlex.split()` and regexes
alone do not correctly handle pipelines, `&&`, subshells, heredocs, redirections, or
quoted examples. Unsupported forms can produce candidates for later inspection, but
not a canonical output.

Provider-specific recognizers should then emit facts:

- Git: repository context (`git -C`, working directory, remotes), full commit IDs,
  created refs, push refspecs, tags, and releases. Treat commit creation separately
  from remote publication. `|| true`, pipelines without `pipefail`, background jobs,
  hooks, and concurrent worktrees weaken an apparently successful result.
- GitHub/GitLab: creation output URLs are strong evidence. Explicit `--repo` wins
  over current-directory context. Resolve API redirects/renames to aliases without
  changing the stable artifact link unexpectedly.
- Phabricator: retain the exact instance hostname as part of the key; `D123` and
  `T123` without an evidenced instance are unresolved. Orc local task IDs must not be
  silently treated as Phabricator tasks.
- Gists, pastes, uploads, release assets, build/CI results, packages, container
  images, and LFS objects: require a provider ID, content digest, or returned URL.
  A local filename or the word "uploaded" is not enough. LFS publication often has
  no useful browser URL and may remain a digest-only artifact.
- Transcript URLs: recognize only explicit, syntactically valid URLs and record the
  relation as `mentioned` unless stronger evidence exists. Extend the current
  conservative GitHub pull-request recognizer rather than making naked numbers
  linkable.

Repository URL normalization removes credentials, trailing `.git`, default ports,
and irrelevant query/fragment data; handles HTTPS, SSH, and SCP-style Git remotes;
and lowercases only components known to be case-insensitive. Artifact URLs use the
provider's canonical form. Dedupe by normalized provider key, not display text, and
retain redirects and alternate remotes as aliases.

Optional network enrichment should verify only recognized provider endpoints. Cache
successes, not-found results, ETags, and transient failures separately so an offline
rebuild remains deterministic and useful.

## Association with summaries and aggregation

Direct evidence is associated first with the originating agent and timestamp. A
phase contains evidence from that agent whose timestamp falls in its half-open time
range. If a command spans a phase boundary, use its start phase and retain its end
time in evidence. Artifacts mentioned by a parent while reporting a child's result
should have a parent `mentioned` relation; they should not overwrite the child's
direct `created` or `published` relation.

Add `artifact_ids` to each work-summary bullet and a deduplicated union to the phase
detail. Existing summary prose need not be regenerated:

1. Deterministically attach direct same-tool/same-agent/same-phase evidence.
2. Match explicit normalized references in existing bullets.
3. If needed, run a separate, cheap association job that receives the existing
   bullet text and the phase's known artifact catalog and may return only catalog
   IDs. Cache it by summary hash, artifact-catalog digest, and association-prompt
   version. Reject unknown IDs.

Agent-lifetime, daily, weekly, monthly, quarterly, and multi-team views are sorted
unions of their contained phase links. They do not ask a model to rediscover links.
The UI can distinguish outputs (`created`, `published`, `opened`, `uploaded`) from
mere references (`mentioned`) and can drill from a rollup to the phase and exact
source evidence.

Changing phase duration should only rebuild derived associations. Stable evidence
must not contain `phase_id`, because phase IDs and boundaries are presentation
choices.

## Site identity

Build site identity from the same project/host catalogs. Rank the primary project as:

1. an explicit archive setting;
2. the root coordinator's launch-repository metadata;
3. the repository containing the root working directory;
4. the repository with the most direct work-output evidence.

An explicit setting always wins, especially for a superproject with active
submodules. Keep all contributing projects and source hosts even when one is
primary. Render one project and one host as, for example,
`Agent Timeline: dev-consumer-a, devbig014`, with the project name linked. Render
`multi-repo` or `multi-host` when necessary and expose the complete, evidenced list
in a tooltip/menu. A project may have several teams and hosts; a team may touch
several projects. Do not encode either relationship as a single field on `TeamData`.

Timezone is display preference, not source identity. Store UTC instants together
with the archive's explicit IANA display timezone, and render that timezone
consistently in every browser. The CLI defaults new archives to
`America/New_York`; a future user-facing selector may deliberately persist a
different choice, but the browser's ambient timezone must not silently override
the archive.

## Security and privacy

Source snapshots and tool outputs can contain access tokens, credential-bearing Git
remotes, signed URLs, private hostnames, command arguments, and proprietary text.
The version-controlled artifact catalog therefore stores facts, not raw snippets.

- Strip URL userinfo and secret-bearing query strings. Never persist presigned URLs;
  store a non-secret provider object key only when it is safe and useful.
- Allow archive-level policies for public, authenticated, private, and redacted
  projects/hosts. Host aliases should be user-overridable.
- Never render `file:`, local paths, `javascript:`, or arbitrary tool-provided HTML.
  External links use `https:` where possible and `rel="noopener noreferrer"`.
- Do not fetch arbitrary transcript URLs. Metadata clients use explicit provider
  allowlists and the existing bounded/conditional-fetch pattern, avoiding SSRF.
- Treat all transcript, command, and API text as untrusted input in model prompts and
  HTML. Models select IDs; they never supply hrefs.
- Record whether an artifact is authenticated or unresolved so the UI does not imply
  that every viewer can open it.

Because the summary directory is intended for Git, default to excluding questionable
URLs rather than relying on the ignored raw snapshot to make them safe.

## Incremental and idempotent migration

Version the catalog, every provider decoder, and each recognizer independently. Key
the extraction cache by source digest, date window, extractor-version set, and
explicit project/host configuration digest. Sort all records and write only changed
JSON. A normal append can scan newly observed stable event/tool IDs and merge by
`evidence_id`; a version change can deterministically rescan the archive-local source
snapshot. Existing monotonic-source checks continue to reject disappeared or
truncated inputs.

Keep extraction-run receipts (time, source digest, versions, candidate/verified/error
counts) separate from canonical catalog content. This preserves a useful run history
without making an unchanged catalog look modified. Git history supplies the audit
trail when a newer extractor removes a former false positive; explicit artifact
aliases preserve identity across repository renames.

Migration does not require re-spending the existing phase, name, or rollup summaries.
Generate the catalog and deterministic association sidecars from snapshots and cached
summary JSON, then rebuild the static site. Only the optional association job incurs
new tokens. A future summary schema can include catalog IDs natively while retaining
the same prose cache and sidecar as the migration path.

## Staged implementation

### MVP: trustworthy links and identity without resummarizing

1. Add archive configuration for primary project URL, additional project URLs, and
   source-host aliases; auto-discover Codex root Git metadata and other evidenced
   repositories without overriding explicit configuration.
2. Add provider command/result normalization and conservative recognizers for Git
   commits/pushes, GitHub pull requests/issues, and Phabricator diffs/tasks.
3. Write projects, hosts, artifacts, evidence, and deterministic phase-association
   sidecars before tool inputs/outputs are scrubbed.
4. Render artifact chips/links in phase details and rollups, plus the evidenced
   project/host title and full-list menu.
5. Backfill the current test archives from their local snapshots and add adversarial
   fixtures for quoted commands, failed/retried commands, wrong repositories, short
   SHAs, credentials, signed URLs, and naked numbers.

### Next: better association and metadata

- Add the ID-only association pass for summaries whose output cannot be attached
  mechanically.
- Generalize conditional metadata caches from GitHub pull requests to issues,
  commits, repositories, Phabricator objects, and other configured forges.
- Expose provenance and relation filters in the UI: output only, all references, or
  unresolved candidates.

### Later: broader artifact ecosystems

- Add releases, tags, CI/build results, packages, container images, gists/pastes,
  uploaded files, LFS digests, and documentation sites through small provider
  plugins.
- Support cross-team deduplication and project/submodule graphs while preserving the
  same artifact and evidence identities.
- Add an explicit review queue for candidates that cannot safely become links
  automatically.
