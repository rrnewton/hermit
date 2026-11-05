# wrkslots user guide

## What a slot contains

A slot is an append-only history plus a directory of linked Git worktrees. An `agent` slot holds
authored work and must be salvaged before reclaim. A `validate` slot is disposable by construction;
its logs and result records must live outside the checkout, so reclaim does not salvage it.

Use `worktrees/slots/<slot>/` for live agent slots and `worktrees/validate/<slot>/` for validation
slots. New-slot source repositories stay outside both directories. The recovery-only path may name
an existing source worktree inside managed storage solely to prove a stranded checkout's Git common
directory; it does not make that path valid for provisioning. `ACTIVE.*` and `ARCHIVED.*` are
readable compatibility views derived from `EVENTS.*`; do not edit any of them or a journal by hand.

Each checkout record carries its source repository, branch, starting commit, configured remote,
remote identity, landed ref, current commit, and remote containment evidence. Each slot records its
type, task and purpose, owner process identity, coordinator history, heartbeat time, and time-to-live.

## Normal lifecycle

1. `init` creates configuration, an empty machine history, the managed directories, and a
   project-local command symlink. It records the project command that distinguishes dead, alive,
   and unverifiable owners.
2. The coordinator assigns the slot. The coordinator or assigned agent runs
   `create --slot-type agent --coordinator-authorized` for authored work, or
   `create --slot-type validate --coordinator-authorized` for disposable validation. An assigned
   agent supplies its own `--owner-pid`. The flag is a reminder and recorded provenance, not a
   permission boundary between same-user processes.
3. The exact owner runs `heartbeat` to renew the slot while work continues.
4. An owner that has clean, published work may run `finish` to record validation and continuation
   evidence. A departed owner is not required to return: later reclaim can salvage from the recorded
   checkout state.
5. Any later participant may run `remove`. Reclaim proceeds only when the heartbeat time-to-live is
   expired, the row's liveness authority reports dead (the registered running command for an agent
   slot, the validation run's own evidence for a validation slot; see "Time-to-live and process
   evidence"), the exact owner process identity is dead, and process, mount, Git, and path checks
   agree. Unknown evidence refuses; it is not treated as
   free. `--validate-complete` lets the exact live owner remove its completed validation slot, or
   lets a later participant remove it after proven owner death without waiting out the heartbeat.
   Process-use, path, and Git checks still run. A newly started validation removal first records an
   identity-bound private seal, releases the registry locks for both bounded process censuses, then
   reacquires the locks and revalidates the exact seal, finish journal, owner, and ACTIVE row before deletion.
   Heartbeats, handoff finishes, and creates for provably disjoint slots can proceed during either
   census; operations on the sealed slot refuse. The short Git and registry mutation phase remains
   serialized.
6. An agent slot whose recorded owner is still alive may be released only with that owner's
   consent, expressed as a handoff. `remove` accepts it when all of these hold: the removal is
   `--coordinator-authorized` with a live coordinator; the heartbeat time-to-live has expired
   (`--validate-complete` is not a shortcut); the current slot generation has a completed
   `write-handoff` whose recorded writer is exactly the recorded owner process identity, so a reused
   PID or a handoff from an earlier generation does not qualify; and the coordinator has read that
   handoff with `read-handoff`. Every other check still runs: slot contents, uncommitted handoffs,
   the census of live processes whose working directory, open files, executable, or mappings are
   inside the checkout, and salvage before deletion. Only the comparison against the recorded
   owner's cgroup is skipped, because the consenting owner is still alive there by definition. Each
   missing condition is named in the refusal. The accepted basis is recorded in the event log as an
   `owner-consented-release-recorded` active-state event with `owner_state: live`,
   `basis: owner-consented-handoff`, and the handoff path and SHA-256 digest. A handoff is also how
   an owner leaves resumption notes, so reading one does not by itself mean the owner released the
   slot: run `remove` only for a slot whose owner said it is done with it. `retire-pending` applies
   this release only when given `--include-owner-consented`; without that flag a scheduled batch
   keeps retaining live-owner slots even when it passes `--coordinator-authorized`. An interrupted
   owner-consented removal must be finished by a `recover` from a `wrkslots` version that has this
   release. An older version refuses because the owner is still alive, and until a newer `recover`
   runs or the owner exits, the interrupted removal's journal blocks every other mutation.
7. A live owner that is done with an agent slot can give it back itself, without exiting, by
   running `release` after `write-handoff`. See "Giving a slot back while the owner keeps running"
   below. The release replaces only the owner-exit, registered-liveness, and time-to-live
   conditions of `remove`; the handoff read, the process-use census, salvage, and holds still apply.
8. Before removing an agent slot, `remove` publishes unpushed commits and tracked and ordinary
   untracked files outside configured regenerable cache paths to the recorded remote. It never
   uploads gitignored content. It records and rechecks the exact remote ref and commit before
   deletion. A validate slot skips this step.
9. `recover` lets any later participant complete an interrupted create or removal from the durable
   history. It is not tied to the participant that began the operation.

Run `wrkslots quickstart` for copyable commands and `wrkslots COMMAND --help` for exact effects and
inputs.

## Configuration file

`init` writes `.wrkslots.yml` at the project root as literate YAML: a header saying what the file
is, and a comment above every key explaining it, including the `sandbox` and `image` sections.
Optional keys that are absent keep their documented meaning, and the file ends with a list of
them.

The reader accepts a strict YAML subset and refuses anything else with the file and line:

- accepted: full-line and trailing comments, block mappings nested by space indentation, block
  sequences (of scalars, of one-line flow collections, and of mappings), one-line flow sequences
  and mappings (`[a, b]`, `{}`), `null`/`~`, `true`/`false`, integers, floats, double-quoted
  strings with JSON escapes, single-quoted strings, and one-line plain strings;
- refused: tab indentation, anchors, aliases, tags, directives, block scalars (`|`, `>`),
  multi-line plain or flow values, complex keys, several documents, and duplicate keys.

`yes` and `no` are strings, as in YAML 1.2, so `protect_system: yes` is refused rather than read as
true. A file whose first non-blank character is `{` is JSON, the format projects created before
YAML support use. Every command reads both formats through one loader, with the same strict key
validation.

Commands that rewrite the file (`init --repair`, `sandbox write-defaults`, `image set-default`,
`config convert`) keep its format: a JSON file stays JSON, and a YAML file is rewritten in the
canonical literate form. **Hand-written comments are not kept.** The previous bytes are saved to
`.wrkslots.yml.bak` first. Convert an existing project, or regenerate the comments after an upgrade,
with:

```sh
wrkslots config convert --to yaml      # or: --to json
```

`init --config-format json` still creates a JSON file.

## Coordinator assignment and creation

`create`, `register`, and `import-existing --apply` require both `--slot-type` and
`--coordinator-authorized`. Omitting either refuses before any worktree or lifecycle record changes
and tells the caller to ask the coordinator. This prevents accidental self-allocation; it does not
pretend that same-user processes have different operating-system permissions.

An assigned agent can run `create` with `--owner-pid` naming itself or an ancestor of the command.
Its live `--coordinator-pid` may run separately, including in another terminal pane; process
ancestry is not evidence of that assignment. The explicit flag records the assignment, while
the owner ancestry check prevents the agent from binding an unrelated process as owner.

Choose an owner process whose lifetime represents that one assigned agent. A shared multiplexing
supervisor, coordinator, or application server may be an ancestor of every worker command, but it
cannot identify which logical agent is alive; binding several agent names to that PID leaves every
row live until the shared process exits and defeats per-agent liveness and reclaim. Separate
coordinator and owner processes must be visible in the same Linux PID namespace so their `/proc`
identities can be read and rechecked.

The coordinator can instead invoke `create` to bind another live child owner. That path requires
the coordinator to be in the command's ancestry and the owner to descend from the coordinator.
Omitting `--owner-pid` also requires an invoking coordinator; the owner then immediately runs
`adopt` itself. Both exact process identities are checked after acquiring the mutation lock and
again before publishing the row. If either identity changes during provisioning, creation
refuses publication and retains the recovery journal.

When the owner runs in a dedicated transient systemd scope, `create`, `register`, `adopt`, and the
live form of `import-existing` can record that stronger boundary. Pass both
`--task-scope-unit UNIT.scope` and `--task-scope-invocation-id HEX32`, or neither. The owner PID is
required, its recorded cgroup path must lie under the invoking user's systemd manager
(`/user.slice/user-$UID.slice/user@$UID.service/`), every path component between the manager and the
scope must be a `.slice` (a scope inside another unit's delegated subtree is refused), the path must
end in that exact scope unit, and `HEX32` is the lowercase 32-hex `InvocationID` reported by
systemd. When the arguments are parsed and again immediately before publishing the row, wrkslots
reads `/run/user/$UID/systemd/units/invocation:UNIT.scope` without following a replacement and
requires its stable symlink target to equal `HEX32`. It then records the scope unit, invocation ID,
cgroup path, boot ID, and owner PID/start-time generation together. If a crash leaves a create or
live import journal, recovery repeats this link check immediately before it would publish the ACTIVE
row, and create recovery also checks it before provisioning any worktree or running hooks; a
missing, replaced, or retargeted link refuses publication and leaves the journal for inspection.
When the scope has really ended, discard the unpublished operation instead:
`recover --slot SLOT --abort-create` removes a create's unchanged provisional worktrees, and
`recover --abort-import` discards an import journal, which never changed any files. For example,
after placing the owner inside `agent-slot01.scope`:

```sh
TASK_SCOPE_UNIT=agent-slot01.scope
TASK_SCOPE_INVOCATION_ID="$(systemctl --user show \
  --property=InvocationID --value "$TASK_SCOPE_UNIT")"
wrkslots create slot01 --slot-type agent --coordinator-authorized \
  --agent codex-1 --task task-123 --purpose "fix parser" \
  --coordinator-pid "$COORDINATOR_PID" --owner-pid "$OWNER_PID" \
  --task-scope-unit "$TASK_SCOPE_UNIT" \
  --task-scope-invocation-id "$TASK_SCOPE_INVOCATION_ID" \
  --repo product=product --branch product=codex/fix-parser
```

These options describe a live scope; they do not apply to a `--from-state-file` import. The
`wrkslotsd` shadow observer accepts the recorded identity only as a captured claim: it does not reread
`/proc/<pid>/stat`, the current boot ID, the systemd invocation link, or cgroup existence at
evaluation/read time, and it has no fresh repository/task/owner/attempt-bound TaskGraph claim input.
It therefore classifies every otherwise unblocked row as `UNKNOWN`; rows without task-scope identity
remain readable and are also `UNKNOWN`. `wrkslotsd plan` is diagnostic and
cannot authorize automatic reclaim. Its `--config` input is a separate shadow-policy document, not
`.wrkslots.yml`; Python remains authoritative for registry-dependent path/layout/landed-ref checks,
Git-registration discovery, and initialized nested-repository inspection.
An append-only interrupted operation remains visible as a `BLOCKED` decision even when no ACTIVE
row was ever published; unavailable generation, active-record digest, and heartbeat fields are
reported as null instead of being invented. A plain rebuild without `--config`/`--evidence` also
reports this blocker and adds `POLICY_INPUTS_MISSING`. Once an index holds policy evidence, a plain
rebuild of that index is refused rather than discarding the evidence; delete the disposable index
file first to rebuild it without policy inputs.

`--coordinator-authorized` on `remove` and `read-handoff` is optional provenance when the recorded
owner is dead. It is required for an owner-consented release of an agent slot whose recorded owner
is still alive (step 6 above), and when `recover` starts a new cleanup for an unregistered
validation path, because that operation has no ACTIVE row naming who allocated it. Resuming the
durable journal does not require the original flag or coordinator; otherwise a departed coordinator
could strand an interrupted cleanup.

`status` always returns the complete readable roster and reports each observable directory or
Git-registration disagreement as a typed inconsistency. Git registrations are observable only for
source repositories named by readable rows; config does not contain a repository inventory.
`create --repo` supplies the requested repositories, so create also reports unrelated managed-root
registrations it discovers there. `create` refuses an overlap with the requested name, slot
path, checkout path, or Git registration. The exact selected source worktree is the sole ancestor
exception when it contains the configured managed root. Create may proceed at a distinct target
while printing every unrelated inconsistency it retained. `read-handoff` validates the complete
authoritative history and the requested slot's record, directory, and Git worktrees, but unrelated
storage drift cannot withhold that slot's handoff. When importing a snapshot with no event log, it
first records the structurally validated snapshot without resolving unrelated repositories, after
separately validating the requested slot's repository and worktree. It records no change to unrelated
rows. Other
lifecycle mutations refuse by default if either managed directory contains a worktree without an
active wrkslots record. During a deliberate migration,
put the global `--allow-existing-unregistered-worktrees` flag before such a command. The requested
operation then touches only registered slots and prints how many unregistered directories it
retained. It never uses that flag as permission to inspect, select, or remove one of them. Run
`wrkslots audit --format json` and import each live slot only after verifying its process evidence.

`wrkslots audit --gate` is the lifecycle-read-only coordinator reminder. It exits 1 for reclaimable,
interrupted, or unregistered slots; exits 2 when an expired slot cannot be classified because
evidence is unavailable; and exits 0 only when neither condition exists. The output names the
affected slots and the next command. It never converts an unknown result into permission to remove.

A registered row whose slot directory is absent -- `lstat` reports that nothing exists at the path
-- is reported as `RECOVERABLE` rather than `BLOCKED` when absence is its only remaining reason
after the same liveness, owner, heartbeat, handoff, and process/path census checks that make a
present slot `DELETABLE`. Its reason names the exact read-only plan to run on the row's machine:
`wrkslots recover-absent-agent-rows --row SLOT=GENERATION=SHA256` for an agent row, or
`wrkslots recover-absent-validate-rows --input FILE` with the complete one-row JSON for a validation
row. That command re-proves every storage, Git, and process condition before it retires the row;
audit's classification grants no authority. `RECOVERABLE` rows count as reclaimable for `--gate`.
An absent row that fails any other check stays `BLOCKED` and still names the recovery command. An
owner release does not stand in for the owner's exit here: both recovery commands require the
recorded owner generation to be dead, so an absent row whose releasing owner still runs stays
`BLOCKED` and says so. A slot path that is occupied by anything other than a real directory -- a
symlink, even a dangling one, an ordinary file, or a path `lstat` cannot inspect -- is never absent:
it stays `BLOCKED` or refuses the audit exactly as before.

Audit may update only its regenerable, project-keyed cache-accounting census below
`XDG_CACHE_HOME` (or a canonical absolute path beneath an existing symlink-free parent outside the
managed project supplied with `--cache-census-state`); it does not change a
registry, worktree, hold, journal, handoff, or Git repository. `--cache-work-limit`
(default 100000 work units) and `--cache-wall-seconds` (default 5 seconds) bound the
cache binding and traversal phase, not the end-to-end audit: registry reading, bounded
state decoding/authentication, and persistence occur outside that allowance.
The same two values separately bound cache-glob planning, which expands each subject's
cache globs before the census: every directory listing is one work unit, and the time
spent walking is charged. Each subject may use an equal share of what earlier subjects
left, so with many subjects a share can be small. Git observations made while planning
are not charged. The planning time bound is checked between directory listings, not
enforced by a worker, so a listing that blocks is not interrupted. A subject whose
planning exhausts its share reports `cache_status: "error"` with null bytes; its
retained census progress is not carried forward. A checkout whose ignored tree needs
more listings than its share reports that error on every audit at the same limits.
Root binding and traversal run in isolated workers with the remaining deadline
enforced even if a filesystem operation blocks. Expiry starts no further work and cancels
the worker, with at most one additional second allowed for reaping it.
Until the census has visited and reverified every directory and entry, JSON reports
`cache_bytes: null` and `cache_status: "partial"`, keeping the row out of `DELETABLE`.
An authenticated subject cursor gives lifecycle subjects fair turns, including subjects
with multiple cache roots. Each turn includes binding and deadline-checked structural
validation, so an expensive peer cannot consume every call before a cheap subject runs.
Deferred subjects retain authenticated progress but report null bytes until checked.
Measurement and recursive verification resume across calls;
finalization reserves a fresh sweep of every directory and file of all roots belonging
to that subject in one invocation. No persisted completed total supplies current evidence.

If that subject's final sweep needs more work than the fixed allowance, or exhausts an
entire fresh binding/traversal wall allowance, the result is `cache_status: "error"` with null
bytes and a specific capacity error. It never cycles indefinitely as partial or publishes
a stale total. Binding or traversal that exhausts the wall allowance before staging
also reports a capacity error, including a blocked operation that made no progress.
Work debits and observation counters survive cancellation. Stable subjects whose
fresh sweep fits can finish at an unchanged budget,
even when measurement and verification together require several invocations. Partial
finalization restarts for the whole subject; previous calls' final checks are never reused.

The state is keyed by active-registry revision and cache-root identity. Persisted progress
is authenticated by a sibling 0600 key; missing, malformed, too deeply nested, obsolete, or
altered state restarts the bounded census. Both writer and reader enforce a 64 MiB state
limit. A state too large to persist produces null/error accounting instead of repeatedly
writing unreadable progress. Unavailable default cache storage also produces null/error
accounting while preserving lifecycle rows, holds, and other refusal reasons. Interrupted
first key writes remain in one private pending file that a later census can rebuild;
only a complete, synced key is published atomically. Unsafe pending files and malformed
published keys remain refusals. Invalid
explicit state-path arguments remain CLI refusals. JSON reports typed CPU seconds, wall
seconds, and work counters for the registry, liveness, process-census, cache-planning,
cache-census, registered-row, and storage phases.

## Git hook installers in new submodules

A repository can ship its own Git hook installer as an executable `scripts/setup-hooks.sh`, which
typically sets `core.hooksPath`. A setting made once in the primary checkout reaches every linked
worktree of that repository, because they share one configuration. A submodule initialized inside
a new slot is different: its Git directory lives under that worktree's own Git directory
(`<common>/worktrees/<name>/modules/...`), so it starts with no hook setting at all.

After `create` has registered the slot, and so after the post-provision hooks have initialized
submodules, it looks at each new checkout and every initialized submodule below it, recursively.
It runs `scripts/setup-hooks.sh` in a repository when all of these hold:

- the script is a regular file, not a symlink, with the owner-execute bit set;
- the repository's common Git directory lies inside the new checkout's own Git directory, which
  is true of a submodule initialized in this slot and false of the checkout itself, whose
  configuration every slot shares;
- the repository's own configuration sets no `core.hooksPath` yet. Hooks that a post-provision
  hook already installed there (for example a project's own hook dispatcher) are left in place
  rather than replaced by the repository's installer.

The script runs from that repository's root, with stdin closed, with `GIT_DIR`, `GIT_WORK_TREE`,
`GIT_INDEX_FILE`, `GIT_COMMON_DIR`, and the other variables that would point Git elsewhere removed
from its environment, and for at most 60 seconds; then its process group receives SIGTERM, and
SIGKILL two seconds later. It runs outside the mutation lock, and nothing it does can fail or undo
the creation. `create` still exits 0, and reports every outcome:

```text
hooks: ran scripts/setup-hooks.sh in /work/slots/s1/product/sub (rc 0)
hooks: WARNING scripts/setup-hooks.sh in /work/slots/s1/product/sub failed (rc 7); the slot was created without these Git hooks
hooks: output tail:
hooks: | the last lines of its combined stdout and stderr
hooks: skipped; no repository private to this slot has an executable scripts/setup-hooks.sh (3 checked)
```

A checkout whose own script was not run because its configuration is shared, or a repository
whose hooks were already configured, gets a `hooks: not run:` line. With `--format json` the lines go to stderr, and the result object gains
`setup_hooks`: `script`, `timeout_seconds`, `repositories_checked`, and one entry per repository
that has the script, with `path`, `status` (`ran`, `failed`, `timed-out`, `failed-to-start`,
`not-executable`, `not-run-shared-git-config`, `not-run-hooks-already-configured`, or
`inspection-failed`), `returncode`,
`output_tail` (at most 20 lines from the last 4 KiB), and `detail`. A creation finished later by
`recover` does not run installers; run the script by hand in that case.

## Time-to-live and process evidence

`init --heartbeat-ttl-seconds SECONDS` records the default copied into every new slot. `heartbeat`
updates the durable renewal time only for the exact owner process generation. Expiry is one required
reclaim fact, never the whole decision.

The registered running command answers for agent slots only. It receives the agent name as its only
positional argument and receives `WRKSLOTS_PROJECT_ROOT`, `WRKSLOTS_SLOT`, `WRKSLOTS_SLOT_TYPE`
(always `agent`), `WRKSLOTS_TASK`, `WRKSLOTS_AGENT`, `WRKSLOTS_MACHINE`, generation, and owner
identity fields in the environment. Its exit status means:

- `0`: the registered mechanism verified the agent is dead;
- `1`: the agent is alive, but only when standard output and standard error together hold exactly
  one non-empty line whose only `agent=` token is `agent=<the requested agent>` and whose only `rc=`
  token is `rc=1`. An uncaught Python exception also exits `1`, so any other output with status `1`,
  including a traceback, is treated as `2`;
- `2`: the mechanism cannot determine the answer;
- anything else: the check failed.

Only `0` satisfies that reclaim condition. (`recover-absent-agent-row`, which acts only on a row
whose storage is already gone, has one narrow exception for `1`, described under "Recover absent
agent rows and ownerless agent worktrees".) The exact recorded process generation must independently
be dead, and the full process-use scan must find no cwd, executable, root, descriptor, mapping,
cgroup, or mount use. If the liveness source is degraded or stale, return `2`; unknown ownership is
not a free slot.

Large registries should configure `init --liveness-batch-command PATH`. Audit then invokes that
project-owned command once, with a `wrkslots-liveness-batch-request/v1` JSON object on standard
input. Every request subject contains the agent and the complete environment that the
single-subject command would receive; its SHA-256 `subject_id` binds those exact values. The command
must return `wrkslots-liveness-batch-response/v1`, echo the request SHA-256, and provide exactly one
result for every subject with the same subject ID and agent, a state of `dead`, `alive`, or
`unverifiable`, and one bounded detail line. A missing, duplicate, unknown, malformed, or misbound
result makes every subject in that batch unverifiable. There is no permissive fallback after a
configured batch command fails. Configurations without the optional command retain the established
per-agent protocol.

A validation slot's `agent` names a run, not a process that a liveness command can find, and a
validation may outlive the agent that launched it. Neither registered command is asked about
validation slots. `audit`, `remove` (including `--validate-complete` and `remove-validate-batch`),
`recover`, and `recover-unbound-owner` ask the run itself, using the same evidence as
`recover-absent-validate-rows` (see "Recover registered validation rows with absent storage"):

- `dead`: every retained run handle under `ignored/validate/runs/` whose checkout names the slot
  directory or a recorded checkout path (its `checkout` or `source_checkout` is one of those paths
  or a path inside one, compared as a unit word is below, so a symlink to the slot or a directory
  inside it names the slot too) has an inactive, unqueued service unit with no live process in its
  control group and, when it records a process generation, one that did not run when it was read
  just before the process tables and units and does not run at the end, and no active or queued
  user-systemd unit names those paths. Removal renames the slot directory to
  `.<slot>.fenced.<generation>.<hex>` before deleting it, so the same paths inside every such fence
  on disk, and inside the fence an interrupted removal's finish journal names, count as the row's
  paths too;
- `alive`: any of that evidence shows the run may still use the checkout; the remedy is to let the
  run finish or stop its unit;
- `unverifiable`: a handle, the process table, user-systemd state, the boot id, or a recorded run
  process's generation cannot be read completely (unreadable evidence shows no run, so it is not
  reported `alive`), a handle file changes or is replaced while one read reads it, a handle repeats
  a JSON field (a repeated field has no single meaning, and keeping the last value could hide a live
  process or move the handle off its row), the row is registered on another machine, or its
  coordinator lease or owner names another stable host identity (an ownerless row is judged by its
  lease, as in `recover-absent-validate-rows`), or this process's view of the host is restricted: it
  is not in the host's initial PID or cgroup namespace, the mount visible at `/proc` is not the
  process filesystem of its PID namespace, `/proc` is mounted with `hidepid`, or a mount that path
  lookup reaches covers a process's directory `/proc/<pid>` or a path inside it (one that a later
  mount over `/proc` hides covers nothing). In any of those a host run can be missing from the
  process table, so an empty table is no evidence. All of this evidence is local to the host, so a
  host without a reachable user service manager cannot prove a validation slot free.

A unit names a row path when one of the words of its property strings is that path or a path inside
it. Each element of a property (one command-line argument, environment assignment or path), each
line of an element, and a property string as a whole are words, and so are their shell words, read
once as the shell splits words and once with the control operators `;`, `&&`, `|` and the others as
words of their own (so `cd '<path>'; make` holds `<path>`). A shell word that holds quotes,
whitespace or an operator is read again as shell text, up to three times
(`bash -c "cd '<path>' && make"`). Each of those is also split into words at whitespace, quotes, and
the characters `=`, `:`, `,`, `;`, `&`, `|`, `(`, `)`, `<`, `>`, `{`, `}` and `$`, and split again
at the same characters except whitespace, so a row path containing a space is still one word in
`--checkout=<path>` or `cd '<path>'`. The rest of each element and shell word from just after every
`=`, and after every one of those characters that `/` or `~` follows, to the end of the element
however long it is, is a word too, so `--checkout=<path>` and `NAME=<path>` keep a path that
contains `:` or a quote. A leading `-`, `!`, `@` or `+` is also tried without it. The row path's own
spellings (below) are also found as text anywhere in a property string, when they begin it or follow
one of those characters and end it or precede `/` or one of those characters, so a row path is found
whole even where it contains a newline or ends just before a `:`. A path that goes on from one
names the row even where it leaves the row again through `..` (`<row>/../slot02`), since the kernel
looks the row up on the way and the path stops resolving once the row is gone. Every reading only
adds words, so a path that contains one of these characters can name a row it is not inside
(`slot01:other` names `slot01`) and refuse falsely, but no reading hides a row path's own spelling.
A symlink to a row whose own name contains one of these characters can still go unmatched by the
symlink's spelling when another of them follows it in the same shell word (`PATH=<symlink>:/bin`),
and a path the shell computes from a variable, a glob or a command substitution is not read.
A relative word is resolved against the unit's working directory (the home directory when it has
none), and `~` is the home directory. Each word is then compared as a whole path: its normalized
spelling, its symlink-resolved spelling as written (a `..` after a symlink leaves the symlink's
target, as the kernel reads it) and the symlink-resolved spelling of its normalized form, against
the row path's spellings formed the same way, by complete path components, and, where the path
exists, the word and each of its existing ancestors by device and inode against the row path
itself. Each path and file that a resolution looks up on the way counts as well, as the kernel needs
every component of a path to exist: a word whose resolution passes through the row names it even
where it ends outside the row (`worktrees/validate/slot01/../slot02` with the project root as its
working directory, or `<symlink to slot01>/..`). A unit naming `slot01/./product`,
`--checkout=slot01//product`, a symlink to `slot01`, `<symlink to slot01/product>/..`, or
`worktrees/validate/slot01` with the project root as its working directory therefore names `slot01`;
one naming `slot010`, `slot01+other` or `/other/<project>/worktrees/validate/slot01` does not.
A word's symlinks are resolved as Python's `os.path.realpath` resolves them, however long the word
is, except that no
component below one that is missing, not a directory, a symlink loop, too long or unsearchable is
looked up, since every such lookup fails; a link target that cannot be read, or more than 40
symlinks followed inside each other, refuses. Its ancestors are looked up from `/` down, stopping
below one that is missing, not a directory, a symlink loop or too long. One judgement may read
33,554,432 characters of words and the paths made from them, make 262,144 lookups (`lstat`, `stat`
and `readlink`), and take 20 seconds; past any of these bounds it refuses, so the row is
unverifiable. The time bound is checked throughout, including every 4,096 characters that the shell
word reader reads (about every 8 milliseconds on the measurement host) and each property and path
answered from what the judgement has already read, and once more as the judgement ends, so that a
row identity read or a property scan that ran past the bound gives no answer. Because every value
is read to the end of its element, a search
list's characters grow with the square of its length: a 130,001-character `NAME=/a:/b:...` list of
10,000 entries refuses, while the 77 active units on the measurement host, with 155,976 characters
of properties, read about 8.5 million characters, a quarter of the bound, and made about 4,950
lookups in 0.4 seconds (2026-10-06). This rule is shared with
`recover-absent-validate-rows`, and with a retained run handle's checkout, which the handle census
matches with the rows within its 30-second time bound; that bound starts before the rows' own paths
are read, and also bounds the matching in place of the 20 seconds.

The process table is read both before and after the user-systemd enumeration, which takes about a
second and a half on a busy host. A run that starts and finishes during the enumeration leaves its
unit inactive and unqueued and may leave a child in the unit's control group; only the later table
shows that child, so the later table is the one judged. A process generation present in both tables
keeps the control group it had in the earlier one, and a generation present only in the earlier
table has exited. The later table is itself read one process at a time after its list of PIDs, so a
run that starts during it can be absent from both tables while the first enumeration reads its unit
inactive. The user-systemd units are therefore enumerated again after the later table, and a
retained run unit that is active or queued in either enumeration is `alive`. The retained run
handles are read again after both enumerations, and a handle found by either read is judged. A
handle that only the later read finds, one whose file differs between the two reads (its bytes,
device, inode, size, or modification or change time, so a handle rewritten in place or replaced by
an identical file counts), and one that the later read no longer finds naming the row were written
or removed after both enumerations had begun, so their unit states do not cover its run: its unit
can be queued after them with its job still waiting to start, showing no state, process or member.
Such a handle is `alive`, and a rerun reads it before the units. A handle rewritten with the same
bytes within one file timestamp tick keeps all of those, so when a handle of the first read changed
within the last two seconds (by this host's clock, which the kernel uses for file times), that is
waited out and the handles are read again before the units: a run that has just written its handle
as it finishes is judged, not refused. A handle that appeared, changed or went between the reads
before and after that wait is `alive` as well. A handle that both reads find alike while either
finds it that recent (the clock stands still or went back, or the change time is more than a quarter
second ahead of it and is not waited for) is `alive` too, and so is one that both reads find alike
while this host's realtime clock may have stepped more than half a second against its monotonic
clock during or between them: after a step back into a handle's timestamp tick, a run that registers
again alike leaves both reads equal. Each read of the handles reads both clocks before its first
handle and after its last, and a change time is recent against the earliest realtime value either
reading took. Each reading takes the realtime clock between two readings of the monotonic clock,
which bounds the clocks' offset at that moment; a scheduling pause between the reads widens the
bounds instead of moving them. A step is assumed when the bounds of the four readings that the two
reads take span more than half a second, so a fall within either read (before or after its handles)
or between them counts. A step forward or a suspend (the monotonic clock stops while the host
sleeps) counts too, and so do pauses between the clock reads that together widen the bounds past
half a second; each keeps the run `alive`. A step back does not show if it is undone, or undone to
within half a second, before a realtime read sees it. After the last read of the handles before the
process tables, and just before those tables, the process generation that each handle records is
read from its own entry under `/proc`. The process tables list thread groups and skip terminal
ones, so they show neither a recorded thread ID nor a thread group leader that has exited while
another thread of its group runs; that entry shows both. A recorded generation that runs then is
`alive` even when it exits before the end, since it may queue its unit after the second
enumeration; one that does not run then cannot act later. Last, the current members of every
control group named after a handle's unit,
and of its descendants, are read from the cgroup v2 hierarchy below the user service manager; a
member there is `alive`. A host without a cgroup v2 hierarchy at `/sys/fs/cgroup`, whose cgroup v2
mount there shows only a subtree of the hierarchy (control groups outside it would read as empty),
with a visible mount below `/sys/fs/cgroup` (a file system over a control group, or a file over its
`cgroup.procs`, replaces what the hierarchy shows there), whose user service manager's control group
is not visible there, or with an unreadable member list is `unverifiable`.

These reads find a run from the moment its unit is active or queued, or while the process
generation its handle records runs. They cannot tell a run whose handle is written before its unit
is queued from a run that finished: both leave a handle that every read finds alike, with
no unit, process or member, and waiting longer does not tell them apart. The wait above does not
either: a handle rewritten alike within its timestamp tick during the wait reads as before. A
`dead` answer therefore relies on a rule for whatever writes a retained run handle: after each
write, including one that leaves the bytes alike, the run must be visible to this evidence (its
unit active or queued, or the process generation that the handle records in `process_identity`,
its `pid`, `start_ticks` and `boot_id`, running) before the write's change time is two seconds old
by this host's clock, which can be one second after the write where file times keep whole seconds.
A launcher meets the rule when it queues the unit before it writes the handle; when it writes the
handle from inside the handle's own unit, or from inside another unit that names the checkout and
stays active or queued without a break until the handle's unit is queued; or when the handle records
a process generation that runs until the unit is queued. Visibility must not lapse in between: a
unit that names the checkout, writes the handle, hands the queueing to another process and ends
before the handle's unit is queued leaves a moment with no unit, process or member, and a judgement
then can answer `dead`. A launcher that writes the handle and only then queues the unit, with no
such process recorded, can have its row judged `dead` when the queueing takes longer than that.

`recover-absent-validate-rows` and `recover-absent-agent-rows` read this evidence through the same
code and in the same order, and refuse wherever the answer above would be `alive` or `unverifiable`:
each first requires the unrestricted host view described above, reads the retained run handles
before and after the process tables and units, and reads the handles' control-group members last. A
validation run can use an agent checkout too, so `recover-absent-agent-rows` judges every retained
run handle whose checkout names the agent row's paths and, like `recover-absent-validate-rows`,
refuses when any handle under `ignored/validate/runs/` cannot be read or parsed.

Audit reports the number of validation rows it judged this way as the liveness phase's
`validation_run_subjects`. A batch removal reads this evidence once while sealing and once more per
slot immediately before deletion. A churning user-systemd snapshot refuses rather than guessing;
rerun the removal. As for agent slots, only `dead` satisfies the reclaim condition, and the exact
owner generation and the full process-use scan must independently agree.

A slot imported from an older state file with no recorded owner identity cannot satisfy the
owner-death condition, even after its heartbeat expires. `recover-unbound-owner` can record what was
inspected but does not turn unavailable process evidence into proof of death. Preserve such a slot
and name it in the migration remainder rather than inventing an owner.

## Reclaim configured caches

`clean-caches` operates only on directories selected by the configured global or per-repository
cache globs. It removes those regenerable directories, not a checkout or slot, and therefore does
not require the slot owner or registered liveness source to be dead. A hold still protects the
selected slot and reports `HELD` without deleting its caches.

A name is not enough to make a directory a cache, so `clean-caches` deletes less than its globs
select. Selection itself, with every refusal it raises, is unchanged and is shared with slot
retirement. Of the selected directories, `clean-caches` keeps any that lies inside a package Cargo
unpacked: the directory, or one above it in the checkout, holds Cargo's `.cargo-ok`. A package can
carry a source directory named `target`, and deleting it leaves Cargo unwilling to unpack the
package again. It also keeps a directory that only a glob with a wildcard (`*`, `?`, `[`, or `**`)
selected, unless a build tool marked it as a cache: a `CACHEDIR.TAG` with the standard signature,
or Cargo's `.rustc_info.json`. A glob without wildcards names one path and keeps its plain meaning.
JSON rows list each kept directory and its reason under `skipped`.

With neither selector, the command is a read-only report over all visible registered, journaled, and
unregistered slots. `--only SLOT` is a destructive selector even though it does not use `--yes`; it
may be repeated, emits rows only for the named slots, and refuses if any name cannot be attributed.
`--yes` instead selects every unheld slot. The two selectors are mutually exclusive. Human output is
the default; `--format json` emits the same typed actions, paths, allocated-byte counts, and total
removed bytes for automation.

Every form takes the exclusive project locks while it reads control-plane state and resolves holds,
replaying each machine's event log once for all of that machine's slots. The read-only report then
releases those locks before it measures cache trees, so a long walk does not block other slot
mutations. Its rows are a snapshot: a slot that changes or disappears during the walk reports
`BLOCKED` with its error, and a hold placed after the locks were released is not reflected. `--only`
and `--yes` keep the locks through measurement and removal.

```sh
wrkslots clean-caches --format json
wrkslots clean-caches --only slot01 --only slot02 --format json
wrkslots clean-caches --yes --format json
```

Targeting is narrow at the storage layer, but authority checking remains global. Before mutation,
`--only` still checks partial control-plane writes and replays every authoritative
`EVENTS.<machine>` shard so an event-only slot or recovery operation cannot be hidden by a stale
compatibility view. It then avoids checkout, Git, and cache-tree traversal for unselected slots. A
global validation runs `_assert_record_paths` for every active row, including unselected rows, so
stored paths cannot escape their managed roots. A scoped `CREATE.*.journal` or
`FINISH.*.journal` carries its length-framed machine and slot in the filename; wrkslots still reads
the complete bounded journal, validates its shape, and requires an exact match with append-only
progress evidence before skipping its checkout, Git, and cache inspection. An unscoped compatibility
`ACTIVE.<machine>.journal` does not carry a slot, so wrkslots performs the ordinary bounded journal
read and validation to discover its target before deciding whether it is selected. A malformed or
ambiguous journal cannot be attributed safely and therefore refuses.

The report is bounded by the configured project and the repositories named by readable rows; the
configuration is not a host-wide Git-worktree registry. Cache globs are operator declarations, not
automatic proof that matching bytes are regenerable. Tracked overlap, unsafe paths, and nested Git
metadata under a selected cache root refuse cleanup. In report-only mode, malformed or otherwise
uninspectable unregistered directories remain visible as `BLOCKED` rows. With `--only`, unselected
rows are intentionally omitted from output and their cache trees are not traversed; global event,
active-record path, and complete bounded journal/provenance checks still run.

## Handoffs and retirement

Write a new handoff outside the checkout while its exact owner is still live:

```sh
wrkslots write-handoff SLOT --agent AGENT --owner-pid PID \
  --expected-generation N --from-file /path/to/HANDOFF.md
```

The input must be a regular UTF-8 file outside every Git working tree. This prevents a tracked,
shared repository document from being mistaken for slot-specific retirement intent. Direct-child
`HANDOFF.md` discovery likewise refuses a Git-tracked file; an untracked direct-child
file remains readable for migration.

The bounded UTF-8 contents go to a generation-bound control-plane sidecar, so writing the handoff
does not make a clean checkout dirty. The sidecar records the source classification, absolute path,
and exact file identity alongside its bytes. Before exposing a publication temporary file,
`write-handoff` records an owner-authenticated intent containing that complete deterministic
envelope; only exact canonical verification records completion. Reads accept the sidecar only when
both events match it. The sidecar is immutable once published; `finish` still refuses every tracked,
untracked, or ignored checkout change. In particular, it does not exempt a pre-sidecar `HANDOFF.md`
inside a flat-layout checkout.

`wrkslots read-handoff SLOT --coordinator-pid PID` prints the exact sidecar contents and durably
enqueues that slot generation for later retirement. Reading never removes the sidecar, an existing
checkout file, or the slot. If no sidecar exists, the command copies a direct-child `HANDOFF.md` into
the sidecar without moving or unlinking the old file. If both exist, their bytes must agree.
The command does not hold the state lock while it writes up to 1 MiB to the caller: it snapshots the
exact generation and file identity, writes and flushes the bytes, then reacquires the lock and
revalidates both before recording the read. A failed flush or concurrent change records no new read.
When the checkout file genuinely changed after an earlier read, ordinary read refuses and keeps
both copies. Published sidecars are immutable: the deprecated adoption option also refuses, so no
command silently replaces either artifact. An operator must preserve and reconcile the disagreement
outside this destructive workflow before a fresh slot can provide new retirement evidence.

Inspect the queue with `wrkslots retirement-queue --format json`. A coordinator may attempt a
bounded group with `wrkslots retire-pending --limit N --coordinator-pid PID --format json`. Each
item runs the ordinary removal state machine independently; a live owner (unless it released the
slot, or `--include-owner-consented` is given and step 6 above holds), fresh heartbeat, hold, process use,
changed handoff, dirty or unpublished work that cannot be salvaged, remote mismatch, or path-fence
race retains the slot in the queue. Successful archived removal clears its sidecar.
Attempt events rotate blocked entries behind never-attempted and less-recently-attempted entries, so
one retained slot cannot starve the rest of a bounded queue. Lock contention is deferred; corrupt,
partial, or indeterminate state stops the batch and requires recovery instead of being mislabeled as
retained. A defect in one item's own storage that removal finds in its first check, before it
changes the slot, such as an active row whose slot directory is gone or a Git worktree registered
inside the slot, is reported as could-not-determine with recovery required. The batch then goes on
to its next item, but only after it reads the registry again under the lock and finds no partial
update, no mutation journal, and the item's row exactly as that check read it; any refusal while it
does so, including a registry file it cannot read or decode, stops the batch, which still reports
what it did. The same defect found at a later step of removal, after it has written the registry,
stops the batch.
Sidecar cleanup atomically renames the exact inode into content-addressed retired control
storage and never unlinks that retired pathname, so a same-UID replacement cannot be mistaken for
the acknowledged artifact. A direct-child checkout handoff is likewise moved by no-replace into an
identity-bound retired path before its fenced slot is removed; pathname recreation preserves both
copies and refuses. The exact handoff bytes remain in append-only history.

## Giving a slot back while the owner keeps running

An agent that finishes with a slot often keeps running, for example to take its next task in a new
slot. Without an explicit release its old slot stays until the process exits and the heartbeat
time-to-live expires. `release` lets the exact live owner give one slot generation back at once:

```sh
wrkslots write-handoff SLOT --agent AGENT --owner-pid PID \
  --expected-generation N --from-file /path/to/HANDOFF.md
wrkslots release SLOT --agent AGENT --owner-pid PID --expected-generation N
cd /a/directory/outside/the/slot
```

Who can release. The caller must be the recorded owner process generation or one of its
descendants, proven the same way as for `heartbeat`: the agent name and the full process identity
(PID, process start time, boot identity, and cgroup) must match, and `--expected-generation` must
name the current generation. `release` refuses a validate slot, a held slot, and a slot whose current
generation has no completed `write-handoff` (a direct-child `HANDOFF.md` alone names no writer, so it does
not qualify). It takes the state lock briefly, appends one event, and runs no process census.

What it records. One `owner-released` event bound to the slot, the generation, the agent, the owner
process identity, the heartbeat stamp at that moment, and the path, SHA-256 digest, and write intent
of the handoff sidecar. Rerunning `release` for the same handoff prints `already released` and
records nothing. A release stops applying, and removal returns to the ordinary conditions, as soon as
any of those bindings changes: another owner (for example after `adopt`), a renewed heartbeat, or a
sidecar that is missing or no longer those bytes.

What changes afterwards.

- The heartbeat stops for that generation. `heartbeat` refuses while the release applies, so a
  released slot cannot quietly return to use. A release cannot be withdrawn: the sidecar is
  immutable and the heartbeat stays stopped. To keep working, create a new slot.
- `remove` and `retire-pending` treat the released owner like an exited one for the owner-liveness
  condition only. They do not wait for the owner to exit, for the registered running command to
  report dead, or for the heartbeat time-to-live to expire; the time-to-live would only re-prove
  what the owner already declared, and the heartbeat cannot be renewed. `retire-pending` needs no
  extra flag for released slots.
- Everything that protects work still runs. The coordinator must first read the handoff with
  `read-handoff`, and the read must match the released digest. `remove` refuses while any process,
  the owner and its children included, has its working directory, root, executable, an open file,
  or a mapping inside the slot; only the comparison against the recorded owner's cgroup is skipped,
  because the releasing owner is alive there by definition. Slot contents, uncommitted handoffs, Git
  operation and remote checks, salvage before deletion, holds, and journals are unchanged.

Before deleting anything, `remove` appends an `owner-release-honored` active-state event recording
the release event it relied on, the owner state it observed, the registered running command's answer,
the heartbeat age, and the handoff digest. An interrupted removal of a released slot is resumed by
`recover` only when that record exists and the release still applies.

Where it shows. `status` adds `owner_release` to the JSON row and an indented
`RELEASED by live owner at <time> (event N)` line in text; `doctor` reports an `owner-released`
finding; `audit` reports the release on the row and judges it like `remove` does, so a released and
read slot that passes every other check is `DELETABLE`; `retirement-queue` adds `owner_release` to
the JSON entry and `owner=RELEASED by live owner at <time>` to the text line. A release that no
longer applies is shown with the reason instead (`owner-release-void` in `doctor`). None of these
outputs change for a slot that was never released.

Mixed versions. The release is a separate event kind, so versions of `wrkslots` without `release`
skip it when replaying history and keep working on every slot. They also keep their stricter rules
for a released slot: an older `remove` still waits for the owner to exit and the time-to-live to
expire, and a heartbeat renewed by an older `heartbeat` changes the bound heartbeat stamp, which
voids the release rather than being overridden by it. As with an owner-consented removal, an
interrupted removal of a released slot whose owner is still alive must be finished by a `recover`
from a version that has `release`; an older `recover` refuses and the journal blocks other mutations
until a newer `recover` runs or the owner exits.

## Remove a batch of agent slots

`wrkslots remove` checks process use several times for one agent slot while the registry lock is
held. `lsof +D` walks the whole slot and every open descriptor on the machine, so on a busy machine
one such check of a large slot takes many seconds: on a 33 GB slot of 68,888 entries, at load
average about 260, one took 9 to 18 seconds. Default `remove` of one agent slot is therefore a batch
of one, as described below: its one `lsof +D` runs before the lock is taken, and its work before the
first deletion has the same 60-second budget as each batch slot, with the same exceptions (listed
below). A batch of one refuses, without salvaging or removing anything, when its `lsof` scan is more
than 300 seconds old at its first process-use check under the lock. A slot that the invoking
`wrkslots` process itself holds open, for example through an inherited descriptor, is refused as
before. `remove --no-lock-budget` runs every check with `lsof +D` under the lock and has no such
limit; use it only for a slot that default `remove` refused for the limit. Validation slots keep
their existing checks. An image-backed agent slot, and any agent slot on a host without `lsof`, uses
the checks under the lock, since the fence of an image-backed slot mounts its image again, possibly
from another device. To remove many reclaimable agent slots, name each exact slot and its current
generation:

```sh
wrkslots remove-agent-batch --coordinator-pid "$COORDINATOR_PID" \
  --slot example-a=3 --slot example-b=1
wrkslots remove-agent-batch --coordinator-pid "$COORDINATOR_PID" --input /path/to/slots.txt
```

The input file holds one `SLOT=GENERATION` per line; blank lines and lines starting with `#` are
ignored, and `--slot` may be combined with it. A batch accepts at most 128 distinct slots.

Authority and every per-slot check are those of `remove`. Each slot goes through the ordinary
removal state machine in its own registry-lock hold: expired heartbeat, registered liveness, a
proven-dead recorded owner (or instead a release by the live owner, as described above, or, with
`--coordinator-authorized`, the owner-consented handoff exception), holds, handoff checks, salvage
of every commit to the recorded remote with read-back, the path fence, the nested-repository checks,
and the archive. The lock is released between slots, followed by a 0.25-second pause so that a
waiting heartbeat can take it. Each slot waits up to 30 seconds for the lock, or for `--wait-lock`
seconds when that is given.

Only the `lsof` scan is shared, and only for the process-use checks made before the path fence.
Before taking any lock, the batch runs one `lsof +D` over every requested slot directory and
attributes each open file to the slot that contains it; a file it cannot attribute counts against
every slot. Where `remove --no-lock-budget` would run `lsof` before the fence, the batch instead:

1. confirms that the slot directory is still the directory the shared scan covered, by device and
   inode;
2. refuses the slot if the shared scan saw any process using it, or if `lsof` printed a warning
   that may concern the slot, exactly as `remove` would refuse on that warning;
3. scans `/proc` afresh for a process whose working directory, root, executable, open descriptor,
   memory mapping, or mount table names the slot, and for a live process in the recorded owner's
   cgroup when that cgroup is evidence.

The fresh scan catches use that began after the shared scan. The shared scan catches a descriptor
opened through a hard link outside the slot, which no `/proc` path names; it is repeated when it is
older than 300 seconds. Nothing is deleted before the fence. A slot that the shared scan saw in use
is refused even if that process has since exited; run another batch or `remove` for it.

The process-use checks after the fence guard deletion. `remove --no-lock-budget` runs `lsof +D` on
the fenced path for each of them. The batch first scans `/proc` once instead. It records the device and inode of
every file and directory in the fenced slot, then compares each process's working directory, root,
executable, open descriptors, and memory mappings with the slot, both by path and by device and
inode. That is the comparison `lsof +D` makes, so a descriptor opened through a hard link, or a
directory reached through a bind mount in another mount namespace, is found although no path names
the slot. A Unix socket bound below the slot's path counts as use as well. When the scan cannot
decide, `lsof +D` decides exactly as in `remove --no-lock-budget`. The scan cannot decide when:

- the slot spans more than one device, or part of it cannot be read;
- the slot is on a filesystem other than btrfs, ext2, ext3, ext4, tmpfs, or xfs, since on others,
  such as overlayfs, a memory mapping can name a different inode from the one `stat` reports;
- on btrfs, a process maps a file whose inode number matches a file in the slot, since btrfs
  reports each subvolume under its own device and the mapping alone does not tell them apart;
- reading a process fails for a reason other than the process having exited or belonging to a user
  the caller may not inspect.

Processes that have exited, and processes the caller may not inspect, are skipped, as `lsof` run by
the same user skips them. As with `remove`, descriptors of other users' processes are visible to
neither check without privilege, and every process's mount table is read. The JSON report counts the
scans after the fence in `fenced_process_scans`, their total time in `fenced_process_scan_seconds`,
and the checks that `lsof` decided in `fenced_lsof_fallbacks`, with each reason in
`fenced_lsof_fallback_reasons` and their total time in `fenced_lsof_fallback_seconds`. Like the
checks `remove --no-lock-budget` makes after the fence, the scan and `lsof` read the fenced slot as
it is when they run.

An image-backed slot needs one more check (see "Disk-image slots" below). Its fence mounts the image
again at the fenced path, and the new mount can have another device number, so a process still
working in the old mount, for example through a detached bind alias in a mount namespace of its own,
matches nothing in the fenced tree. Immediately before the fence of an image-backed slot the batch
therefore runs the ordinary `lsof` check of `remove --no-lock-budget`, held to the slot's time limit
below, which finds such a process by device and inode while the old mount is still the slot's. The
JSON report counts those checks in `image_lsof_checks`, with their total time in
`image_lsof_check_seconds`.

Each slot's work before its first deletion is limited to 60 seconds, counted from when the slot
takes the registry lock, so that other clients waiting for the lock are not kept waiting past their
own limits. Each Git command, the registered liveness command, each nested-repository check, each
`/proc` scan, and `lsof` receive at most the time that remains. A command still running when the
limit is reached receives SIGTERM, then SIGKILL 5 seconds later if it has not exited, and is then
waited for up to 5 more seconds. The slot is refused and left in place, with its path fence rolled
back if it had been made; retry it later, or remove it alone with `remove --no-lock-budget`, which
has no such limit. A slot whose nested repositories take long to check can therefore be
refused by the batch and removed by `remove --no-lock-budget`. Three stretches are not limited, because stopping them
partway would leave a removal that only `wrkslots recover` can finish: the local Git registration
repair that follows the path-fence rename, a rollback of the path fence, and the deletion of the
slot. Once the first file is deleted, the slot's removal runs to completion, and its duration grows
with the number of files in the slot. The JSON report gives the limit in `item_budget_seconds` and,
for each removed slot, `seconds_before_deletion`: how long it held the lock before it began
deleting.

A refused slot is left in place and reported with its reason, and the remaining slots are still
attempted. A missing row, a changed generation, a validation slot, and a row whose directory is
absent are refused before any scan; use `recover-absent-agent-rows` for the last. Any other
refusal, including one for corrupt registry state, also refuses only its own slot. A slot whose
removal left an interrupted journal, which `wrkslots recover` must settle, stops the batch instead:
the remaining slots are refused and the output ends with `RECOVERY REQUIRED`. Wrkslots records
each operation's progress in this machine's append-only history before it writes the journal file,
so an operation interrupted between the two leaves no file. The batch therefore also replays
that history, and an operation it records as begun and not finished, for the slot or for no named
slot, stops the batch in the same way. So does a history that cannot be replayed. The batch counts
a journal or an unfinished operation only if it is still present while the batch holds the registry
lock, so one that another client writes and retires inside its own lock hold is waited out, not
reported. When the lock is not taken within the lock wait, the batch stops without naming
`wrkslots recover`. The human output is one summary line,

```text
requested=3 removed=2 refused=1 shared_process_censuses=1 fresh_process_scans=7 fenced_process_scans=2 fenced_lsof_fallbacks=0 item_budget_seconds=60 seconds=131.4
```

followed by one `REMOVED:` or `REFUSED:` line per slot; `--format json` prints the same content as
one object. An unexpected error, meaning anything other than a refusal, during the shared scan or
a slot's removal stops the batch but still prints the report. The removals already made stand and
are listed; the slot being removed and every later slot are refused; the error appears in an
`ERROR:` line and in the JSON `error` field, which is otherwise `null`; and the traceback goes to
stderr. An unexpected error before the shared scan prints only the traceback and exits 1; nothing
has been removed at that point. Exit status is 0 only when every requested slot was removed, 1 when
any slot was refused (including a batch that removed nothing) or an unexpected error occurred, and
3 when the whole command was refused, for example for a malformed or repeated item.

The batch writes no state of its own. Each slot's journal and events are written and retired
inside that slot's lock hold in exactly the form `remove` uses, so an interrupted batch leaves at
most the state of one interrupted `remove`, which `wrkslots recover` resumes, and no file that an
older client does not recognize.

## Git remotes and salvage

For each `--repo NAME=PATH`, `create` uses the configured `origin` by default. Supply
`--remote NAME=REMOTE` to choose another configured remote and `--remote-url NAME=URL` when the
caller must verify its exact fetch URL. Wrkslots records a SHA-256 identity and refuses if the URL
changes.

Wrkslots also requires Git's single effective push URL to be byte-for-byte identical to that fetch
URL. Any non-empty `remote.<name>.pushurl`, multiple URL, or push-only rewrite such as
`url.<base>.pushInsteadOf` that changes the effective destination is refused; SSH and HTTPS
spellings are not normalized into equivalence. Before agent salvage performs its first fetch, ref
readback, or push, this authority check covers every parent checkout and initialized nested
repository in the salvage set. The exact URL and its digest are captured with each fully checked
salvage candidate and are then the only destination used by isolated network Git commands. A later
checkout-config change can only refuse the operation; it cannot choose a new network destination.
The isolated transport uses plumbing commands that do not apply URL rewrite configuration and
binds the fresh repository's config bytes before execution; changing that config also refuses before
the network command starts. Remote-ref discovery and salvage readback are metadata-only.

Removal decides that a clean checkout is already published without importing the remote's
branches. One `ls-remote` lists the remote's branch heads. A head whose commit is already in the
checkout's object store is compared there. Only when none of those contains the checkout's HEAD are
the remaining heads fetched, and then only their commits (`--filter=tree:0` where the remote supports
it), into an empty temporary repository inside the checkout's own Git directory; no file contents,
no tag-only history, and nothing in the system temporary directory. No ref of the checkout,
including `refs/remotes/*`, is changed. When the slot's owner is already gone (dead or never
recorded, heartbeat expired, no hold, handoff read), removal first checks every checkout and
initialized submodule locally, then asks these questions before it takes the registry lock, and
reuses each answer (or refusal) under the lock for at most 300 seconds for the same commit, object
store, and remote URL, so the lock is not held across that network traffic. The local facts (HEAD,
status, remote URL) are always read again under the lock.

Those local checks run about twenty Git commands per checkout or submodule, and removal used to
repeat them three times under the lock. Within one removal, the facts of a clean checkout or
submodule found before the lock are now reused under it only while a fingerprint read again under
the lock is unchanged: the worktree root and Git directories, HEAD and the branch, every
configuration value Git applies there (and, for a submodule, in its source repository and in a local
mirror its remote names), the index file's identity and times, the status, unfinished operations,
and grafts, shallow boundaries and replacement refs; and for at most 300 seconds. A checkout with
changes is always checked again in full, so its salvage commit holds the content present under the
lock. The list of initialized submodules is likewise read again under the lock. Under the lock a
removal checks the slot for direct use twice before the path fence (before salvage and immediately
before the fence) and once after it.

A submodule that is a shallow clone, whose history stops at a boundary, is accepted only when it has
no changes and a branch of its remote already contains its HEAD. That is decided by fetching the
remote's branch commits into an empty repository and finding HEAD among the commits the remote sent,
never by walking the shallow history, in which Git reports a branch tip with missing parents as
containing any commit. Such a submodule is never bundled or pushed from; anything else about it
refuses ("checkout uses shallow history"), and a shallow top-level checkout still refuses.

When `with-proxy` is installed on `PATH`, Wrkslots uses it for Git fetches, pushes, remote ref
readback, and post-provision hooks. The hook's children, including recursive submodule commands,
inherit the wrapper's environment. A wrapper failure stops the operation; Wrkslots does not retry
without it. On hosts without `with-proxy`, commands use the caller's network environment. Local
Git inspection uses no wrapper, and Wrkslots does not change global Git or proxy configuration.
Network Git commands keep global and system Git configuration disabled, but explicitly use the
GitHub CLI credential helper when an executable `FLEET_REAL_GH` or `gh` is available. This permits
authenticated HTTPS salvage without admitting unrelated global Git configuration into a removal.

A repository path is resolved from the configured project root, not from the caller's current
directory, and must be relative. Use an ordinary path inside the project root, or path components of
the form `../NAME` or `../NAME/PATH` for a repository in one direct sibling directory. This is a
normalized-path rule rather than a byte-for-byte spelling requirement, but every other raw `..`
traversal, every absolute path, and every path with a symlink component is refused. Wrkslots stores
the normalized relative path, including `../NAME/PATH` for a sibling. Worktree destinations remain
confined to the configured managed worktrees directory.

For a dirty or unpushed agent checkout, reclaim constructs a commit without changing the checkout's
ordinary index or branch. It includes tracked and ordinary untracked files except configured cache
paths and handoff files (any basename starting `HANDOFF`, which removal preserves separately). The
commit message names the slot, checkout, and source head, not the registry task. When the remote's
URL is listed in the `salvage_push_remotes` configuration key, reclaim pushes the commit to
`refs/salvage/<machine>/<slot>/...`, reads that exact ref back, and records the result. Absent or
empty, that key allows no remote: the work is kept in a verified local bundle as described below,
beneath the explicit `--salvage-archive-root` or, by default, beneath the control directory's
`wrkslots-salvage/`. If the bundle cannot be written and verified, removal refuses. Wrkslots never
deletes these bundles; each holds the complete history reachable from its salvage commit, so remove
a bundle and its receipt yourself once the work is recovered. Older wrkslots clients refuse a
configuration that contains `salvage_push_remotes`, so upgrade every client that shares the
configuration before setting it. Salvage refs, like the absent-agent rescue refs under `refs/rescue/wrkslots/`,
are deliberately outside `refs/heads/`: they are addressable by exact name but are not branches, so
publishing them does not add branches to a repository that keeps only `main`. Receipts written
before this change name `refs/heads/salvage/...` or `refs/heads/rescue/wrkslots/...` and still
verify against the exact ref they recorded. If the checkout was already clean and published, the existing remote containment
is recorded instead. A failed or unverifiable push preserves the checkout.

When an allowed remote cannot accept a salvage ref, an operator may explicitly choose durable
local custody by adding `--salvage-archive-root ABSOLUTE_PATH` to `remove`. The
directory must already exist, must be owned by the current user, must not be group/world writable,
must have no symlink component, and must be separate from the managed project tree (the control
directory, which is the default root, is the one exception). Wrkslots still
tries the recorded remote first. Only after a salvage push refuses, for any reason, or after the
initial fetch fails in the transport, does it write one self-contained Git bundle per affected
repository under the supplied root. A fetch failure counts as a transport failure only when Git's
combined output reports a proxy `CONNECT` refusal, an unresolved host, or a refused, timed-out, or
unreachable connection, and reports none of the refusals listed next. Any line Git
relays from the server (`remote:`), a `remote error:` packet, an HTTP error answer, a proxy `407`
authentication demand, failed authentication, a host key or certificate problem, or a missing
repository still refuses, even when outage text appears beside it. Because a redirect can make Git
report the redirect target's connection failure, wrkslots then asks the recorded remote once more
with HTTP redirects disabled; the checkout is archived only if that attempt fails in the transport
too. Over SSH, Git passes the server's own error text through without a `remote:` prefix and
redirects do not apply, so a server message that reports a connection failure and names none of the
refusals above can still be read as a transport failure; wrkslots then keeps the work in verified
local custody rather than publishing it anywhere. Finally it checks again that the checkout's
remote URL still matches the recorded one. A
transport failure can also happen after an earlier network step succeeded, so an archive records
that the remote could not be used at that moment, not that it was never reached. Each bundle has
a schema-1 JSON receipt binding the machine, slot generation, checkout and repository identities,
remote URL digest, source and salvage commits, status digest, archive ref, byte count, and SHA-256.
Wrkslots clones the bundle into a fresh empty bare repository beside the bundle (never in the
system temporary directory), resolves the exact archive ref, and runs
`git fsck --full --strict --no-dangling` before recording `archived-local`. The same bundle,
receipt, and digest are read back again at the destructive boundary; within one removal, bytes whose
SHA-256 and size match a bundle that removal already restored and checked are not cloned and checked
again. When the slot's owner is already gone (dead or never recorded, heartbeat expired, no hold,
handoff read) and the checkout would be archived because its remote is not in
`salvage_push_remotes`, removal makes and verifies the bundle before it takes the registry lock;
under the lock it finds that bundle, rereads its bytes, and writes the receipt. A bundle that an
earlier, refused removal already made is restored and checked again before the lock, not under it.

An interrupted removal can be resumed with ordinary `wrkslots recover`; its finish journal carries
the verified local receipt. If interruption occurred just before the finish journal was created,
repeat the original `remove` command with the same archive root. The deterministic archive path is
reused only when its receipt and bundle still exactly verify. A missing, changed, partial, or
wrongly-bound archive refuses and leaves the checkout intact. Local archive custody after a refused
push is intentionally not available through `retire-pending`, because selecting that durability
boundary must be an explicit per-slot operator action; a remote outside `salvage_push_remotes` always
uses the default root there.

```sh
mkdir -p "$HOME/temp/agent_checkouts"
wrkslots remove slot01 \
  --coordinator-pid "$CURRENT_COORDINATOR_PID" \
  --expected-generation 1 \
  --salvage-archive-root "$HOME/temp/agent_checkouts"
```

Gitignored content is excluded by repository policy, not by size. It is never added with `--force`,
never included in the salvage status digest, and never uploaded to the recorded remote. Authored work
that must survive belongs in tracked or ordinary untracked paths, or in a separately recorded
artifact store; an ignored build tree is not remote preservation evidence.

Initialized Git submodules are checked separately against the corresponding source repository's
remote URL. Each nested repository gets its own salvage commit and remote readback, so an
uncommitted file inside a submodule cannot disappear behind an outer gitlink that did not move. If
explicit local custody is enabled, a nested repository whose own remote refuses gets its own bundle
and receipt; a successful parent remote publication does not weaken that nested check.
The source checkout is allowed to switch between the recognized GitHub HTTPS, SCP-style SSH, and
`ssh://git@github.com/` spellings only when the lowercase owner and repository components remain
identical. Salvage continues to use and recheck the managed slot's own URL. A different host, owner,
or repository still refuses; arbitrary local URLs and other hosting providers remain byte-exact.

One kind of local URL is accepted: an absolute path naming a local mirror of the source submodule's
remote, meaning the source submodule's own Git directory or another local repository whose own
remote names the same repository as the source's remote. A linked worktree shares its
superproject's configuration, so when that configuration names such a module directory as
`submodule.<name>.url`, `git submodule update` in the slot clones from it and records that path as
`origin`. Removing the slot deletes neither repository, but neither is where work is published, so
the nested checkout's HEAD is judged at the source submodule's own remote, which must lead elsewhere
(a network URL, or an absolute path to yet another repository). Salvage never pushes through that
alias: unpublished nested work is kept in a verified local bundle. A local path to any other
repository, including one whose remote names a different repository or no remote at all, still
refuses.

For `recover-absent-agent-row(s)`, only the digest of a checkout's remote URL was recorded. A source
repository whose remote has since moved between the GitHub spellings above for the same owner and
repository (for example from `git@github.com:` to `https://github.com/`) is accepted when the
recorded digest is that of one of those spellings; any other change still refuses.

## Import older slots

For a live slot already at its final managed path, run `import-existing` first as a dry run, then
repeat with `--apply --verified-live`, its live `--owner-pid`, and the current
`--coordinator-pid`. The owner may be the invoking process. A coordinator repairing another live
owner's slot must name an owner process that descends from that coordinator and whose working
directory is inside the slot; liveness without that path evidence refuses.

```sh
wrkslots import-existing slot01 --help
wrkslots import-existing slot01 --slot-type agent --coordinator-authorized \
  --agent codex-1 --task task-123 --purpose "continue task-123" \
  --repo product=../product --apply --verified-live \
  --owner-pid "$OWNER_PID" --coordinator-pid "$COORDINATOR_PID"
```

For a slot whose owner has exited, use the older version 3 `worktree-state.json` as evidence instead
of reconstructing ownership from the directory. Run `import-existing SLOT --from-state-file
worktree-state.json --source-host-id ID` as a dry run, then add `--apply` and the current
`--coordinator-pid`. The source row supplies the agent, task, purpose, allocation time, checkout
paths, and exact owner process generation. Because the older owner sidecar omitted the stable host
identity, `ID` must be the current source host's `/etc/machine-id`; a mismatch refuses. The imported
row starts a fresh heartbeat time-to-live. Its prior `active`, `lease-quarantined`,
`owner-lease-revoked`, or `release-requested` status is retained as evidence, never treated as
permission to remove. If the older row omitted its task or purpose, the active record says that the
field was not recorded; it does not invent what the slot held, and the exact source row remains
attached. The slot row's current task is used when present; the owner sidecar is only the fallback
when that row field is absent, because the sidecar describes the owner process at binding time.
Likewise, its agent name is retained as provenance rather than treated as a current assignment:
several older rows may name the same agent, and those rows do not prevent that name from owning one
new live slot. The ordinary registry still refuses two live assignments for one agent. A
source-file import also refuses while its exact recorded owner process is still live; that owner
must use the ordinary live import path instead.

Name each source row's checkout repositories with `--repo NAME=PATH`; `NAME` is the prefix of its
`NAME_path` field. A source row with nested paths remains nested even when newly created slots use the
flat layout. An empty residual slot directory can be imported with no `--repo`: its exact source row
is retained, and removal later verifies that the directory contains no checkout before recording
that there was nothing to salvage. A present checkout without a matching `--repo` refuses and
prints the missing flags. A row with no owner sidecar also refuses before writing a journal or
active row, because its owner death cannot be established.

Every applied import writes the complete candidate row to the append-only operation history before
publishing it in ACTIVE. If the importer exits after that write, any later participant runs
`wrkslots recover --coordinator-pid PID`; recovery does not reread the older state file or depend on
the original coordinator. Other unregistered slot directories are retained while the selected slot
is verified. Source rows whose physical directories are already absent remain in the older
file as history rather than being fabricated as active storage.

## Recover ownerless validation paths

Do not fabricate an ACTIVE owner for an unregistered validation path. `import-existing` remains the
route for a demonstrably live owner or an exact owner generation from a retained row; `remove`
remains the route for a registered row. Destructive recovery of an ordinary ownerless linked Git
worktree is disabled because wrkslots cannot atomically exclude both the checkout and its
same-UID-writable Git administration. Preserve and inspect it. The ownerless recovery route remains
available for `validate-cargo-*` directories:

```sh
wrkslots recover --coordinator-authorized --coordinator-pid "$COORDINATOR_PID" \
  --ownerless-validate-cargo-home worktrees/validate/validate-cargo-example \
  --recovery-note "the retained run handle has no Cargo-home field"
```

The Cargo-home form may instead use
`--completed-record ignored/validate/runs/validate-example.json` to bind cleanup to an exact
terminal run record. A recordless path requires a non-empty explanation, but prose is not
authority: wrkslots records the exact path and filesystem identity and independently verifies that
no retained record names it and no process uses it. Cargo homes must have the exact configured
parent and `validate-cargo-*` name and cannot be Git worktree roots. The cleanup retains positive
cwd, executable, root, descriptor, mapping, and mount checks, then deletes only the excluded path.
Any later participant may resume its journal with plain `recover --coordinator-pid PID`.

`--ownerless-validate-checkout` remains accepted so existing automation and pre-upgrade journals
fail with an explicit preservation message. It performs the bounded authored-work,
HANDOFF, submodule, remote-binding, and liveness checks, but it does not create a cleanup journal or
remove the linked worktree. An initialized submodule is itself a preservation reason even when Git
configuration would otherwise hide its state.

This targeted route does not hide anything from ordinary `status`: every other unregistered
directory is returned as a typed inconsistency beside the full readable roster. Status is
diagnosis, not permission to mutate. During a larger deliberate migration, the global
`--allow-existing-unregistered-worktrees` flag may accompany the exact cleanup command; it retains
all other paths.

## Recover registered validation rows with absent storage

If external cleanup removed disposable validation directories without retiring their ACTIVE rows,
generate an explicit bounded input from a fresh `wrkslots audit --format json`. Each registered row
includes its canonical `record_sha256`; copy only the intended rows into this form:

```json
{"schema": 1, "rows": [{"machine": "host", "slot": "validate-fresh-example", "generation": 1, "record_sha256": "<64 lowercase hex digits>"}]}
```

Inspect the read-only plan first, then apply the exact same file:

```sh
wrkslots recover-absent-validate-rows --input /path/to/rows.json
wrkslots recover-absent-validate-rows --input /path/to/rows.json --apply \
  --coordinator-authorized --coordinator-pid "$COORDINATOR_PID"
```

The command refuses agent rows, changed generations or row digests, holds, any path or Git
registration that still exists, unreadable handle/process/systemd evidence (including any handle
that repeats a JSON field), and any live process or
user service that may use the row. A retained run handle contributes its exact service unit and,
when recorded, process generation to the present-tense liveness proof; its run state is not treated as a validation
outcome. The configured agent-liveness probe is likewise not an ownership authority: an agent may
restart elsewhere, while a validation may outlive the agent that launched it. The resource proof is
the exact recorded owner generation, retained service identity when present, an all-process path and
mount census, all user services/scopes/jobs, and absent Git and filesystem state. The same run
handle, process, and user-systemd evidence is the liveness authority for validation slots whose
storage still exists; see "Time-to-live and process evidence".

The whole batch is preflighted before writing. Recovery then appends one archive transition per row,
records that durable prefix, and appends one ACTIVE-removal transition per row. A crash may expose a
prefix, not a fictitious all-or-nothing update; the journal makes that prefix resumable and refreshes
the compatibility snapshots before completion. Every archive keeps the established schema-2 shape,
records physical storage as removed (its disposition at archive time), and states that the validation
outcome remains unknown. Rerunning the same command or plain `wrkslots recover --coordinator-pid
PID` resumes safely. Do not infer a validation result or run number from this registry repair.
Cross-user process evidence uses passwordless `sudo -n` with fixed root-owned `find` and `grep`
binaries and refuses if that read-only census is unavailable or incomplete. Use `--format json` when
another tool needs the typed per-row outcome.

## Relocate a moved source repository

A checkout's recorded repository path is its Git evidence. When the source repository moves, for
example from `../tools` to `../tools/tools`, `status` reports every row that names
it as `repository-evidence-unavailable`, and no mutation of those rows can proceed. First repair the
Git link of each present checkout, then plan and apply the relocation:

```sh
git -C ../tools/tools worktree repair PATH-OF-EACH-PRESENT-CHECKOUT
wrkslots relocate-repository ../tools ../tools/tools
wrkslots relocate-repository ../tools ../tools/tools --apply \
  --coordinator-authorized --coordinator-pid "$COORDINATOR_PID"
```

The command changes only the repository path, and only on rows of this machine that record FROM.
It refuses unless TO is the same repository: FROM must no longer be a Git repository; TO must be the
top of one; every present checkout recorded under FROM must be a linked worktree whose Git directory
lies in TO's common directory, that TO lists, and whose recorded branch exists in TO; and every
affected checkout's recorded HEAD, start point, and remote URL must match TO. A clone of the same
remote holds the same commits and URL, so the linked-worktree check is what tells the repository
apart. A row whose storage is gone has no Git directory of its own, so at least one present checkout
from the same FROM must vouch for the move, and every affected row moves in the same command. Rows
without storage are written first, so an interrupted apply can be rerun while the present checkouts
still record FROM. Each row gets one `repository-relocated` event. The plan prints each row's new
record SHA-256 for a later `recover-absent-agent-row`.

A wrkslots client without `../NAME/PATH` support refuses to read a registry that records such a path.
Upgrade every client of the registry, including other checkouts that run their own copy of wrkslots
against it, before `--apply`.

## Recover absent agent rows and ownerless agent worktrees

Agent storage is never treated as disposable. For an exact ACTIVE agent row whose directory is
already absent, take `machine`, `slot`, `generation`, and `record_sha256` from a fresh JSON audit and
inspect the read-only plan before applying it:

```sh
wrkslots recover-absent-agent-row SLOT --expected-generation N \
  --record-sha256 SHA256
wrkslots recover-absent-agent-row SLOT --expected-generation N \
  --record-sha256 SHA256 --apply --coordinator-authorized \
  --coordinator-pid "$COORDINATOR_PID"
```

The command requires any recorded exact owner generation to be dead and requires the registered
liveness authority to report dead, with one exception: an authority verdict of alive (exit `1`) is
overridden, with a `NOTE:` on standard error, when the row records a usable owner and that exact
generation (PID, start ticks, and boot) is proven dead from the initial PID namespace. The authority
answers about an agent name, and another live process carrying that name -- a restarted session or a
leaked helper -- cannot use a row whose storage is gone and whose every owner operation binds the dead
generation. Without the exception such a row could never be recovered, and the one-slot-per-agent
rule would refuse every new slot for that agent. An unverifiable verdict (exit `2` or any other
failure), a live or indeterminate owner generation, a row with no usable owner, or a caller outside
the initial PID namespace still refuses. The command then performs the same full-host process,
cgroup, mount, and user-systemd census used for absent validation rows. It does not claim the
vanished working tree was clean. Each recorded checkout commit must be preserved on that checkout's
own remote before its exact stale Git worktree registration is removed, in one of two ways:

- When the remote's URL is listed in `salvage_push_remotes`, the commit is pushed individually to a
  dedicated rescue ref and read back from the remote.
- Otherwise recovery publishes nothing. It accepts the checkout only when a ref already on that
  remote contains the recorded commit. Branches (`refs/heads/*`), rescue refs (`refs/rescue/*`), and
  tags (`refs/tags/*`) are searched first; only when none of them contains the commit are the
  remote's other refs, such as a hosting service's `refs/pull/*` or a user's `refs/archive/*`,
  fetched and searched, because a hosting service can advertise one or two refs per review. Any
  ref name Git accepts qualifies, including one of only two components such as `refs/stash` and one
  whose bytes are not UTF-8: names reach Git as their exact bytes, and output shows unprintable
  characters escaped. The remote's refs are read fresh and fetched only into a temporary
  repository, under numbered names in `refs/wrkslots-fetched/`; no ref in the local repository,
  including `refs/remotes/*`, is read or
  changed, although fetched objects are stored in its object store. The plan prints the chosen ref
  as `remote_containment=[...]`, with the object the remote advertised for it (for an annotated tag,
  the tag object). The apply records it in its journal and checks again, with
  `git merge-base --is-ancestor` against a freshly fetched copy, immediately before removing each
  checkout's registration and again before changing ACTIVE. Recovery neither created nor protects
  that ref, so the archive says that the commit stays preserved only while the ref contains it.
  Ancestry is read from the commit objects, never from a commit-graph file. A shallow remote is
  refused: it can advertise a branch without holding that branch's older history, so its refs cannot
  show that it holds the recorded commit. Each fetch asks for complete history, so the remote is
  always asked, and reports whether it is shallow, even when every object is already local. Give
  such a remote complete history, for example with `git fetch --unshallow` there, then rerun.
  Listing the remote's refs is stopped and refused after 120 seconds, and each fetch after 300
  seconds. Each fetch names the recorded commit, and every advertised tip already held locally, as
  history the local repository has, so it does not download the recorded commit's history again. A
  branch whose history reaches local commits only through commits the local repository lacks can
  still download objects the local repository already holds.

  Because such a fetch stops at history the local repository already holds, every check that finds
  the chosen ref containing the commit is confirmed by fetching that ref alone, commits only
  (`--filter=tree:0` when the remote supports filtering; otherwise its full history), into a
  temporary repository whose object store starts empty. The commit must then be an ancestor of the
  ref using only what the remote sent; a remote whose own grafts or replacement refs hide part of
  the ref's history, or that lacks it, is refused. Push the commit to a branch on that remote, then
  rerun.

A checkout whose remote is not listed and that no such ref contains is refused by both the plan and
the apply. A resumed recovery whose recorded ref no longer contains
the commit is also refused, and it keeps the ACTIVE row, the journal, and the remaining
registrations. The archive is durable before
ACTIVE is changed and explicitly records that uncommitted, untracked, ignored, and HANDOFF contents
could not be inspected because storage was already absent.

Removing a linked worktree's registration deletes what Git keeps only for that worktree, under
`<common>/worktrees/<id>/`, and both a rescue ref and an existing remote ref preserve only the
recorded commit's history. On both paths, the plan, the apply, a resume, and the step immediately
before removal therefore each refuse while that directory holds any of the following:

- An index that stages content, or holds an unmerged conflict, that the recorded commit does not
  contain. Commit and publish that content, or reset the index to the recorded commit if it is not
  wanted, then rerun. Staged path names are read as the bytes Git stores, and the refusal shows
  unprintable bytes escaped.
- A per-worktree ref (`refs/worktree/*`, `refs/bisect/*`, or `refs/rewritten/*`) naming anything
  outside the recorded commit's history. Publish its commit and delete the ref, or delete the ref if
  it is not wanted, then rerun. A ref name that is not UTF-8 is shown escaped.
- A resolve-undo entry in that index whose content never appeared at the same path in the recorded
  commit's history. Resolving a conflict keeps the conflicting versions in the index, through later
  commits, and only that index keeps them. Path names are compared as the bytes Git stores, so a
  name that is not UTF-8 or that holds a carriage return is checked exactly; each history command
  is stopped and refused after 300 seconds. Publish that content, or reset the index to the recorded
  commit if it is not wanted (`GIT_INDEX_FILE=<common>/worktrees/<id>/index git read-tree <commit>`),
  then rerun.
- An unfinished merge, cherry-pick, revert, rebase, or bisect (`MERGE_HEAD`, `CHERRY_PICK_HEAD`,
  `REVERT_HEAD`, `BISECT_LOG`, `rebase-apply`, `rebase-merge`, or `sequencer`). Finish or abandon it,
  then rerun.

Reflogs, `ORIG_HEAD`, and `FETCH_HEAD` record past positions rather than unfinished work, and they
are deleted unchecked, as by every `git worktree remove`. Every remote check for a checkout runs
first. The final checks and the removal then run while recovery holds that worktree's `index.lock`,
so no Git command can stage or commit there in between. An `index.lock` that already exists belongs
to a Git command recovery cannot see, running or stopped, and refuses until it is gone. A resumed
recovery that finds the recorded commit pruned from the local repository fetches it back from the
remote ref that preserves it before making these checks.

Git also keeps a full clone of each submodule of a linked worktree in that directory, at
`<common>/worktrees/<id>/modules/<name>/` (a nested submodule under
`.../modules/<name>/modules/<nested>/`), so a submodule commit that was never pushed may exist
nowhere else. Before writing its journal, recovery examines every such repository. A commit is
unpublished when HEAD, a ref outside `refs/remotes/`, or a reflog names it, wherever it is stored,
or the repository stores it in its own object directory, loose or in any kind of pack, and nothing
names it, and no remote-tracking ref of that repository, or of the shared submodule repository
`<common>/modules/<name>`, reaches it. Recovery salvages each unpublished commit HEAD or a ref names
and each that no other unpublished commit has as a parent, so their history holds every unpublished
commit. A commit the repository stores whose history is incomplete refuses. A commit the repository
reads only through `objects/info/alternates`, and that nothing in it names, may belong to another
repository that shares that object directory; when no remote-tracking ref and no commit recovery
salvages reaches it, the row refuses, because it can be shown neither published nor salvaged. One
stored in the own object directory of another of these repositories that recovery examines is not
counted that way, since examining that repository covers it. A
reflog entry naming a commit Git cannot read refuses too (`git reflog expire --stale-fix --all`
drops such entries). Git skips a pack it has no usable index for without failing, so recovery
refuses a pack, in the repository's own object directory or one it borrows from, whose index is
missing, fails its checksum, or was not made for that pack (only the pack's header and trailer are
read), and also refuses when Git reports anything while listing the stored objects. An index that
passes those checks yet maps an object name to another object in its pack is not detected. A repository
using SHA-256 object names refuses. The plan prints what will happen to each submodule as
`submodule_salvage=[...]`.
For each submodule with unpublished commits:

- When the submodule's remote is listed in `salvage_push_remotes`, each commit is pushed to
  `refs/rescue/wrkslots/<machine>/<slot>/<checkout>-<generation>-submodules/<submodule>/<commit>` on
  that remote and read back. A remote given as a local path must keep its objects and refs outside
  the administrative directories recovery deletes. It refuses when the repository, or a directory
  of objects or refs it uses (an alternate it borrows from included), lies inside one of them, or
  it or a directory below it is one of them under another name, as a bind mount makes it; when a
  symbolic link lies below such a directory of objects or refs; and when a mount point lies below
  any of them.
- Otherwise, or when that push is refused, the commits are written with complete history to a local
  bundle under `wrkslots-salvage/`, verified like the salvage bundles described above. Before the
  removal, recovery refuses when the bundle's directory, or a directory below it, is a directory of
  the `worktrees/` directory of the common Git directory under another name, as a bind mount makes
  it, or when it is on storage the checks below refuse.

If salvage fails, the row is refused before its journal is written, and nothing is moved or
removed. Rescue refs already pushed and bundles already written for the row's other submodules
stay; a rerun accepts a rescue ref or bundle that already holds the same commits. Submodule state that salvaging commits
cannot keep also refuses: an unfinished operation, staged content or a resolve-undo entry in a
submodule's index, a shallow clone, a submodule repository with worktrees of its own, a ref naming
something other than a commit, a missing or unreadable commit, a Git lock file (left by a running or
stopped Git command), or an unsafe directory entry.

Recovery never deletes these repositories. While it holds the worktree's `index.lock` it renames
`modules/` to `wrkslots-recovery-modules/` and puts a regular file at `modules`, so a Git command
already running cannot make a repository there anew, then examines them once more; any unpublished
commit outside the recorded salvage refuses. Immediately before removing the registration it moves
them, by one rename and with their reflogs, to
`<common>/wrkslots-retained-modules/<slot>+<generation>+<digest>+<id>/modules/`, where `<digest>` is
the first 16 hexadecimal digits of the row's `record_sha256`, so neither a later row that reuses the
slot name nor a later checkout that reuses Git's `<id>` shares the directory. Recovery first writes
`wrkslots-retained-owner.json` there, naming the machine, slot, generation, `record_sha256`, and
`<id>`, and listing each entry of the administrative directory with its kind. A process that already
held a working directory or directory descriptor inside them follows the renames, so a commit it
makes after that last examination is neither salvaged nor recorded, but it stays in the retained
copy.

Removing the registration deletes everything below the administrative directory, and Git walks into
a mount point there and deletes what the mounted file system holds. A mount point at or below
`<common>/worktrees/<id>/` therefore refuses the row. Recovery also refuses when retaining the
repositories would lose an object they read: a symbolic link anywhere in `modules/`, or an
`objects/info/alternates` entry, followed as deep as Git follows them, that names a different place
once the tree is moved (an absolute path into `<common>/worktrees/<id>/modules/`, or a relative path
that leaves the moved tree into the administrative directory) or that names a place in the
`worktrees/` directory of the common Git directory, which Git deletes with a registration. Git
resolves an entry following symbolic links in order, so a `..` after a link climbs from where the
link points; recovery checks that place and also the one the entry names when normalized without
following links. A relative entry that stays inside the moved tree, or one that names the common
directory's own objects or a place outside the common directory, still works from the retained
copy. Any such place outside the moved tree refuses, though, when it or a directory below it is a
directory of `worktrees/` under another name, as a bind mount makes it, when anything below it is a
symbolic link, or when a mount point lies below it: Git reads objects there that the removal may
delete.

An entry that leads into a proc file system refuses, whatever it resolves to: `/proc/self/...`, and
`/dev/fd/...` or `/dev/stdin`, links into `/proc/self/fd`, name a different place for each process
that reads them, so recovery cannot see what a Git command already running there reads. The same
holds for a rescue remote given as a local path: the path, and each gitfile, `commondir`, or
alternates entry Git follows from it, as written and joined to where Git reads it, refuses when it
leads into a proc file system, since the Git command serving the remote resolves it from its own
place, not recovery's. The mount holding such a place outside the moved tree, a rescue remote's
storage, or the bundle's directory is examined too. A union file system refuses: one that does not
show its layers (`aufs`, `fuse-overlayfs`, `mergerfs`, `bindfs`, and the like), and any overlay
mount. The mount table names an overlay's layers only by the paths they had when it was mounted,
while the kernel holds the directories themselves, which a rename since can have put anywhere,
the administrative directory included; nothing shows which directories they are now. Storage
another mount namespace, a network file system, or a FUSE file system other than those named shows
is not seen.

Recovery makes these storage checks before it lists the objects of any repository, so a link,
alternates entry, or mount they refuse is reported as that, not as whatever Git reports while
reading through it. It reads `/proc/self/mountinfo` while examining the checkout and again before
moving the repositories. After moving them, immediately before the removal, it repeats every check above on
the retained copy, and also refuses a mount point at or below the retained directory. It checks the
retained directory itself as well, since a rename since the move can carry it into the
administrative directory: `wrkslots-retained-modules/`, the row's directory in it, and its
`modules/` must each still be a directory and not a symbolic link, `modules/` still the directory
moved, as identified before it was moved; resolved, they must name the place below the common Git
directory where recovery put them; and neither that place nor anything below it may be a directory
of the administrative directory under another name. Every directory of the repositories examined
there must still be at its place below `modules/`, the same directory: one gone, or another put in
its place, refuses, since the one examined may now be anywhere, the administrative directory
included. Each salvage is then checked again: a bundle and its receipt must still be where they
were written, below private directories outside every administrative directory recovery deletes,
and still hash to what was verified; a rescue remote given as a local path must still keep its
objects and refs outside those directories, and its rescue refs must still name the commits. A
rescue remote reached through the network is not checked again. Something made after that last
check is not seen. When that last check refuses, the repositories stay where they are (the refusal names the place to put them back if
they were moved) and a rerun moves them back.

A recovery stopped before the removal moves them back to `modules/` when it resumes, before checking
anything; until then every check of that checkout refuses. It moves nothing back, and refuses, when
the owner file names another row or directory, when the retained directory holds anything recovery
did not put there, or when an entry of the administrative directory that the owner file lists is
gone or has changed kind. That last means `git worktree remove` stopped partway, so Git may no
longer register the checkout: the repositories stay in the retained directory, and once nothing
left in the administrative directory is wanted, remove it by hand and rerun. Moving the
repositories back removes the owner file, and the directory recovery made to keep them once
nothing else is in it. Once the registration is removed, wrkslots never removes a retained
directory: inspect it and delete it yourself once nothing in it is wanted.

To plan or recover many such rows, name each exact row as `SLOT=GENERATION=SHA256`, with the
generation and `record_sha256` from the same audit:

```sh
wrkslots recover-absent-agent-rows --row example-a=3=SHA256 --row example-b=1=SHA256
wrkslots recover-absent-agent-rows --input /path/to/rows.txt --apply \
  --coordinator-authorized --coordinator-pid "$COORDINATOR_PID"
```

The input file holds one row per line; blank lines and lines starting with `#` are ignored, and
`--row` may be combined with it. A batch accepts at most 128 distinct slots, all on this project's
machine. Each row goes through `recover-absent-agent-row` unchanged, with the same authority,
journal, and proofs, in its own registry-lock hold; nothing is shared between rows. The lock is
released between rows, followed by a 0.25-second pause, and each row waits up to 30 seconds for the
lock, or for `--wait-lock` seconds when that is given. On a busy machine one row's plan takes 5 to
20 seconds and its apply 20 to 40 seconds, all of it inside the lock and most of it the full-host
process and user-systemd censuses, which each apply runs three times.

A refused row is left in place and reported with its reason, and the remaining rows are still
attempted. Corrupt or interrupted registry state stops the batch, refuses the remaining rows, and
prints `RECOVERY REQUIRED`; a rerun of the same batch resumes an interrupted row's recovery and then
continues. The human output is one summary line,

```text
mode=apply requested=3 would_recover=0 would_resume=0 recovered=2 already_recovered=0 refused=1 seconds=71.4
```

followed by one `WOULD-RECOVER:`, `WOULD-RESUME:`, `RECOVERED:`, `ALREADY-RECOVERED:`, or
`REFUSED:` line per row, each followed by that row's single-row output indented by two spaces;
`--format json` prints the same content as one object. The two `WOULD-` outcomes appear only
without `--apply`: `WOULD-RESUME:` names the row whose interrupted recovery an `--apply` run would
finish first. Exit status is 0 only when no row was refused: without `--apply` every row would be
recovered or resumed or was already recovered, and with `--apply` every row was recovered or
already recovered. It is 1 when any row was refused, and 3 when the whole command was refused,
for example for a malformed or repeated row.

The local branch is separate evidence, not the salvage authority. It may be absent or may directly
name the recorded commit, an ancestor, a descendant, or a divergent commit. Recovery records that
exact state, issues no source-repository command targeting the branch for mutation, and requires the
same state immediately before and after registration reconciliation. Only the recorded slot HEAD is
given a rescue ref; a distinct local branch tip is left in place without a separate rescue ref. The
branch witness expires after the registration removal is durably journaled, so the archive tells the
operator to re-read every local branch rather than asserting its later state. Symbolic branch refs,
a rescue destination colliding with any recorded local branch, relative local remotes, and local
remotes whose Git common directory aliases any source repository in the row all refuse before the
journal or salvage push.

An unregistered agent worktree is recovered without assigning it an owner, task, or handoff. Supply
the exact path and the Git identities established during inspection:

```sh
wrkslots recover-ownerless-agent-worktree worktrees/slots/example \
  --repository repo --head COMMIT --branch agent/example --remote origin \
  --remote-url-sha256 SHA256 [--handoff-sha256 SHA256]
```

Repeat with `--apply --coordinator-authorized --coordinator-pid "$COORDINATOR_PID"` only after the
plan agrees. The worktree must be one direct child of the managed agent root. A HANDOFF.md requires
the exact digest supplied after the coordinator reads it; the command rechecks it before every
destructive boundary and preserves it through the same remote salvage path. An unread or changed
handoff, any live process or user unit reference, a path or inode change, non-ordinary Git state, or
an ACTIVE/archive identity collision refuses. Tracked and ordinary untracked files outside
configured cache paths, and initialized nested repositories are committed to verified salvage refs
before the exact inode is fenced and removed. Any later participant can resume the journal with
ordinary `recover`.

The `worktrees/slots/ignored` path is not an agent worktree. Only when it contains the command's
one exact supported cache hierarchy can it be relocated intact outside the managed slot root:

```sh
wrkslots recover-ownerless-agent-cache
wrkslots recover-ownerless-agent-cache --apply --coordinator-authorized \
  --coordinator-pid "$COORDINATOR_PID"
```

This command accepts no arbitrary path, refuses symlinks, mounts, `.git`, `HANDOFF.md`, unexpected
top-level content, live use, or an occupied destination, and journals the same-inode relocation.

## Disk-image slots

A slot can be stored in one of two representations, chosen per slot when it is created:

- `worktree`: plain directories. Checkouts, caches, and build outputs are ordinary directories
  on the host file system.
- `image`: the slot directory is the mount point of one sparse ext4 image file. The host sees a
  fixed handful of files per slot, however many files the agent creates inside it:

  ```text
  <control>/slot-images/<slot-type>/<slot>/
      IMAGE.json   representation record (location, backend, ceiling)
      slot.img     sparse ext4 image mounted at the slot directory
      state.img    sparse ext4 image for the sandbox's per-slot private HOME layer
      state/       mount point of state.img
  ```

Why this helps: a slot's millions of small build files become metadata inside the image instead of
metadata of the host file system. A runaway writer fills its own image and gets "no space left on
device" instead of filling the host. Reclaim unmounts and deletes one file instead of walking a
tree.

Representation is a storage choice only. Ownership, heartbeats, salvage, handoffs, and reclaim
are identical for both. Git linked worktrees are still used; only the checkout files and anything
written below the slot live in the image. The Git common directory stays where it was.

### Choosing the representation

`init` writes `slot_representation: image` for a new project. Rerunning `init` on an existing
project keeps what it has, and a configuration without the key means `worktree`, so upgrading
wrkslots never converts a live project. Change the default for future slots with:

```sh
wrkslots image set-default image      # or: worktree
```

`create --representation worktree|image` overrides the default for one slot. Existing slots keep
their representation; `wrkslots image convert SLOT --to image|worktree` migrates one idle slot in
place (below).

### Sizing: a ceiling, not a reservation

Images are sparse and are never pre-allocated. `configuration.image.ceiling_bytes` (default
512 GiB; set with `init --image-ceiling-gib`) is only the largest the slot may grow. A fresh
image costs about 70 MiB of host space (the ext4 journal and metadata). Deleted files return
their blocks to the host: kernel mounts use online discard, FUSE mounts return them on
`wrkslots image trim` or unmount. `wrkslots image grow SLOT --ceiling-gib N` raises a ceiling
without copying anything.

Host-wide space is still finite. Watch it with `wrkslots image status`, which reports each image's
host-allocated bytes, bytes used inside it, and inode count.

### Mounting without host configuration

Nothing is written to host configuration; mounts are created and removed by wrkslots itself.
`configuration.image.backend` selects how:

| Backend | Mechanism | Needs | Trade-off |
|---|---|---|---|
| `kernel` | `sudo -n mount -o loop,discard` | passwordless sudo | native speed |
| `fuse` | `fuse2fs` as the invoking user, in a transient user service | `/dev/fuse`, `fuse2fs` | no privilege; metadata-heavy work is about ten times slower |
| `auto` (default) | `kernel` when `sudo -n true` works, else `fuse` | either | |

`WRKSLOTS_IMAGE_BACKEND` overrides the choice for one command. Every wrkslots command first
mounts any image-backed slot whose image is not mounted (after a reboot, for example), at the
location its `IMAGE.json` records, so lifecycle logic always sees the slot content. A slot's own
image mount is not treated as a process using the slot.

A mount point cannot be renamed, so the path fence that removal uses unmounts the image, renames
the empty mount point, and mounts the image again at the fenced path. The unmount refuses while
something uses that particular mount, which is the same refusal the fence already expects. A process
working in another mount of the same file system, such as a bind copy in this or another mount
namespace, does not stop it, and keeps the old mount after the image is mounted again. Removal
therefore checks an image-backed slot with `lsof` before the fence, while the old mount is still the
slot's mount.

### Converting an existing slot in place

```sh
wrkslots image convert slot07 --to image       # plain directories -> image
wrkslots image convert slot07 --to worktree    # image -> plain directories
```

Conversion refuses while any process has files open in the slot or works inside it (an idle
agent may stay registered). It copies the content, proves the copy matches file by file (path,
type, size, mode, and modification time), swaps it in at the same path, and re-verifies every
checkout's Git identity. The branch, registry record, and path are unchanged. `--keep-original`
keeps the pre-conversion copy for inspection; use it for the first conversions of a project.
Conversion is not journaled: convert only idle slots, and if a conversion is interrupted, inspect
the slot, its image directory, and the kept copy before retrying. Converting to `worktree` discards the sandbox state
image (the slot's private HOME layer).

## Running commands and agents inside a slot's box

`wrkslots run SLOT -- COMMAND` runs COMMAND boxed to one slot. The box does not depend on how the
slot is stored: a plain-worktree slot and an image-backed slot get the same limits and the same
view. Only the location of the slot's private state differs (`state.img` for an image slot,
`<control>/slot-state/<type>/<slot>/` for a plain slot). Nothing is written to host configuration.

**What the box is for.** It is an accident boundary for cooperative agents: it keeps an agent's
writes (stray checkouts, build output, edits to the wrong tree) inside its slot and the paths it
was given. It is not containment against a hostile agent. What each mode leaves open is listed
under "Limits of the box" below.

- **Limits.** The calling process joins a transient systemd user scope inside the slot's own slice,
  `wrkslots-<slot>.slice` below `wrkslots.slice`, and then execs COMMAND. Memory, CPU, task, and IO
  limits (`--memory-max`, `--memory-high`, `--cpu-quota`, `--tasks-max`, or
  `configuration.sandbox.limits`) apply to the slice, so every command run against the slot shares
  one budget. When the caller is already inside a slice (a harness's own sandbox slice, for
  example), the slot's slices are created inside it: wrkslots never moves a process out of an
  enclosing slice.
- **Process identity.** Every step execs, so COMMAND keeps the caller's PID and stays a child of
  the caller. A terminal multiplexer that only starts agents in a pane whose own shell is at its
  prompt keeps working (except with `root` isolation; see below).
- **Isolation** (`--isolation`, `configuration.sandbox.isolation`):

  | Mode | Limits | File-system view | Needs |
  |---|---|---|---|
  | `userns` (default) | yes | built in an unprivileged user and mount namespace | unprivileged user namespaces |
  | `root` | yes | the same view, built by a short-lived `sudo -n` launcher in a plain mount namespace, then privileges dropped | passwordless `sudo` |
  | `cgroup` | yes | none: the host file system as is | nothing more |

  Setuid programs cannot gain privilege inside a user namespace, so `sudo`, and any harness
  launcher that performs a setuid step, fail under `userns`. `root` exists for them. The launcher
  enters the slot's scope first (so the cgroup is inherited), checks that `sudo -n` will not
  prompt, and passes the complete environment in a private 0600 spec file in the per-user runtime
  tmpfs (`$XDG_RUNTIME_DIR/wrkslots`, read-only inside every box), because sudo scrubs it. The
  helper takes your identity from sudo (`SUDO_UID`, `SUDO_GID`, and your groups from the group
  database) and refuses a spec that disagrees. It deletes the spec as soon as it has read it. It
  switches to your uid, gid, and supplementary groups at once, keeping only `CAP_SYS_ADMIN` while it
  builds the view (so a FUSE mount you own stays reachable). It then drops that capability too and
  execs COMMAND. No user namespace exists, so setuid programs inside the box work. One consequence:
  sudo stays in front of COMMAND and relays a private terminal to it. A terminal multiplexer
  therefore sees only `sudo` as the pane's foreground process. It can neither start an agent in
  such a pane nor detect one, so an agent launcher runs the boxed harness itself
  (`shell-command SLOT -- HARNESS ...`) and tracks it without the multiplexer's agent start.

### The view

With `userns` or `root` isolation, COMMAND sees:

1. **A fresh `/tmp`**: an empty tmpfs (mode 1777, `tmp_size`, default `16G`) for this launch only,
   discarded when COMMAND exits. `TMPDIR` is `/tmp`.
2. **`$HOME` as a per-slot layer.** A persistent, private, writable directory from the slot's state
   is mounted over `$HOME`. With `home: ro` (default), every top-level entry of the real `$HOME`
   appears in it as a **read-only** bind mount of the real entry, with its submounts. Nothing is
   copied, and `~/.config`, `~/.local/bin`, and the rest stay readable. Symbolic links are recreated
   as links. Other mount paths that expose the same directory (for example the file system that
   `$HOME` is bind-mounted from) are made read-only too, even with `protect_system: false`, so the
   view offers no writable path into the real `$HOME` except `home_shared` (see "Limits of the box"
   for what a determined process can still reach). New top-level files land in the layer. A harness that
   rewrites its state file with a temporary file and a rename therefore works, and each slot keeps
   its own copy.
   - `home_private_files` (default `.claude.json`): top-level files seeded **once** into the layer as
     a private, writable copy, instead of being bound read-only.
   - `home_private` (default `.cache`, `.buck`): per-slot, persistent, writable directories. A
     top-level entry is simply part of the layer; a nested one is bound from the layer over the
     real path.
   - `home_shared` (default `.claude`, `.codex`, `.muse`, `.config/muse`, `.config/opencode`,
     `.local/share/opencode`, `.local/state/herdr`, `.local/share/muse`, `.cargo/registry`,
     `.cargo/git`): bound **read-write** from the real `$HOME`, so an agent keeps its login,
     settings, and transcripts, and cargo can fill its package caches (`~/.cargo/bin` and
     `~/.cargo/config.toml` stay read-only on purpose; a project that pins a toolchain version
     may also need the toolchain manager's home directory here so it can install that version;
     the literate configuration names it). Missing paths
     are skipped. Only the named paths are writable; the rest of `~/.config` and `~/.local` stays
     read-only. **These directories hold harness settings and hook files that run later outside
     any box**, so an agent that edits them affects its unboxed runs.
   - `home: hidden` binds only the `home_expose` paths (default `bin`, `.local/bin`) read-only
     into the layer, plus the shared and private paths, and covers the aliases with empty tmpfs.
3. **The wrkslots control directory stays read-only**, whatever `protect_system` says: the
   registry, journals, and every other slot (including other slots' image mounts) can be read but
   not written. Other slots' private `$HOME` layers (`<control>/slot-state`) and the image
   directory (`<control>/slot-images`) are masked. Registry commands that write (`heartbeat`,
   `finish`, `write-handoff`) therefore run outside the box, for example from the coordinator.
4. **Writable binds**: the slot, the Git common directories its checkouts commit into, the
   project's blessed `outputs`, and `read_write` paths.
   `outputs` (default `ai_docs`, `experiments`) are relative to the project root, the primary
   checkout that holds `.wrkslots.yml`, and are skipped when missing. `read_write` entries are
   absolute paths; a leading `~` and `$USER` or `$HOME` are expanded, so a project can name a
   per-user credential staging directory as `/var/.../$USER/...`.
5. **Masks** (`home_hidden`, default `.ssh`, `.gnupg`, `.aws`, `.azure`, `.kube`,
   `.docker/config.json`, `.netrc`, `.git-credentials`, `.pgpass`, `.arcrc`,
   `.config/gh/hosts.yml`, `.config/gcloud`): a directory shows as an empty read-only directory
   owned by you, and a file reads as empty (`/dev/null`). This applies under `$HOME` and its
   aliases.
6. **Everything else read-only** with `protect_system` (default), except `/proc`, `/sys`, `/dev`,
   and `/run/user` (the user bus there is what `systemd-run --user --scope`, `busctl`, and terminal
   multiplexers need). Each mount keeps its own `nosuid`/`nodev`/`noexec`/atime flags. A mount that
   cannot be made read-only while you could write to it refuses the run.

`env` adds variables to COMMAND's environment (a leading `~` in a value is your `$HOME`). The
network is not changed. Commands that need a path this view hides should list it in `read_write`
(writable) or rely on the read-only view. A project located under `/tmp` loses everything except
its bound paths behind the fresh `/tmp`.

`wrkslots run SLOT --print -- COMMAND` shows the scope, slice limits, environment additions, and
the helper command (for `root`, the view spec) without running anything.

### Limits of the box

The box stops accidents, not a process that sets out to leave it:

- **The user bus.** `/run/user` stays writable, so a boxed process can ask the user's systemd
  manager to start a *service* (`systemd-run --user` without `--scope`, or `systemctl --user
  start`), which runs outside the box and outside the slot's slice. `systemd-run --user --scope`
  keeps the command inside.
- **`root` isolation** runs COMMAND with your uid and no user namespace, so it can run setuid
  programs (that is its purpose): `sudo` works inside and can do anything sudo allows.
  `no_new_privs` cannot be set, because harness launchers that need a setuid step would then fail.
  It shares the host's PID namespace: a process in the box can reach the real file system through
  `/proc/<pid>/root` of any of your processes outside it, and can ptrace them. A PID namespace
  would close that, but harness launchers that talk to your systemd manager fail inside one (the
  bus cannot identify a peer in another PID namespace).
- **`userns` isolation** runs COMMAND in a user namespace, where setuid programs gain nothing and
  host processes cannot be ptraced or entered through `/proc/<pid>/root`.
- **`home_shared` and `read_write` paths are writable by design**, and harness settings and hooks
  kept there run later outside the box.
- **Git directories are writable** so commits work, which includes the shared common directory of
  a linked worktree: a boxed process can write `.git/hooks/*` (and `config`), which Git runs later
  in other checkouts of the same repository, outside the box.
- **`cgroup` isolation** applies limits only.
- **A coordinator box can write the registry and every slot, by design** (see "Boxing a
  (sub)coordinator"). It is the one box in which the "registry is read-only" rule does not hold.
  It cannot box a command into a slot itself (`wrkslots run` refuses there, because the
  coordinator box hides every slot's private state); slot agents it launches through agentctl
  start in their own terminal panes, outside the coordinator box.
- **The box's own identity is an environment variable.** A process that deliberately sets
  `WRKSLOTS_BOX`, or forges a mount table, can change how image commands and the slot-in-use
  census treat it; that is outside what the box guards against. Real use of a slot (a working
  directory or an open file inside it) is still found either way.

### Configuring the box

`init` writes the full `sandbox` section, every default spelled out, into a new project's
configuration; `init --sandbox-isolation userns|root|cgroup` chooses its `isolation` at the same
time. An existing project keeps what it has; `wrkslots sandbox write-defaults` adds every
missing key (and missing `limits` key) without changing present keys or anything else.
`wrkslots sandbox show-config` prints the effective settings as JSON. Unknown keys are refused.

```yaml
sandbox:
  isolation: userns
  home: ro
  home_shared: [.claude, .codex, .muse, .config/muse, .config/opencode, .local/share/opencode,
                .local/state/herdr, .local/share/muse, .cargo/registry, .cargo/git]
  home_private: [.cache, .buck]
  home_private_files: [.claude.json]
  home_hidden: [.ssh, .gnupg, .aws, .azure, .kube, .docker/config.json, .netrc, .git-credentials,
                .pgpass, .arcrc, .config/gh/hosts.yml, .config/gcloud]
  home_expose: [bin, .local/bin]
  outputs: [ai_docs, experiments]
  read_write: []
  env: {}
  tmp_size: 16G
  protect_system: true
  coordinator_writable: worktrees
  limits: {memory_max: null, memory_high: null, cpu_quota: null, tasks_max: 8192,
           io_weight: null, all_slots_memory_max: null, all_slots_cpu_quota: null}
```

This is the shape only: in the file, each list is written one item per line with a comment above
every key, because the reader does not accept a flow collection spanning lines.

Per-run options override it: `--isolation`, `--home`, `--tmp-size`, `--env NAME=VALUE`,
`--no-protect-system`, and the limit options replace values; `--home-shared`, `--home-private`,
`--home-hidden`, `--home-expose`, `--output`, and `--read-write` add to the configured lists.

### Starting an agent in a slot

`wrkslots shell-command SLOT [--isolation MODE] [--format json] [-- COMMAND...]` prints one
exec-only command line that replaces an interactive shell with `wrkslots run SLOT -- $SHELL -i`
(or, with a COMMAND, with that program boxed; the pane then closes when it exits). It uses
absolute interpreter paths and adds nothing to the shell's environment. With `--format json` it
also prints the slot directory and the effective isolation. An agent launcher runs it in a fresh terminal pane,
waits until the pane's own shell PID is again the foreground shell inside a wrkslots slice,
registers the slot directory as the agent's working directory, and then starts the harness. The
agent and everything it runs stay in the slot's box. Under `root` isolation the launcher instead
runs the COMMAND form with the harness, because the multiplexer cannot start an agent behind sudo.

### Boxing a (sub)coordinator

A coordinator agent creates slots and launches its own subagents into them. It needs the registry
and the slots writable, which no slot's box allows, but it should not be able to write anywhere
else in `$HOME`. `wrkslots box` gives it that box, for any wrkslots project:

```sh
wrkslots box --isolation root --cwd worktrees -- claude      # from the project root
wrkslots shell-command --box --cwd worktrees --format json  # the same, for launchers
agentctl start lead --harness claude --cwd PROJECT/worktrees --project-box --box-isolation root
```

The view is the one `run` builds, with the same flags and defaults, but with no slot and a
different writable set, chosen by `--writable` (default `configuration.sandbox.coordinator_writable`,
else `worktrees`):

| Scope | Writable besides home_shared, outputs, read_write, and /tmp |
|---|---|
| `worktrees` | the managed worktrees directory: every slot, the registry and journals, slot images and state, validate slots; plus the Git directories slots commit into (the project root's repository with its `modules/` and `worktrees/`, every repository directly below the project root, and every repository an active slot uses) |
| `project` | the whole project root, plus Git directories outside it |

In both scopes the agentctl registry in the working directory (`.agentctl`, agentctl's default)
is writable, so the coordinator can launch agents; it is created if missing. A project that keeps
validation logs or other coordinator output outside the worktrees directory either chooses
`project` or lists those paths in `read_write`. Everything else under `$HOME`, including sibling
projects, stays read-only, and the `home_hidden` credentials stay masked.

`$HOME` is the box's own persistent private layer, `<control>/box-state/NAME/home` (`--name`,
default `coordinator`; agentctl uses the agent name), built exactly like a slot's; other boxes'
and slots' private layers are masked. `/tmp` is fresh per launch. Limits apply to the box's own
slice, `wrkslots-box-NAME.slice` below the all-slots slice.

**Image slots and mount propagation.** A coordinator that creates an image slot needs its mount to
be real on the host: workers launched into the slot must see it, and so must the coordinator. Two
rules make that work:

1. A coordinator box receives host mount events (slave propagation) under the worktrees
   directory, so an image slot mounted on the host after the box started appears inside it.
   Everywhere else its mounts are private: a mount made elsewhere on the host is not visible in
   the box. Nothing mounted inside any box ever reaches the host.
2. When `wrkslots create`, `remove`, `image mount|unmount|convert|grow|trim`, or any other command
   mounts or unmounts a slot image from inside a coordinator box, the mount runs in the host's
   mount namespace, as a transient service of your systemd user manager (`systemd-run --user
   --wait --pipe`), which starts the same root image helper through `sudo -n`. The helper checks
   every path again there. The result is verified in the host's mount table, read through the
   same user manager, because the box's own table can keep copies of a slot mount the host has
   already removed. Such a copy would keep the loop device bound (or the FUSE server running), so
   after unmounting, wrkslots replaces the empty mount-point directory with a fresh one on the
   host; the kernel then detaches every copy of the old mount, in every namespace. Removing or
   renaming a slot's mount-point directory happens on the host too. Unmounting is idempotent, so a sequence interrupted halfway is
   finished by running the command again. This works from `userns` boxes too, where sudo itself
   cannot run. If the user manager is unreachable from the box, image commands fail with a
   message saying so; create image slots outside the box in that case.

A slot's box keeps all its mounts private, as before: it needs no mount made after it started,
and it refuses to mount, unmount, convert, or remove slot images (a mount made on the host would
be invisible to it). Run those from outside the box or from a coordinator box.

**Subagents start outside the box.** A worker the coordinator launches through a terminal
multiplexer (`agentctl start --slot`) runs in a pane whose shell the multiplexer's server spawns,
outside the coordinator's box and slice. The worker then boxes itself into its own slot. The
coordinator's box needs only the multiplexer's socket, which is under `/run/user` or in the
shared harness state.

**Harness trust prompts.** A harness that asks whether to trust a folder (Claude, Codex) asks
once per box: Claude keeps the answer in the box's private copy of `~/.claude.json`, Codex in the
shared `~/.codex`. agentctl stops at the prompt for a human to answer.

### Builds with a shared action cache

A build tool whose outputs are materialized on demand keeps only the final outputs in the slot.
For example, Buck2 with `materializations = deferred` and remote execution or a remote action
cache: intermediates stay in the cache, outside every slot, and a second slot building the same
targets gets cache hits without downloading intermediates. The daemon's own state (`~/.buck`) is
one of the private `home_private` directories. Without a remote cache, every intermediate
output is written inside the slot's image.

### The machine-wide guard

`wrkslots limits apply --memory-fraction 0.8 --cpu-fraction 0.9` applies runtime (reboot-cleared)
ceilings to the user's whole `user-UID.slice` through `sudo -n systemctl set-property --runtime`,
so every process the user runs shares one outer bound, including agents that a harness moves
into a slice of its own. `wrkslots limits show` prints the current values; `clear` removes them.

## Recovery and compatibility views

Every mutation appends a numbered, hash-linked JSON event before refreshing the readable ACTIVE,
ARCHIVED, hold, or journal view. Readers derive state from the event history whenever it exists, so
a stale compatibility view cannot override later evidence. A complete event left at an atomic-write
temporary path can be promoted by `recover --discard-partial`; handoff publication temps use
no-replace promotion and require independent durable authority. Owner-written temps must match an
outstanding full-envelope write intent, while checkout-file projections must match a prior
provenance-bearing nonqueued read. An orphan temp or a temp beside a conflicting durable sidecar is
preserved and refused. Malformed or ambiguous files refuse.

Create and removal journals contain the exact paths, Git identities, completed steps, salvage
receipts, and remaining work. If the mutable journal view is missing, `recover` reconstructs the
pending operation from the append-only history. Recovery rechecks every destructive precondition;
it does not trust the earlier participant's conclusion.

## Test scenarios

The source package includes deterministic and stress tests for concurrent creation, owner and
time-to-live disagreement, unavailable liveness, dirty and unpublished salvage, ignored-file
exclusion, validate deletion without salvage, unread handoffs, process use, path fencing, interrupted
operations, hash-chain corruption, missing compatibility views, and later-participant recovery.

The default test run uses a PID namespace for tests that only need isolated process evidence and
runs five host-visible process tests separately. This changes test cost, not coverage or assertions.
