---
name: wrkslots
description: Manage isolated Git worktree slots with one-agent ownership, machine-sharded state, leak and unpushed-work diagnostics, holds, safe cache reclamation, owner-alive handoff, post-exit removal, and crash recovery. Use before provisioning, importing, protecting, inspecting, reclaiming, finishing, recovering, or removing a managed worktree slot.
---

# Wrkslots

Treat the configured control and slot directories as tool-owned. Use `worktrees/wrkslots`; never
edit `ACTIVE.*`, `ARCHIVED.*`, holds, or recovery journals, and never substitute raw `git worktree`
or directory removal for a lifecycle command. `create` is the normal allocation path because Git
provisioning and registration are one operation. `register` and `import-existing` are exceptional
recovery and migration paths.

## Initialize

Initialize from the project root with the registered liveness command. Its exit status is part of
the removal boundary: rc 0 means verified dead, rc 1 means alive, and rc 2 means unverifiable. A
probe or occupancy scan failure is never evidence that a slot is clear.

For a single repository whose slot directory must itself be the checkout root, use flat layout:

```sh
wrkslots init . \
  --worktrees-dir worktrees/slots --layout flat \
  --liveness-command worktrees/agent_liveness_probe.py \
  --max-active-slots 12 \
  --cache-glob target --cache-glob node_modules \
  --post-provision-hook 'scripts/bootstrap-slot' \
  --post-provision-hook 'tools/validate.py --help' \
  --disk-advisory-gib 250 --disk-provisioning-floor-gib 200 \
  --disk-emergency-gib 100
```

Flat layout permits exactly one repository and produces this shape:

```text
worktrees/
  wrkslots
  agent_liveness_probe.py
  ACTIVE.<machine>.json
  ARCHIVED.<machine>.json
  slots/
    slot01/                 # checkout root
```

Control files remain beside `slots/`, never inside it. Flat layout permits exactly one repository
per slot. The default nested layout remains `<worktrees-dir>/<slot>/<repo-name>/` and supports
multiple repositories. `.wrkslots.yml` is literate YAML (a strict subset; older projects may hold
JSON, which is read the same way; `wrkslots config convert --to yaml` converts). The
corresponding policy keys are `cache_globs`, `repo_cache_globs`, `post_provision_hooks`,
`disk_advisory_bytes`, `disk_provisioning_floor_bytes`, and `disk_emergency_bytes`; configure all
three disk thresholds or none. A global `--cache-glob PATH` applies to every checkout. For a
heterogeneous nested slot, repeat `--repo-cache-glob NAME=PATH-GLOB`; those patterns are additive
only for the matching `--repo NAME=...` checkout.

A configuration written by an older build that lacks required fields makes commands refuse.
`init --repair` adds compatible fields and prints every change, but must not be used to reinterpret
populated nested storage as flat storage. It also upgrades the exact empty schema-1 active and
archive files and the repository command symlink written before packaging. It refuses populated,
malformed, or other unsupported state rather than reporting an empty or partial slot list.

## Create and recover provisioning

Create one fresh slot for one agent. Repeat `--repo` and `--branch` for multiple repositories. The
configured `origin` remote is used by default; use `--remote NAME=REMOTE` to select another
already-configured remote. Use `--remote-url NAME=URL` when the caller must verify the selected
remote's exact fetch URL; otherwise wrkslots records its configured URL. Wrkslots requires the
recorded remote identity to remain unchanged. Record the coordinator process generation even when
the owner will adopt later.

```sh
worktrees/wrkslots create slot01 \
  --agent codex-1 --task task-123 --purpose "fix parser" \
  --coordinator-pid "$COORDINATOR_PID" --owner-pid "$OWNER_PID" \
  --repo product=. --remote product=origin \
  --remote-url product=https://github.com/example/product \
  --branch product=codex/fix-parser
```

The selected source worktree may contain the configured managed root, as in `--repo product=.`
above. That exact source root is not a target collision; any other Git-registered ancestor, the
requested slot root itself, or a descendant is refused.

Provisioning always starts with an empty per-checkout build cache; it never copies a donor cache.
The first dependency build may therefore take many minutes instead of roughly one or two minutes,
and per-slot cache storage is not shared by reflinks.

Post-provision hooks run in order inside the fresh checkout before the active row is published. Use
them for real setup and verification commands; verify an entry point by running it, such as
`tool --help`, rather than by checking that a directory exists. A failed hook is a provisioning
failure, not a warning: report its captured output. The incomplete slot and create journal are
retained for inspection, no active row is silently advertised as ready, and later mutations refuse
until `recover` resumes the journal. Hooks must tolerate retry after a partial run.
Fix the referenced script or environment without changing the configured hook list while a create
journal exists; recovery verifies that the journal and configuration still name the same hooks.
If the journal says a hook was `running` when the process stopped, inspect its effects first and use
`recover --retry-running-hook` only when repeating that command is safe.
If the provisional slot should instead be discarded, use `recover --abort-create` only after
inspection. It is destructive, but first proves every provisional checkout still has its recorded
branch and HEAD and has no non-cache source changes; any disagreement preserves all worktrees.

## Inspect, audit, and diagnose

Use `status --all-machines` for ordinary read-only inspection. It returns the full readable roster,
reports outstanding journals, and emits every observable row/directory/Git-registration
disagreement as a typed inconsistency with an overall inconsistent summary. Git registration
inspection is limited to source repositories named by readable rows; configuration is not a
repository inventory. Those findings do not make a row healthy and never authorize a mutation. A
malformed standalone journal is one of those typed inconsistencies, so status still returns the
readable roster; lifecycle mutations continue to refuse until the journal is repaired or recovered.
Corrupt event/schema/hash state, duplicate cross-shard ownership, and an ambiguous stored
repository identity remain global refusals. Heartbeat age and TTL expiry are diagnosis only and
never authorize removal.

Use `doctor` for the diagnosis-oriented view of all registry/storage disagreements. Like `status`,
it authorizes nothing and exits 0 when readable inconsistencies are found; unlike `status`, it is
organized as findings rather than as the active roster.

Use `audit [--format human|json]` for the lifecycle verdict view. It reports each slot as
`DELETABLE`, `BLOCKED` with the collected reasons, or `HELD`, reports cache sizes, and raises the
count-level leak signal when worktree count exceeds running-agent count. Treat a scan failure as occupied, never
clear: a false occupied result delays reclamation, while a false clear result can destroy live work.
Audit deliberately uses the currently stored remote-tracking refs and reports that it did not fetch;
`finish` and `remove` repeat the authoritative checks after fetching.

Use `unpushed [--format human|json]` when remote durability is in doubt. An uncontained local HEAD
has two opposite readings: work may truly need a push, or the same-named remote branch may have moved
after a rebase and the local tip may be stale. Do not decide from a matching commit subject and do
not force-push. If a same-named remote ref exists, use the exact path-limited
`git diff <local-head> <remote-ref> -- <touched-files>` command printed by wrkslots to distinguish
the cases. Unlike `audit`, this command fetches the configured remote before reporting.

## Holds and cache reclamation

Use `hold SLOT --reason "..."` and `unhold SLOT`; never create or remove hold metadata by hand. A
hold is a hard human protection: both source removal and cache reclamation skip the slot, including
bulk operations confirmed with `--yes`. Tool-owned hold metadata is not source dirtiness.

`clean-caches` removes only configured regenerable directories such as `target/` or
`node_modules/`. It is intentionally independent of source-removal safety: dirty state, unpushed
commits, and a live owner do not make a build cache irreplaceable. With no selector it reports only.
A registered slot is excluded from an inferred bulk sweep unless it is named with repeated `--only`
or the sweep is explicitly confirmed with `--yes`; transient process sampling does not override
recorded ownership. Unregistered leaked slots are also reported and may be reclaimed explicitly by
name or by `--yes`; source removal still requires inspection or import into the normal lifecycle.
Cache cleanup refuses a policy that overlaps tracked source, crosses a submodule or mount boundary,
or whose checkout identity changes while cleanup is in progress.
Cache presence and allocated bytes are measured dynamically rather than stored as a registry flag,
so clean-start slots are not counted as reclaimable until a cache actually exists.
Its JSON `action` is `REPORT`, `REMOVED`, `HELD`, or `BLOCKED`; `cache_error` explains a blocked
inspection. An unregistered checkout has no trusted repository-name mapping, so repository-specific
cache policy is never guessed from its directory name; import it before applying such a policy.
When global globs are also configured, explicitly selected or confirmed unregistered slots can
reclaim only those global paths and report that repository-specific paths were omitted. With only
repository-specific policy, import is required first.

```sh
worktrees/wrkslots clean-caches --only slot01
worktrees/wrkslots clean-caches --yes --format json
```

The configured disk ladder is advisory below its warning threshold, refuses `create` below the
provisioning floor unless `--override-disk-floor` is supplied, and becomes an emergency below its
lowest threshold. Follow the refusal's `audit` and `clean-caches` commands first; at the emergency
floor stop builds, preserve or publish work, and reclaim space immediately. The override never
bypasses the emergency floor.

## Finish and remove source

While the owner is alive, `finish` proves clean Git state outside configured cache paths, trusted
remote identity, containing remote refs, landed ancestry, and path identity. Cache globs are trusted
destructive policy: configure only regenerable directories and never a source path. It records
validation, limitations, exact commits, and the coordinator continuation while retaining the row
and all physical storage.

```sh
worktrees/wrkslots finish slot01 \
  --agent codex-1 --owner-pid "$OWNER_PID" --expected-generation 1 \
  --validation "make test: pass"
```

Only the recorded coordinator may run `remove`. Removal requires the recorded handoff, registered
liveness rc 0, a dead owner process generation, and independent absence checks for cwd, executable,
file descriptors, memory maps, mount references, and the recorded cgroup. rc 1, rc 2, a live or
indeterminate owner, or any unreadable proof preserves every row and path. Holds also refuse removal.

Use `adopt` only to bind an unbound live owner from that owner's real process ancestry. It never
replaces a bound historical owner. If an older owner never bound and is independently verified
absent, the recorded coordinator uses `recover-unbound-owner`; this preserves `owner: null`, records
the recovery note and validation evidence, and prepares the same post-exit removal path.

Use `recover --coordinator-pid PID` whenever a create or removal journal exists. Preserve every path
after a refusal and report the exact message.

For a registered agent row whose directory is already absent, use
`recover-absent-agent-row` with the fresh audit's exact machine-selected slot, generation, and
`record_sha256`. It publishes and reads back every recorded checkout commit before removing an exact
stale Git registration, then archives before changing ACTIVE. For a real unregistered agent
worktree, use `recover-ownerless-agent-worktree` with its exact path, repository, HEAD, branch,
remote, and remote URL digest. This path deliberately records no owner, task, or handoff. If
HANDOFF.md exists, read it and supply its exact `--handoff-sha256`; the command rechecks and
preserves it, while an unread or changed handoff refuses. The exact non-worktree
`worktrees/slots/ignored` rust-script cache uses
`recover-ownerless-agent-cache`; it is relocated intact and is not a general directory exemption.

## Migrate an existing worktree manager

Cut over only from a quiescent, green repository with all intended work landed. Keep prior active
and archive files read-only rather than rewriting history; importing old archive history is optional.
Initialize one machine shard at a time, explicitly selecting the machine with global `--machine`
(or `WRKSLOTS_MACHINE`) and the integration ref with `--default-landed-ref`; these authority fields
cannot later be changed through repair. Then update all callers on that machine atomically. A stale
wrapper should fail loudly and print the replacement command rather than silently forwarding an
obsolete workflow.

Ideally migrate with no live trees. For a straggler already at its final managed path, run
`import-existing` first as a dry run, then repeat with `--apply --verified-live`, a real live
`--owner-pid`, and the recorded `--coordinator-pid`. It imports current ownership, not historical
archive rows. Multiple stragglers may be imported one at a time; migration import tolerates the
other still-unregistered slot directories while continuing to verify the selected checkout.

Wrkslots intentionally does not clone donor build caches, infer source corruption from cross-tree
drift heuristics, or attach and re-home an existing local branch during `create`. `create` continues
to require a fresh branch; use `import-existing` only for a checkout already provisioned at its final
path.

Run `worktrees/wrkslots --help` for the complete CLI.
