# Related work for wrkslots

This note compares systems that create, own, recover, and retire working copies. It focuses on
lifecycle guarantees rather than command syntax or product scope. The OpenCode and Gastown sources
were read at their pinned revisions on 2026-08-29; the other GitHub-hosted sources were checked at
their pinned revisions on 2026-09-22. The Kubernetes website citations are live documentation
rather than immutable sources.

## Bottom line

The ecosystem does not lack worktree managers. It lacks a harness-neutral recovery controller for
authored work on a long-lived shared host.

- Git and standalone managers understand repositories but not the exact process that owns a slot.
- Agent products understand their own sessions but do not expose a shared ownership and recovery
  protocol to other harnesses.
- Ephemeral CI deregisters one-job runners; separate operator automation can export logs and wipe
  or replace their machines.
- Kubernetes supplies a useful reconciliation model, but not Git, process, or salvage semantics.

Wrkslots occupies the intersection: exact renewable owner evidence, conservative host-wide
liveness checks, distinct policies for authored and disposable work, verified remote salvage,
path and generation fencing, and crash-resumable cleanup evidence. It should reuse Git and proven
worktree-manager mechanics and own only this cross-harness evidence and recovery layer.

### Current implementation and migration boundary

The migration is staged. Python `wrkslots` remains the lifecycle authority. The current Rust
`wrkslotsd` component is a read-only observer of the hash-linked event log, a builder of a
disposable SQLite index, and a diagnostic shadow policy evaluator. Its `explain` and
`plan` commands reconstruct and self-consistency-check decisions from the captured inputs; they
also reject evidence that has aged out, and cannot apply a plan. The disposable index is not an
authenticated store: a coordinated same-UID rewrite of every bound input and projection can stay
self-consistent, and this phase does not compare it atomically with the live `EVENTS` directory.
Rebuild from trusted event/config/evidence inputs before treating its output as current. Once an
index holds policy evidence, a plain `rebuild` without `--config`/`--evidence` is refused rather
than discarding that evidence high-water mark; to return to a policy-free index, delete the
disposable index file and rebuild. It has no
daemon or socket behavior and performs no lease management, cleanup, rescue, deletion, or
repository mutation. It also has no repository-discovery inventory beyond its supplied event and
evidence inputs. The reconciliation daemon and lifecycle actions discussed below are roadmap,
not behavior shipped in the current Rust component. Its fail-closed
input boundary accepts at most 16 MiB per event and 127 nested JSON containers, Unicode scalar
strings, exact signed/unsigned 64-bit integers, and finite binary64 floats; Python remains
authoritative for its broader JSON input domain. Rust also rejects unknown event kinds, malformed
or generation-mismatched hold transitions, and non-object state evidence even where the current
Python reader ignores some of those values; the Python writer emits the stricter shape. Replay
likewise refuses operation progress that reuses a pending journal path for a different slot or
operation, where the Python reader overwrites the older marker, and an `operation-completed` with no
pending progress, recovery attempt, or identical earlier completion, which the Python reader
ignores. Either history fails the whole replay, so no decision is derived from a history in which
an older operation's blocker could silently disappear. When recovery starts for an operation whose
journal is no longer pending, the attempt is bound to that operation's completed journal path if
the history records one: completion is logged before the journal file is unlinked, so an
interrupted cleanup leaves a completed journal for recovery to load again. The CLI renders active and archive revisions as
decimal JSON strings so all `u64` values remain exact. First index publication requires a filesystem
that supports Linux `renameat2(RENAME_NOREPLACE)`. The candidate can diagnose a generation-bound
task-scope claim, census freshness, heartbeat and minimum-stale boundaries, holds and operation
markers, reported process use, and reported checkout `(device, inode, mount-id)` identities. It
does not yet independently reread `/proc/<pid>/stat`, the current boot ID, the systemd invocation
link, or cgroup existence at evaluation/read time. It also has no fresh TaskGraph claim document
bound to repository, task, owner, and lifecycle attempt. Every otherwise unblocked scoped row
therefore includes `TASK_SCOPE_RUNTIME_UNVERIFIED` and `TASKGRAPH_CLAIM_UNVERIFIED` and remains
`UNKNOWN`; legacy rows remain `UNKNOWN` as well. No current input can produce `ELIGIBLE`.

The policy JSON is a narrow evaluator configuration, not the project's authoritative
`.wrkslots.yml`. This slice does not compare checkout/repository paths, layout defaults, or landed
refs against that originating registry; it does not enumerate Git registrations from repositories
that are absent from the supplied rows; and it does not discover initialized nested repositories.
Those configuration-coupled and repository-coupled checks remain with Python `wrkslots`. The
evidence JSON is caller-supplied and no shipped collector authenticates or independently rechecks
its claims, which is why digest binding and freshness alone do not remove the runtime gate.

The policy file is strict JSON with `schema: 1`, the event-log `machine`,
`minimum_stale_seconds`, `max_plan_slots`, `evidence_max_age_seconds`,
`maximum_census_seconds`, and `maximum_future_skew_seconds`. The evidence file is strict JSON with
`schema: 1`, that same `machine`, the exact `event_tip_sha256`, RFC 3339
`census_started_at`/`observed_at` bounds, the observing `boot_id`, and one row per observed slot.
Each row binds `slot`, `generation`, and `active_record_sha256`; identifies and classifies the
recorded scope when present; records journal and process-use state; and lists each checkout's name,
path, directory/symlink/mount checks, device, inode, and mount ID. Unknown fields, duplicate rows,
incomplete present identities, unbound extra slots, stale or future evidence, overlong census
windows, a census beginning before its bound event-tip timestamp beyond configured skew, and
evidence for another event tip are refused. Heartbeat age is evaluated at the earlier of census
start and evaluation time, so a future-skewed census cannot make a live heartbeat look old.
Conversely, a heartbeat later than that instant has a negative age and still counts as within its
TTL and minimum stale age, so a heartbeat written after the census began cannot drop those
blockers either; a lead beyond `maximum_future_skew_seconds` additionally records
`CLOCK_BEFORE_HEARTBEAT`. Heartbeats and the event-tip timestamp are parsed with the same
CPython `datetime.fromisoformat` grammar that replay accepts, not only RFC 3339. That includes
CPython's offset rule: an offset whose whole-second part is zero is UTC and its fractional seconds
are discarded, so `+00.5`, `-00:00:00.5`, and `+00:00,9` all mean `+00:00`, while `-00:00:01.5`
keeps its fraction. A heartbeat that still cannot be aged is `BLOCKED` with
`HEARTBEAT_TIMESTAMP_UNSUPPORTED`, a record with no heartbeat is `BLOCKED` with
`ACTIVE_HEARTBEAT_MISSING` (either may be recent), and a missing TTL records
`ACTIVE_HEARTBEAT_TTL_MISSING` without skipping the minimum-stale check. Missing evidence for an
active slot adds a digest-bound `EVIDENCE_MISSING` reason; the heartbeat and minimum-stale blockers
are still evaluated because they depend only on the record and the census clock. The SQLite index retains canonical policy inputs and the
evaluation instant; `explain` and `plan` replay the indexed event chain, compare active/archive/hold
projections, re-evaluate every decision, reject evidence that is stale at read time, and reject
incomplete coverage or any changed field. Open progress, reclaim, recovery, and retirement markers
block their slot; an absent-journal census alongside an open marker also records an unknown
evidence conflict while preserving the stronger blocker.
If an interrupted operation has no ACTIVE row, the observer still emits one `BLOCKED` decision for
its slot so both `explain` and `plan` expose the open marker. Such a pending-only decision has null
generation, active-record, and heartbeat fields when the event history does not provide them; it
cannot be mistaken for a reclaim candidate and does not require an invented evidence-census row.
This remains true for a plain rebuild without optional policy inputs; that decision also carries
`POLICY_INPUTS_MISSING` and null policy/evidence bindings.

The index is disposable and derived: rebuild it after upgrading the observer. `INDEX_SCHEMA`
versions only the table layout, so a change to replay or policy semantics does not bump it. Bumping
it would not produce a clearer refusal: readers would report only `unsupported derived index schema
N`, and `rebuild` deliberately refuses to overwrite an index whose schema it does not recognize, so
the operator would have to delete the file by hand. Keeping the schema also keeps the evidence
high-water check in force across the upgrade. Old stored results cannot be served under new
semantics, because every read replays the indexed event chain again. Without policy inputs, `explain`
and `plan` derive their decisions from that replay and do not read the stored ones. With policy
inputs, a stored decision that the new code would not produce is refused as `indexed policy decision
differs from canonical re-evaluation` or `indexed policy decisions do not exactly cover replayed
active and pending state`. In both cases the remedy is `wrkslotsd rebuild` over the same index path,
using the same or newer evidence.

This marker treatment is deliberately stricter than the current Python audit, not a parity claim.
Python records `reclaim-started`, `recovery-started`, and `retirement-attempted` as attempt evidence
but has no separate abort event when a checked operation refuses. The Rust shadow therefore keeps
such an attempt blocked until the append-only history contains both the exact-generation archive
record with `physical_storage: removed` and removal of that generation from ACTIVE. Python's
`operation-completed` records only journal cleanup. Finish and ownerless-cleanup rollbacks and late
refusals emit it while retaining the slot, so for those operations it does not clear reclaim,
recovery, or retirement markers. Until a future producer adds an explicit terminal outcome for
non-removing operations, those attempts remain a rollout blocker; the observer can explain them but
cannot clear or override them.

Replay does not yet model the episode identity that Python binds to each scoped journal. Python
refuses a history in which progress on a scoped journal path changes the `fenced` name or create
`operation_id` of the episode it continues, or reopens the path with the identity of a completed
operation. Rust replay checks only the machine, slot, and operation kind of each progress journal,
so it accepts such a history. This slice cannot emit `ELIGIBLE`, so the gap cannot clear a slot
today; replay must refuse these histories as Python does before any phase makes eligibility
actionable.

Create and import-existing are the exception, but only when the completion leaves an ACTIVE row that
owns the attempt's storage. A create journal records its slot type and every checkout path it plans,
and an import journal carries its whole row. Such a completion closes the `recovery-started` marker
for the same slot and operation when that marker is bound to the completed journal path and the
ACTIVE row has the attempt's slot type and owns every one of those checkout paths. Each journal a
recovery binds has its own marker, so a later recovery through another journal leaves an open marker
in place. Recovering the same journal again adds that attempt's storage to what its marker requires,
and attempts under different slot types then require storage no row can own. An import that recorded
no checkouts, which a historical import of a slot whose checkouts are all missing publishes, owns
nothing and needs only the slot type. A marker bound to another path stays open, such as the default
for a journal that predates the event log, and so does one whose journal does not identify its
storage, including a create journal that names no path. A completion that leaves no such row also
leaves the marker open. The current abort paths below leave no provisional storage, but replay reads
histories written by every past writer, and a production history contains a create journal completed
after `recovery-started` with no row ever published while its provisioned worktree, holding commits
beyond the recorded start point, remained on disk. Such a slot therefore stays `BLOCKED` with
`ACTIVE_RECORD_MISSING` and `RECOVERY_PENDING` instead of disappearing from the observer. The cost
is a blocker for a slot that current code aborted cleanly. After an aborted create it ends only when
a later create of the same slot at the same journal path publishes a row of the same slot type that
owns every checkout path the aborted attempt planned. `_cmd_create` refuses an existing slot path,
and Python derives that path from the slot type's root, so such a create proves that the aborted
attempt left no slot directory. A create of the same slot name under the other slot type proves only
that its own root was clear, and it leaves the marker open. The path check is stricter than that
proof: a nested re-create that plans fewer checkouts than the aborted attempt leaves the marker
open, although the refused slot directory held all of them. So does a re-import through the same
journal that records fewer checkouts than an aborted import, although Python publishes an import
only when the slot directory holds exactly the row's checkouts. Replay compares checkout paths
rather than deriving each slot directory from the checkout names, so these are false blockers and
never false clearances. The proof does not extend to branches or caches outside the slot path that
an older writer may have left. Some of these blockers are permanent, because no event that current
writers append can close them and phase 1 has no operator acknowledgment event: a marker whose
journal does not identify its storage; a create marker bound to the legacy singleton journal,
because current creates complete only at their scoped path; an aborted import-existing followed by a
create rather than a re-import of the same checkouts, because the completion's operation differs;
and a create or import marker still open when the slot is archived, because archive evidence clears
only finish attempts. The enumeration of `py/wrkslots/cli.py` below supports the row-present cases
for histories written by that code. Line numbers are at the commit "wrkslots: bound audit cache-glob
planning"; later commits move them.

- `_clear_journal` (5754) is the only writer of `operation-completed` (5765). Every
  `_write_event_file` and `writer.append` call passes a literal kind. The other occurrences, at 5840
  and 7453, are readers. The append happens before the journal is unlinked, with the test
  interrupt `after-operation-completed` at 5772 between them.
- Create journals come only from `_create_journal_payload` (kind `create`, 13808). Import journals
  come only from `_import_journal_payload` (kind `import-existing`, 14826).
- `_recover_create` and `_recover_import_existing` are dispatched at 35298 and 35309, after
  `recovery-started` is appended at 35234.

The eight `_clear_journal` calls on a create or import journal, and the state each leaves:

| Call site | Line | Storage and ACTIVE state when the journal completes |
| --- | --- | --- |
| `_cmd_create` | 14144 | `_write_active_state(action="slot-created")` (14135) runs first, so the new ACTIVE row owns the storage |
| `_recover_create` | 24388 | The ACTIVE row is already durable and must match the journal exactly. Every checkout HEAD is verified first, and `--abort-create` is refused (24351) |
| `_recover_create` | 24507 | Recovery provisions and runs hooks, then publishes the row with `action="slot-created-by-recovery"` (24499) |
| `_abort_create` | 24116 | Called at 24392 only when there is no ACTIVE row. It removes caches and worktrees, deletes branches at their expected heads, and removes the slot directory. Any unexpected state raises before the clear, so neither storage nor a row remains. The marker stays open because a completion without a row cannot distinguish this from an older writer that left storage |
| `_publish_import` | 14868 | `_write_active_state(action="slot-imported")` (14858) runs first |
| `_recover_import_existing` | 24546 | The row is already durable and matches exactly, and `_verify_import_record` passes. `--abort-import` is refused (24543) |
| `_recover_import_existing` | 24552 | `--abort-import` with no row. An import publishes rows only for checkouts that already exist, so it changes no files and owns no provisional storage. The checkout was never registered and has no row, so the recovery marker stays open and the slot remains blocked rather than leaving the observer |
| `_recover_import_existing` | 24576 | Recovery publishes the row with `action="slot-imported-by-recovery"` (24567) |

The checks that run after these clears never undo them. They are `_validate_global_state` and
`_assert_only_slot_changed`, in `_cmd_create`, in `_cmd_import_existing`, and after recovery
dispatch (35436). They only read state, so a refusal there leaves the slot in the state the table
records. A recovery refused before its clear appends no completion, which leaves both the journal
marker and the recovery marker open.

Finish is different. `_rollback_path_fence` (21556) and the two refusal rollbacks in
`_begin_finish` (22104 for a private finish, 22126 otherwise) complete a finish journal while
retaining the slot. `_rollback_validation_fence` (27417, called from
`_recover_ownerless_validation` at 28548, 28636, and 28704) does the same for an ownerless
validation. Those operations therefore keep the archived-removal rule above.
For a pre-event-log finish journal, recovery can import snapshots that are already at any side of
the archive/ACTIVE publication boundary. Once those snapshots prove physical removal, the observer
retains a synthetic legacy-journal cleanup marker until an `operation-completed` at that journal's
path; this keeps the interrupted operation visible without leaving a false recovery blocker after
completion. A completion at another journal path, such as a later scoped finish, leaves it pending.

Python `clean-caches --only SLOT` limits cache-directory traversal and scoped-journal checkout/Git
inspection to the named slot. It reads each complete bounded unselected scoped journal and verifies
both its shape and its exact append-only progress provenance without inspecting its checkouts. It still
performs global control-plane discovery, validates every active record path, and replays every
authoritative `EVENTS.<machine>` shard before mutation. That replay is intentional with the current
format: event filenames contain only sequence numbers, and event-first journal publication means a
slot can exist only in an event when the process stops. Eliminating unselected-shard replay safely
requires a trusted locator that is bound to every shard's count and terminal digest (and that also
indexes pending operations); compatibility `ACTIVE`/`ARCHIVED` snapshots are not such an authority.

## Capability comparison

| System | Useful capability | Gap relative to wrkslots |
| --- | --- | --- |
| Git | Worktree registry, locks, repair, clean removal, and pruning registrations whose directories are already gone | No agent ownership or liveness, dirty-work salvage, reconciliation daemon, or crash journal |
| Worktrunk | Age-gated pruning of clean integrated worktrees, dirty and locked refusal, process reaping, trash staging, and process-group tethering | Command-driven rather than lease-driven; no remote rescue of abandoned dirty work or resumable cleanup journal |
| WTP | Managed paths, setup hooks, branch-aware removal | Explicit removal only; force mode discards dirt; no stale-owner policy or salvage |
| git-worktree-runner | Agent launch, pull-request-aware cleanup, dirty-tree refusal, and phantom-registration repair | No exact owner identity, renewable lease, autonomous expiry, or salvage |
| Claude Code | Native agent worktrees, live-owner locks, periodic stale cleanup, unpublished-work protection, and a commit/push/draft-PR completion path | Product-specific; the core implementation is not publicly auditable and no cross-harness ownership or deterministic rescue contract is published |
| Codex | Atomic thread-owner metadata, managed roots, resumable-thread association, and clean-only removal | Ownership is explicitly not activity or exclusion; CLI automatic cleanup is disabled; no TTL, host-wide liveness proof, or salvage |
| OpenCode | Central create/list/reset/remove service and post-failure Git-registry checks | Explicit force-removal workflow; no renewable owner evidence or salvage |
| Vibe Kanban | Registered-workspace inventory, orphan cleanup, and forced Git/filesystem removal | Application-specific and destructive; no external owner lease, conservative liveness proof, or salvage-before-delete contract |
| Gastown | Multi-fact recovery verdict, live tmux checks, and recurring patrol | Coupled to reusable polecat sessions; no deterministic remote salvage or journaled filesystem evidence |
| Parallel Code | Task/worktree/agent association, agent termination before close, and dirty/unmerged warnings | Confirmed close ultimately force-removes, including a recursive-deletion fallback; no expiry or rescue protocol |
| jj workspaces | Snapshot tracked working-copy changes before removal and recover stale working copies through the operation log | Ignored files are deleted; no process ownership, TTL, disk-pressure controller, or remote publication |
| Ephemeral CI runners | One-job deregistration plus separately operated log export and machine replacement | Isolation depends on operator automation; deliberately loses unpublished local state and does not solve authored work on a shared host |
| Kubernetes controllers | Leases, owner references, finalizers, and restartable reconciliation | An architectural model rather than Git/process/filesystem salvage semantics or a local security boundary |

## Git: the storage substrate, not the lifecycle authority

Git stores linked-worktree registrations in the repository's common directory. `git worktree
list --porcelain`, `lock`, `repair`, `remove`, and `prune` are the necessary low-level mechanisms.
Pruning only removes administrative entries after their working directories are already missing;
it does not select or delete abandoned working directories. Clean removal protects tracked and
untracked files unless the caller supplies `--force`.

These facts are necessary but incomplete. Git cannot identify the process or session that owns a
worktree, distinguish a live owner from a recycled PID, classify a validation checkout as
disposable, or prove that dirty and unpushed work was published before removal. Wrkslots therefore
treats Git's registry as one observed fact, not as the ownership record.

Source: [Git worktree documentation at 3bc03411](https://github.com/git/git/blob/3bc0341126508f78f5869cbfc0005e987efdf0c7/Documentation/git-worktree.adoc).

## Standalone worktree managers

### Worktrunk

Worktrunk is the closest public standalone comparator. `wt step prune` considers age, integration,
dirt, and locks; its documented default minimum age is one day, and it removes only integrated,
clean, unlocked candidates. `wt remove` can reap processes associated with a worktree, and
`wt step tether` binds a process group to worktree lifetime. Removal uses trash staging where the
platform supports it.

This is a strong basis for interactive worktree hygiene, and wrkslots should track its Git-facing
mechanics instead of recreating generic worktree UX. The remaining distinction is authority:
Worktrunk's age is worktree or reference age, not a renewable lease held by an exact process
identity. Its process discovery is narrower than a conservative host-wide reconciliation across
process cwd, file descriptors, mapped files, cgroups, mounts, and namespaces. It also does not
publish abandoned dirty work to a deterministic, non-branch rescue ref before reclaiming space.

Sources:

- [pruning design at 2a4aa28c](https://github.com/max-sixty/worktrunk/blob/2a4aa28c269230dbbeb8b5a64cb3dfb30c1b2fdb/docs/src/content/docs/step.md)
- [removal and process reaping at 2a4aa28c](https://github.com/max-sixty/worktrunk/blob/2a4aa28c269230dbbeb8b5a64cb3dfb30c1b2fdb/docs/src/content/docs/remove.md)
- [removal guard at 2a4aa28c](https://github.com/max-sixty/worktrunk/blob/2a4aa28c269230dbbeb8b5a64cb3dfb30c1b2fdb/src/commands/remove.rs#L252-L268)
- [prune implementation at 2a4aa28c](https://github.com/max-sixty/worktrunk/blob/2a4aa28c269230dbbeb8b5a64cb3dfb30c1b2fdb/src/commands/step/prune.rs)

### WTP

WTP creates branch-named worktrees beneath a configured root and can run setup hooks. Removal is an
explicit command. Its force option can discard dirty content, and it has no external owner lease,
stale-owner reconciliation, or salvage-before-delete contract.

Sources:

- [WTP overview at 842920d4](https://github.com/satococoa/wtp/blob/842920d489e96c67798c6b2689f48dc885e42094/README.md)
- [WTP removal at 842920d4](https://github.com/satococoa/wtp/blob/842920d489e96c67798c6b2689f48dc885e42094/cmd/wtp/remove.go)

### git-worktree-runner

git-worktree-runner launches coding agents in worktrees and includes pull-request-aware cleanup.
Its cleanup skips dirty trees by default and repairs locked phantom registrations. This directly
addresses the common launch-and-clean workflow. It does not record a generation-bound process
owner or renewable lease, run an autonomous expiry policy, or rescue unpublished work; the force
path remains destructive.

Sources:

- [cleanup at d576398f](https://github.com/coderabbitai/git-worktree-runner/blob/d576398fc0642dc91cc248f63635e4dea283d416/lib/commands/clean.sh)
- [removal at d576398f](https://github.com/coderabbitai/git-worktree-runner/blob/d576398fc0642dc91cc248f63635e4dea283d416/lib/commands/remove.sh)

## Agent harnesses and workbenches

### Claude Code

Claude Code exposes native `--worktree` sessions and isolated subagents. Its public changelog
documents owner locks while sessions run, periodic cleanup of stale worktrees, protection for
unpushed and untracked work, and a normal completion path that can commit, push, and open a draft
pull request.

This is substantial lifecycle behavior, but the public repository exposes changelog statements,
not the core implementation. The evidence therefore supports the documented behavior, not claims
about its exact liveness proof or atomicity. It is also a single-product facility: no public
protocol lets another harness share its ownership decisions or finish its recovery transaction.

Sources from the public changelog at `56f36532`:

- [worktree lifecycle entry at lines 3739-3742](https://github.com/anthropics/claude-code/blob/56f36532530f88b572854538d685fcf781141e8c/CHANGELOG.md#L3739-L3742)
- [worktree lifecycle entry at lines 1398-1401](https://github.com/anthropics/claude-code/blob/56f36532530f88b572854538d685fcf781141e8c/CHANGELOG.md#L1398-L1401)
- [worktree lifecycle entry at lines 3179-3183](https://github.com/anthropics/claude-code/blob/56f36532530f88b572854538d685fcf781141e8c/CHANGELOG.md#L3179-L3183)

### Codex

Codex's public Rust implementation records thread ownership atomically under a managed worktree
root and lets its browser associate worktrees with resumable or archived threads. Removal refuses
dirty worktrees. The source also explicitly warns that ownership metadata does not prove activity
or exclusive use, and its CLI configuration disables the automatic cleanup used by another
surface.

The metadata is a useful identity primitive, but not a host-wide lease: it has no published TTL,
exact process-liveness evidence, or abandoned-work salvage transaction.

Sources at `286d4ecf`:

- [thread ownership metadata](https://github.com/openai/codex/blob/286d4ecf44b4e9daba0a9fdd229a4047b770a71a/codex-rs/worktree/src/metadata.rs#L1-L85)
- [ownership caveat](https://github.com/openai/codex/blob/286d4ecf44b4e9daba0a9fdd229a4047b770a71a/codex-rs/tui/src/worktree_browser.rs#L1-L47)
- [clean-only removal](https://github.com/openai/codex/blob/286d4ecf44b4e9daba0a9fdd229a4047b770a71a/codex-rs/worktree/src/lib.rs#L284-L328)
- [CLI cleanup setting](https://github.com/openai/codex/blob/286d4ecf44b4e9daba0a9fdd229a4047b770a71a/codex-rs/worktree/src/settings.rs#L1-L36)

### OpenCode

OpenCode has one worktree service that creates, lists, resets, and removes worktrees. It chooses a
directory below application-managed data, registers the directory with its project store, emits
ready or failed events, and disposes its watcher and store entry during removal. If Git reports a
removal failure, it checks whether the registration nevertheless disappeared.

The single-service boundary and post-failure registry check are worth retaining. OpenCode does not
record an external process owner, renewal time, or TTL, and removal is an explicit force operation
without prior publication of dirty or unpushed work.

Source: [OpenCode worktree service at df35e842](https://github.com/anomalyco/opencode/blob/df35e842f59bc115bb7c0479a8e11f017d443f2c/packages/opencode/src/worktree/index.ts).

### Vibe Kanban

Vibe Kanban reconciles its registered workspaces with repository worktrees and removes orphans.
Its removal path invokes forced Git worktree removal and falls back to recursive filesystem
deletion when the directory remains. This is a direct precedent for inventory-driven cleanup, but
the destructive fallback is not a substitute for exact renewable ownership, conservative host-wide
liveness checks, or salvage-before-delete.

Sources at `d5cbb538`:

- [orphan cleanup](https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/crates/workspace-manager/src/workspace_manager.rs#L537-L645)
- [forced Git and filesystem cleanup](https://github.com/BloopAI/vibe-kanban/blob/d5cbb5380fa0b32e98ef9b8d987f63decce4be3a/crates/worktree-manager/src/worktree_manager.rs#L228-L260)

### Gastown

Gastown manages agent worktrees as polecat sandboxes. `DecideWorkstate` combines agent state, hook
work, cleanup status, failed pushes or merge requests, Git dirtiness, stashes, unpushed commits,
active work, and merge-queue state. Unknown or failed Git checks become `NEEDS_RECOVERY`, not
clean. A separate tmux check refuses reclaim when lookup fails or the session is live. Its recurring
patrol can retry interrupted cleanup through a later participant.

This multi-fact, fail-closed verdict and recurring reconciler are strong precedents. Gastown is
coupled to long-lived reusable polecat sessions and permits an explicitly destructive force path;
it does not define wrkslots-style verified remote salvage and journaled filesystem evidence.

Sources at `649b832b`:

- [workstate decision](https://github.com/steveyegge/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/polecat/workstate.go)
- [reclaim blockers](https://github.com/steveyegge/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/polecat/reclaim.go)
- [lifecycle patrol design](https://github.com/steveyegge/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/docs/design/polecat-lifecycle-patrol.md)

### Parallel Code

Parallel Code associates tasks, agents, and worktrees. Its close flow terminates the agent and warns
about dirty or unmerged work. After user confirmation it can force-remove the worktree, with direct
recursive deletion as a fallback. That is a useful interactive warning boundary, but not an
autonomous stale-owner or salvage contract.

Sources at `47ef0dec`:

- [task and worktree model](https://github.com/johannesjo/parallel-code/blob/47ef0dec7559602402ad4637053729aa1e2ac04e/README.md#L46-L77)
- [close-task orchestration](https://github.com/johannesjo/parallel-code/blob/47ef0dec7559602402ad4637053729aa1e2ac04e/electron/ipc/tasks.ts#L58-L79)
- [dirty and unmerged work warning](https://github.com/johannesjo/parallel-code/blob/47ef0dec7559602402ad4637053729aa1e2ac04e/src/components/CloseTaskDialog.tsx#L80-L159)
- [force-removal path](https://github.com/johannesjo/parallel-code/blob/47ef0dec7559602402ad4637053729aa1e2ac04e/electron/ipc/git.ts#L833-L890)

## Alternative lifecycle models

### jj workspaces

Jujutsu's working-copy model snapshots tracked changes before removing a workspace, and its
operation log supports recovery from stale working copies. This is a useful model for separating a
durable repository operation from a disposable checkout. The documented removal semantics delete
ignored files, however, and jj does not own process identity, renewal, disk-pressure policy, or
remote rescue publication.

Sources at `899c03f2`:

- [working-copy model](https://github.com/martinvonz/jj/blob/899c03f24b45f02e132121e7c118e01cc6afd19c/docs/working-copy.md)
- [snapshot-before-remove implementation](https://github.com/martinvonz/jj/blob/899c03f24b45f02e132121e7c118e01cc6afd19c/cli/src/commands/workspace/remove.rs)

### Ephemeral CI runners

GitHub's ephemeral mode automatically deregisters a runner after it processes one job; that action
does not itself export logs or wipe the host. GitHub tells operators to forward runner application
logs externally, while autoscaling or reimaging automation separately retires, wipes, or replaces
the machine. Together those practices are the strongest answer for genuinely disposable work
because cleanup does not need to infer whether a later owner will return.

The Actions runner's `PipelineDirectoryManager` is also not an unconditional between-job wipe. It
deletes known pipeline directories only when `workspace.clean` selects `all`, `resources`, or
`outputs`; otherwise it retains them.

That model is safe only after valuable output has left the worker. It deliberately does not
preserve an agent's uncommitted source, untracked experiments, or local commits that were never
published, and it does not address several long-lived agents sharing one development host.

Sources:

- [GitHub ephemeral runner guidance at 5648c698](https://github.com/github/docs/blob/5648c6985586b49f88b98e5a8b7e6d60f207b3a4/content/actions/reference/runners/self-hosted-runners.md)
- [runner workspace cleanup at 50bd7667](https://github.com/actions/runner/blob/50bd7667ef037c0bef8fdc38337a7a797f2279c7/src/Runner.Worker/PipelineDirectoryManager.cs)

### Kubernetes controllers

Kubernetes uses owner references and finalizers so a controller can reconcile desired and observed
state after any particular controller restarts. Leases carry renewable timestamps, while a
finalizer prevents an object from disappearing until cleanup has completed. These are useful
control-plane patterns, not claims that same-user local processes have separate permissions or
that a local state file is a security boundary.

Sources:

- [owners and dependents](https://kubernetes.io/docs/concepts/overview/working-with-objects/owners-dependents/)
- [finalizers](https://kubernetes.io/docs/concepts/overview/working-with-objects/finalizers/)
- [Lease objects](https://kubernetes.io/docs/concepts/architecture/leases/)

## Slot storage and sandboxing

`wrkslots` added two things that the lifecycle comparison above does not cover: storing a slot as
one disk image, and running a command or agent boxed to one slot. The public systems below solve
parts of this; none combines per-slot storage bounds, per-slot resource limits, and lifecycle
authority without root or host configuration.

| System | File-system confinement | Resource limits | Storage bound per workspace | Needs root or host setup |
|---|---|---|---|---|
| bubblewrap | user + mount namespaces, bind mounts | none (delegates to caller) | none | no (unprivileged user namespaces) |
| Claude Code sandbox runtime | bubblewrap on Linux, Seatbelt on macOS, around shell commands | none | none | no |
| Codex CLI sandbox | Landlock + seccomp, or bubblewrap, with read-only/workspace-write/full modes | none | none | no |
| OpenCode | per-tool allow/ask/deny permission rules; no OS sandbox | none | none | no |
| systemd-run / systemd-nspawn | nspawn: full container; `systemd-run --user`: mount properties need `PrivateUsers` | cgroup properties on units and slices | none | nspawn needs root |
| Podman (rootless) | full OCI container | cgroup v2 when delegated | image layers; volume quotas need XFS project quotas | subordinate UID ranges configured by root |
| Dev Containers / Codespaces | container per workspace | container runtime limits | container disk | container runtime or cloud VM |
| btrfs subvolume + qgroup | none | none | yes, but quotas account every change on the whole file system | root to delete subvolumes and to enable quotas |

What `wrkslots` takes from them:

- **bubblewrap's model** — a fresh user and mount namespace, read-only binds of the host, writable
  binds of exactly what the task owns — implemented directly so it works where bubblewrap is not
  installed, and extended with a private per-slot `$HOME` layer over read-only binds of the real
  entries, and with a `root` mode (a `sudo -n` launcher and a mount namespace, then dropped
  privileges) for harness launchers that need a setuid step. Like bubblewrap without seccomp, and
  unlike a container runtime, the result is an accident boundary for cooperative agents on the
  user's own uid, not containment of a hostile process: the user's systemd bus stays reachable,
  and `root` mode allows setuid programs.
- **Harness sandboxes confine commands, not budgets.** The Claude Code and Codex sandboxes decide
  where a command may write and connect; neither bounds memory, CPU, processes, or bytes at rest,
  and neither reclaims anything. `wrkslots run` adds those per slot and leaves the network and the
  harness's own sandbox alone, so the two compose.
- **systemd slices as the budget unit**, as `systemd-run` offers, with the slot's slice nested
  inside whatever slice the caller is already in so a caller's enclosing policy is never escaped.
- **Images instead of quotas.** btrfs qgroups bound a subvolume's bytes but make every write on
  the host file system pay for quota accounting, and deleting a subvolume needs root. A sparse
  ext4 image bounds bytes and inodes per slot, confines metadata growth to the image, and is
  reclaimed by deleting one file. Its mount is created and removed by wrkslots through `sudo -n`
  or unprivileged FUSE (`fuse2fs`), so no host configuration is required.

Sources:

- [bubblewrap](https://github.com/containers/bubblewrap)
- [Claude Code sandbox runtime](https://github.com/anthropic-experimental/sandbox-runtime)
- [Codex sandboxing](https://github.com/openai/codex/blob/main/docs/sandbox.md)
- [OpenCode permissions](https://opencode.ai/docs/permissions/)
- [systemd.resource-control](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html)
- [fuse2fs](https://man7.org/linux/man-pages/man1/fuse2fs.1.html)

## Design lessons for wrkslots

The comparison narrows what wrkslots should implement:

1. Use Git's registry and removal operations; do not build another Git front end.
2. Borrow Worktrunk's clean/integrated/locked classification, process reaping, and trash staging.
3. Follow Gastown and Kubernetes in deriving decisions from several durable facts and letting a
   later reconciler resume an interrupted transaction.
4. Treat harness metadata as a useful input, never sufficient proof that a process is live or dead.
5. Keep authored agent slots distinct from disposable validation checkouts. Only the latter may be
   reclaimed without source salvage.
6. Make cleanup monotonic and generation-fenced: revalidate immediately before every irreversible
   step, and record enough evidence to resume rather than guess after a crash.
7. Publish dirty, untracked, and unpushed authored work to a deterministic rescue reference before
   removal; optional agent-written summaries may add context but must not change the rescue result.
8. Keep the reconciler narrow. Repository-specific policy, build-cache eviction, and session
   discovery belong behind adapters rather than in the core lifecycle state machine.
