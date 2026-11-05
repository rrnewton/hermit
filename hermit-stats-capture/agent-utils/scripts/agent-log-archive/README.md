# Agent log archive fetcher

This directory is the source-controlled template for a private, non-deleting
archive of agent-harness state collected from several machines. The canonical
fetcher is Python 3.10+, is mypy-strict, and uses only the standard library.
`fetch_agent_logs.sh` is a compatibility launcher for existing cron entries.
Transfers require `rsync` and GNU `du -bl`. Remote sources also require
batch-mode SSH plus GNU `readlink -f` and `find -printf`.

## Install and configure

Install the fetcher into a private archive directory, then replace the example
inventory with real hosts:

```bash
archive_dir="$HOME/agent_logs_archive"
install -d -m 0700 "$archive_dir"
install -m 0755 scripts/agent-log-archive/fetch_agent_logs.py "$archive_dir/"
install -m 0755 scripts/agent-log-archive/fetch_agent_logs.sh "$archive_dir/"
install -m 0644 scripts/agent-log-archive/log_roots.tsv "$archive_dir/"
install -m 0644 scripts/agent-log-archive/machines.example.tsv "$archive_dir/machines.tsv"
```

The fetcher refuses a group- or world-accessible archive. `machines.tsv` has
three tab-separated columns:

```text
# short_name	host	absolute_home
build01	build01.example.com	/home/example
```

`log_roots.tsv` has four:

```text
# machine_or_*	archive_name	source_path_relative_to_home	requirement
*	.codex	.codex	required
build02	gas-town-main	work/my-town	required
```

`*` applies a root to every machine. A machine-specific row may add a distinct
archive name, but may not collide with a wildcard destination. Names, machine
endpoints, scopes, relative paths, and the fully expanded destination matrix
are validated for uniqueness and containment before a run starts.

The four required defaults cover the observed Orc, Claude, and Codex transcript
stores. Optional defaults cover per-user Gas Town/Beads state, alternate Claude
accounts, and Gas Town's default town location.

## Run

Inspect and probe the selected matrix before the first transfer:

```bash
cd "$HOME/agent_logs_archive"
./fetch_agent_logs.sh --list
./fetch_agent_logs.sh --check-sources
./fetch_agent_logs.sh --dry-run
./fetch_agent_logs.sh
```

`--machine NAME` and `--root NAME` are repeatable selectors. `--check-sources`
is a quick presence/reachability check and does not walk full manifests.
`--dry-run` runs rsync and records planned transfer metrics without copying
source members.

View the quick history without reading the large receipts:

```bash
./fetch_agent_logs.sh --history
```

The first Python run automatically backfills missing terminal Bash receipts and
migrates the legacy ten-column history schema, preserving its approximate byte
value and saving the original as a read-only `history.legacy-*.tsv` file.
The explicit, idempotent maintenance command is:

```bash
./fetch_agent_logs.sh --backfill-history
```

Both paths de-duplicate by `run_id`. Unknown legacy metrics remain blank and
carry explicit `unknown` or `legacy_approximate` provenance; they are
never invented as zero.

Every real or dry run estimates source size and checks filesystem capacity.
Because an incremental rsync may transfer much less than the complete source,
an estimate larger than free headroom is a warning. Set a hard reserve when
desired:

```bash
./fetch_agent_logs.sh --min-free-bytes 10737418240
```

The reserve is checked before every transfer against a refreshed, conservative
full-source estimate, so it can intentionally block an incremental transfer
that might in practice need less space. A blocked operation still gets a
complete result row. Required-source absence, reachability failures, source
disappearance after a successful probe, rsync failures, and post-copy failures
are distinct statuses.

## Receipts and quick history

An exclusive `_fetch_state/fetch.lock` serializes runs. Rsync partials live
under `_fetch_state/partials/` and are retained for the next invocation. The
fetcher never passes an rsync deletion option.

Each invocation creates `_fetch_runs/<run-id>/` containing:

- `run.tsv`: run timing, status, operation counts, capacity, and aggregate
  metrics;
- `results.tsv`: exactly one detailed row per selected machine/root operation;
- `fetch.log`: the human-readable probe and rsync transcript;
- `warnings.jsonl`: structured capacity, rewrite, manifest, and SQLite warnings;
- `manifests/<machine>/<root>.jsonl`: observed post-rsync source membership;
- exact config snapshots and fetcher SHA-256 values.

Completed receipt files are fsynced and mode `0400`; their directories are
mode `0500`. Only after sealing does the fetcher atomically append and fsync
the history row.
`_fetch_runs/latest` points to the newest completed receipt.
`_fetch_runs/history.tsv` is the quick append-only index: one row per completed
invocation with start/end time, duration, status, complete operation counts,
logical transferred bytes/files, manifest totals, retained-member counts,
SQLite warning counts, and free space before/after. It separately records
rsync's logical selected size, literal/matched payload, and sent/received wire
bytes, plus counts of operations whose stats were complete or unknown. Dry-run
rows use the same columns but their mode identifies transferred values as
planned.

## Manifests, retention, and SQLite

Deletion is intentionally disabled. Source-side rotation therefore leaves
source-absent paths in the destination as retained history. The per-run JSONL
manifest—not a blind walk of the accumulated destination—is the authoritative
membership observation for that run. It records the configured and canonical
source roots plus every observed relative path, type, size, nanosecond mtime,
and symlink target. An unstable manifest fails its operation. Warnings call out:

- destination SQLite `-wal`/`-shm` files absent from the source manifest;
- retained source-absent SQLite database files;
- a current source file smaller or older than its previous destination member;
- live source WAL/SHM files and source changes observed around transfer.

Nothing in those cases is deleted. A raw file-by-file rsync cannot promise a
transactionally coherent snapshot of a live SQLite database. Do not open an
archive database in place: a retained stale WAL can be mistaken for a current
sidecar and opening SQLite can itself change files.

For analysis, first materialize an immutable working copy containing only paths
listed by the chosen run manifest. Copy each database together with the WAL and
SHM members listed for that same run. On that working copy, use SQLite's backup
API or `.backup` to produce a separate database, then run
`PRAGMA quick_check` on the backup. If the copied DB/WAL set cannot be opened or
validated, obtain a source-side SQLite backup; the rsync receipt must not be
treated as transactional evidence.

Full harness trees can contain credentials, private prompts, and symlinks into
working trees. Keep the archive private and unpublished. Destination container
paths may not be symlinks or escape the archive. A configured source root must
resolve below its machine home and may not overlap the local archive. The
canonical source is pinned between the final probe, rsync, and manifest.
Symlinks inside that source tree are preserved, not followed
(`--copy-links` is never used).
