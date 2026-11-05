# wrkslots

`wrkslots` is the single lifecycle interface for Git worktrees used by coordinators, coding agents,
and disposable validation. It records typed ownership and renewal evidence in an append-only,
hash-linked history, salvages agent work before reclaim, and lets any later participant finish an
operation whose original participant disappeared.

It is deliberately fail-closed. Reclaim requires the recorded time-to-live to expire, the project
liveness command to report dead, the exact owner process identity to be dead, and process, mount,
Git, and path checks to agree. Unavailable evidence refuses. `agent` slots publish dirty and
unpushed work, including initialized Git submodules, before deletion; `validate` slots skip salvage because their evidence must live
outside the disposable checkout. `remove --validate-complete` lets the exact owner clean up a
completed validation slot immediately, or lets a later participant skip only the heartbeat wait
after owner death. An agent owner that is done with a slot but keeps running can give it back with
`release` after writing a handoff; removal then stops waiting for the owner's exit and the
time-to-live, and every other check still runs.

Slots can be stored as plain directories or, the default for new projects, as one sparse disk
image per slot, so an agent's build trees never become host file-system metadata and reclaim
deletes one file. Independently of that storage choice, `wrkslots run SLOT -- COMMAND` boxes a
command (or, through an agent launcher that uses `wrkslots shell-command`, a whole agent) to one
slot. The box applies per-slot memory, CPU, and task limits, a fresh `/tmp`, and a file-system
view with the real `$HOME` read-only under a per-slot private layer and credentials masked. Only
the slot, its Git directories, the project's blessed output directories, and shared harness state
are writable. The box is identical for plain and image slots, is configured in the project's
`sandbox` section, and needs no host configuration. `wrkslots box -- COMMAND` is the coordinator
variant: the same view with no slot, but every slot and the registry writable, for an agent that
creates slots and launches subagents into them; image slots it creates are mounted on the host.
See the user guide's "Disk-image slots", "Running commands and agents inside a slot's box", and
"Boxing a (sub)coordinator".

## Install

```sh
python3 -m pip install ./py/wrkslots
```

Python 3.10 or newer, Git, and Linux `/proc` are required.
Disk-image slots additionally need `mkfs.ext4` (e2fsprogs) and either passwordless `sudo` or
`fuse2fs` with `/dev/fuse`. `wrkslots run` needs a systemd user manager (`systemd-run`, `busctl`)
and, for its file-system view, unprivileged user namespaces (`isolation: userns`) or passwordless
`sudo` (`isolation: root`, for commands that need setuid helpers).

## Quick start

```sh
wrkslots init . --worktrees-dir worktrees/slots \
  --liveness-command tools/agent-liveness.py

wrkslots create slot01 --slot-type agent --coordinator-authorized \
  --agent codex-1 --task task-123 --purpose "fix parser" \
  --coordinator-pid "$COORDINATOR_PID" --owner-pid "$OWNER_PID" \
  --repo product=product --branch product=codex/fix-parser

wrkslots heartbeat slot01 \
  --agent codex-1 --owner-pid "$OWNER_PID" --expected-generation 1

wrkslots remove slot01 \
  --coordinator-pid "$CURRENT_COORDINATOR_PID" --expected-generation 1
```

`init` writes `.wrkslots.yml` as literate YAML, with a comment above every key; the reader
accepts a strict YAML subset and still reads the JSON files of older projects
(`wrkslots config convert --to yaml` converts one). See the user guide's "Configuration file".

Creation requires `--coordinator-authorized` as a readable reminder, not as a claimed permission
boundary. The same-user processes can bypass any such convention. Removal accepts the flag as
optional provenance. Recovery requires it only when starting a new direct cleanup of an
unregistered validation path; resuming the durable journal never depends on the original
coordinator.

An assigned agent can run `create` with its own live `--owner-pid` and the live
`--coordinator-pid` it was given, including when they run in separate terminal panes. The tool
records and rechecks both exact process identities. Only an invoking coordinator can create an
unbound slot or assign another owner, which must be its descendant.

The owner PID must represent that one agent's lifetime. Do not bind several logical agents to a
shared multiplexing supervisor, coordinator, or application-server PID: process ancestry cannot
distinguish those agents, and the shared live generation prevents per-agent liveness and reclaim.
Separate coordinator and owner processes must also be visible in the same Linux PID namespace.

`create` uses an existing source repository and its configured `origin` remote by default. Use
`--remote NAME=REMOTE` when a checkout should use a different configured remote. Add
`--remote-url NAME=URL` when the caller must verify that remote's exact fetch URL; otherwise
wrkslots records the configured URL. Repository paths must be relative: use an ordinary path inside
the project root, or path components of the form `../NAME` or `../NAME/PATH` for a repository in
one direct sibling directory. This is a normalized-path rule rather than a byte-for-byte spelling
requirement, but every other raw `..` traversal, every absolute path, and every path with a symlink
component is refused. Accepted paths are stored in normalized relative form. It creates a new linked worktree and local branch; it
never reclaims another slot to satisfy an allocation.

To register a live slot that already exists on disk but has no active row, first inspect the exact
arguments and then apply the registration with all ownership fields:

```sh
wrkslots import-existing slot01 --help
wrkslots import-existing slot01 --slot-type agent --coordinator-authorized \
  --agent codex-1 --task task-123 --purpose "continue task-123" \
  --repo product=../product --apply --verified-live \
  --owner-pid "$OWNER_PID" --coordinator-pid "$COORDINATOR_PID"
```

`status` is read-only: it returns the complete readable roster and reports every observable
registry/storage disagreement as a typed inconsistency instead of withholding the roster. It can
inspect Git registrations only for source repositories named by readable rows; a repository absent
from the registry is not discoverable until a command such as `create --repo` supplies it. `create`
refuses a name, path, or Git registration that overlaps its requested slot, except for the exact
selected source worktree when that source contains the configured managed root. It may create at a
distinct target while printing unrelated inconsistencies it retained. Other lifecycle mutations remain strict by
default while a managed directory contains worktrees that have no wrkslots record. During a
deliberate migration, place the global `--allow-existing-unregistered-worktrees` flag before such a
command. The command retains those directories and acts only on registered slots; use `wrkslots
audit --format json` to inventory what still needs evidence-based import.

Salvage pushes only to remotes whose URLs are listed in the `salvage_push_remotes` configuration
key (`init --salvage-push-remote URL`, repeatable); absent or empty allows none. Work on any other
remote is kept in a verified local bundle beneath the control directory's `wrkslots-salvage/`, and
removal refuses if that bundle cannot be written and verified. Salvage never stages a file whose
basename starts `HANDOFF`, and its commit message carries no registry task text. If an allowed remote
refuses an agent slot's salvage ref, an operator may rerun that one removal with an existing,
absolute archive directory outside the project:

```sh
wrkslots remove slot01 --coordinator-authorized \
  --coordinator-pid "$CURRENT_COORDINATOR_PID" --expected-generation 1 \
  --salvage-archive-root "$HOME/temp/agent_checkouts"
```

This opt-in fallback writes a separate self-contained Git bundle and schema-1 receipt for each
affected top-level or initialized nested repository. Wrkslots verifies the SHA-256, exact archive
ref, complete restoration into an empty bare repository, and full object connectivity before the
ordinary path fence may remove anything. A refused salvage push of any kind, or an initial fetch
that Git reports as a transport failure and whose retry without redirects is again classified as
a transport failure, is recorded as `archived-local`, never as remote `salvaged`. A fetch failure
that carries a relayed `remote:` line, a `remote error:` packet, an HTTP error, a proxy
authentication demand, a missing repository, or an authentication, host-key, or certificate
problem still refuses, and omission of the option leaves the slot untouched. Over SSH, Git passes
the server's own error text through without marking it, so a server message that reports a
connection failure and names no refusal wrkslots recognises can still be read as a transport
failure; the result is verified local custody, never publication.

For initialized nested repositories, a later source-checkout switch between recognized GitHub
HTTPS and SSH spellings does not block cleanup when both URLs still name the same owner and
repository. Wrkslots salvages through the slot's own checked URL. Different hosts, owners, or
repository names, and non-GitHub URL spelling changes, still refuse.

`import-existing --from-state-file worktree-state.json --source-host-id ID` admits a slot whose
owner has already exited from an exact version 3 source row. It preserves the row and source-file
digest, starts a fresh heartbeat time-to-live, and records the complete candidate ACTIVE row before
publication so `recover` can finish after the original participant disappears. Rows without an
owner sidecar refuse rather than turning unavailable ownership into permission to delete.

An unregistered validation checkout or `validate-cargo-*` directory with no recoverable owner is
not imported with invented ownership. Destructive recovery of an ordinary linked Git worktree is
disabled: wrkslots cannot atomically exclude both its checkout and same-UID-writable Git
administration, so the checkout is preserved for inspection. Use `import-existing` only for a
demonstrably live owner. The separate `validate-cargo-*` recovery route accepts an exact terminal
run record, or an explanation when no record survives, and records the exact path and filesystem
identity before bounded cleanup. Ordinary `status` reports every remaining unregistered directory
as an inconsistency and still returns the readable active roster; it does not authorize cleanup or
make an inconsistent row healthy.

Agent drift has separate bounded recovery commands. `recover-absent-agent-row` accepts one exact
ACTIVE identity, publishes and reads back every recorded commit (it refuses a remote not listed in
`salvage_push_remotes`), removes only matching stale Git registrations, and archives before removing
the row. `recover-ownerless-agent-worktree` accepts one
exact unregistered worktree and its complete Git identity, salvages authored and nested-repository
work without assigning an owner, task, or handoff, then fences and removes it. A present HANDOFF.md
requires its exact SHA-256 after the coordinator reads it; that content is rechecked and copied
beside the registry as `HANDOFF-OWNERLESS-RETIRED.*.md`, never into the salvage commit. Any other
uncommitted handoff file refuses. `recover-ownerless-agent-cache` relocates only its one explicitly supported cache
tree outside the managed slot root; it is not an exemption for arbitrary directories.

When a source repository moves, its rows report `repository-evidence-unavailable`.
`relocate-repository FROM TO` rewrites the recorded path of every affected row on this machine after
proving TO is the same repository; see the user guide.

Agent handoffs can live outside their checkouts. `write-handoff` copies bounded UTF-8 from an exact
live owner into a generation-bound control sidecar; it never exempts checkout dirt from `finish`.
`read-handoff` prints the exact contents and durably enqueues that slot generation without removing
anything. `retirement-queue` inspects pending items, and `retire-pending --limit N` attempts each
through the unchanged ordinary remove state machine, retaining every refusal for a later retry.
Existing direct-child `HANDOFF.md` files are copied only when read, must agree with any sidecar, and
remain in place until the slot is successfully archived and removed.

An owner that is done with a slot but keeps running gives it back explicitly:

```sh
wrkslots write-handoff slot01 --agent codex-1 --owner-pid "$OWNER_PID" \
  --expected-generation 1 --from-file /path/to/HANDOFF.md
wrkslots release slot01 --agent codex-1 --owner-pid "$OWNER_PID" --expected-generation 1
```

Only the exact recorded owner process generation or its descendant can release, and only a slot
that has a handoff and no hold. The release is bound to the owner identity, the generation, the
heartbeat stamp, and the handoff digest, and it stops that generation's heartbeat. After the
coordinator reads the handoff, `remove` and `retire-pending` no longer wait for the owner to exit
or for the time-to-live; they still refuse while any process, the owner included, uses the slot,
and they still salvage before deleting. `status`, `doctor`, `audit`, and `retirement-queue` show
`RELEASED by live owner at <time>`. See the user guide's "Giving a slot back while the owner keeps
running".

If a command reports an interrupted operation, preserve the paths and run:

```sh
wrkslots recover --coordinator-pid "$CURRENT_COORDINATOR_PID"
```

Run `wrkslots quickstart`, `wrkslots COMMAND --help`, or `wrkslots --userguide` for the complete
contract.

For a periodic coordinator reminder, `wrkslots audit --gate` returns 1 when a slot is ready for
reclaim or an interrupted/unregistered slot needs attention, 2 when expired-slot evidence is
unavailable, and 0 when no action is currently indicated. A configured batch liveness command is
called once for the complete subject-bound registry set; malformed, missing, duplicate, or misbound
results fail closed. Recursive cache bytes use a bounded resumable census: incomplete counts are
reported as null/partial and cannot make a slot deletable. JSON includes per-phase CPU, wall, and
work counters. Audit never removes anything or changes lifecycle state; it may update only its
regenerable project-keyed cache-accounting state below `XDG_CACHE_HOME` or an explicit census path.

## Development and stress testing

```sh
make test TEST_SUITE=python
python3 tests/e2e_stress.py --seed 1 --workers 8 --seconds 20
```

The lifecycle suite uses a PID namespace where possible to retain real process checks without
repeatedly scanning every process on a shared host. Five tests require host-visible process
evidence and run outside that namespace. Coverage and assertions are identical.

See [USER_GUIDE.md](USER_GUIDE.md) and [RELATED_WORK.md](RELATED_WORK.md).
