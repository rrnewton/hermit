# wrkviz

Build a self-contained, zoomable website from Codex, Claude Code, or Orc coordinator and subagent
transcripts. Provider importers reconstruct the fork/join tree, agent lifetimes, tool/wait/idle
intervals, interaction edges, and full message views; cached model summaries add phrase,
paragraph, hourly, daily, weekly, monthly, and quarterly levels of detail.

The generated archive is ordinary JSON and Markdown plus dependency-free HTML/CSS/JavaScript. It
is designed to be committed, backed up, copied to another machine, and served with Python's built-in
web server.

## What it provides

- Real-time coordinator and nested-agent tracks with structural spawn/result fork-join edges and
  optional detailed intermediate messages.
- Active, tool-running, waiting, idle, and blocked intervals inside work phases.
- Hindsight short names and lifetime summaries grounded in completed work while retaining official
  paths and coordinator nicknames.
- Phrase, paragraph, cultivated work-summary, condensed-transcript, and rendered Markdown views.
- Separate technical and newcomer-oriented plain-language summaries for selected hours, days,
  weeks, months, and quarters, with an in-modal audience switch.
- A durable newcomer project overview and fail-closed glossary boundary: retired mechanical
  definition artifacts remain auditable, but unclassified candidate strings never become prompt
  context or rendered links.
- A linked site title grounded in structured repository metadata, with durable multi-project and
  multi-host identity plus explicit provenance.
- Versioned content-addressed model caches, a logical-key artifact catalog with context-quality
  scores, append-only raw-log snapshots, and immutable run receipts.
- A zero-model, append-only prompt/response projection plus a read-only query CLI with chronological
  prompt ordinals, inclusive ordinal ranges, stable timeline and message references,
  JSON/JSONL/Markdown/text output, time/team filters, relationship traversal, and separate search
  scopes for owner prompts, agent responses, or the complete provider-neutral transcript.
- A self-contained static website served by a built-in loopback server, with backwards-compatible
  range-sharded timeline loading, deterministic gzip sidecars, immutable digest URLs, strong
  validators, browser revalidation, and no CDN dependency.

Install from the package index:

```bash
python3 -m pip install wrkviz
```

```bash
wrkviz refresh \
  --root-session SESSION_UUID \
  --team example-team \
  --output ./timelines/example-team \
  --timezone America/New_York \
  --project example-project=https://github.com/example/example-project \
  --source-host build-host-01

cd ./timelines/example-team
python3 serve.py --port 8765
# in another shell, inspect every run and the exact model-token ledger
python3 run_stats.py
# or navigate the same archive with its dependency-free Python CLI
./timeline --help
./timeline teams
./timeline agents --team example-team --format jsonl
./timeline show phase:example-team::PHASE_ID --transcript --format markdown
./timeline search "reproducible build" --scope all --limit 10
./timeline search 'backend maturity B3' --in owner-prompts
./timeline search '50% ptrace' --in agent-responses --prompt-author owner
./timeline search KVM --in all-transcript --sort newest --limit 20
./timeline prompts --range 200-300
./timeline prompts --which all --format jsonl > all-prompts.jsonl
./timeline prompts --format jsonl > prompts.jsonl
./timeline stats
# inspect retired glossary cache quality without writing or calling a model
wrkviz audit-glossary --output . --details
```

The generated identity files remain ordinary static-site files. The bundled server transparently
selects deterministic gzip-6 companions for large browser assets and uses ETags so reloads can
reuse validated responses; another basic static server can still serve the identity files.
Modern generated sites start from `data/timeline-v2.json` and fetch immutable UTC-day detail
objects only as their time ranges become visible, while `./timeline` reads the smaller schema-3
shards under `data/timeline-v3/`. A published build no longer writes the schema-1 monolith
`data/timeline.json`: both readers reached it only as a fallback behind those two, and it was a
quarter of a gigabyte. A copy an older build left behind is left where it is rather than deleted
by a rebuild; `wrkviz gc` reports it and, when asked, reclaims it. Full transcript search uses separate content-addressed UTC-day
objects, so it is independent of phase chunking and includes final messages that fall exactly at an
agent lifetime boundary. Search objects contain verbatim message text plus condensed one-line tool
records; they never contain raw tool inputs or outputs. A false-positive-only trigram filter in the
catalog skips UTC-day objects that definitely cannot match the query, while exact matching inside
every selected object remains authoritative. Compact linkage sidecars keep cross-day prompts and
responses connected even when only one text day matches, and current browser exports verify each
content-addressed object's bytes before parsing it. Search input is debounced and shard loading is
concurrency-bounded so normal typing does not accidentally download the whole corpus.

The `teams`, `agents`, `phases`, `rollups`, and `search` commands return stable `team:`, `agent:`,
`phase:`, `rollup:`, `message:`, and `tool:` references that can be copied into `show`. Showing a
message returns its mechanically linked prompt or responses when those records are available in
the export. In an exported package,
`data/export.json` records the requested slice under
`display_window`; query results report the actual contained team and record intervals. Do not infer
the slice from file modification times.

Use `refresh-claude --session-file SESSION.jsonl` for a Claude lineage or
`refresh-orc --source-root PROJECT --root-session SESSION_UUID` for an Orc SQLite lineage. All
three refresh commands accept inclusive `--start-date` and exclusive `--end-date` local-calendar
bounds, or inclusive `--start-time` and exclusive `--end-time` RFC3339 instants for an exact
slice. Each bound may use either its date or time form. `--project LABEL=URL` and
`--source-host HOSTNAME` are repeatable; the first explicit
project is primary. Codex archives infer repository labels and links from structured session
metadata when no override is needed.

Orc coordinator restarts are linked only when explicitly registered. Repeat
`--continuation-session UUID` for a whole successor root. If Orc reused an older root for unrelated
work before the restart, pass compact JSON with the first source-native message ID that belongs to
this logical team:

```bash
wrkviz ingest-orc \
  --source-root PROJECT --root-session ORIGINAL_UUID \
  --continuation-session NEXT_UUID \
  --continuation-session '{"session_id":"REUSED_UUID","start_message_id":"MESSAGE_UUID"}' \
  --team example-team --output ./timelines/example-team
```

The bounded form excludes the reused root's earlier events, agents, spawns, and unrelated task
databases from normalized data and summary inputs. The first successful ingest freezes the native
message boundary; later reruns must preserve the recorded ordered prefix.

Claude Code deletes transcripts older than its `cleanupPeriodDays` (thirty days by default) but
keeps every prompt the owner typed in `~/.claude/history.jsonl`. Register that history as a
prompt-only team so the prompts whose transcripts are gone still reach the timeline:

```bash
wrkviz ingest-claude-history \
  --history-file ~/.claude/history.jsonl \
  --project-pattern '/work/example-project|/worktrees/example-' \
  --team claude-history-host01 --output ./timelines/example-team
```

Only prompts typed in a working directory matching the pattern join the team; sessions whose full
transcript is archived as a `claude` team are left out (`ingest-project` derives that list from
the manifest) so nothing is projected twice. Its events are `user_prompt` records with
`ingress_kind` `claude_history`; it has no responses, tool calls, or edges to offer.

After ingesting one or more teams, build the combined verbatim transcript projection without any
model call:

```bash
wrkviz extract-transcripts --output ./timelines/example-team
cd ./timelines/example-team
./timeline prompts --range 200-300
```

The full chronological JSONL report is `extracted/transcripts/prompts.jsonl`;
`messages.jsonl` adds coordinator responses linked mechanically by provider turn identity.
`timeline prompts` defaults to `--which human`, where durable `owner_human` and `other_human`
labels count as human. `--which bot` selects `agent` and `system`; `--which all` also includes
unknown or externally unattributed records. Selection never guesses authorship from message prose.
`./timeline stats` is the zero-model accounting view: it reports record, whitespace-delimited word,
and UTF-8 text-byte totals separately for mechanically identified human, bot/agent, and unattributed
prompts, mechanically linked and total responses, and generated summaries. It also
shows available versus unavailable project-overview, agent-lifetime, work-phase, and
technical/plain-language rollup summary slots. Repeat `--team` or
provide half-open RFC3339 `--start-time`/`--end-time` bounds to inspect a slice; use `--format json`
for automation. Project overviews are omitted from time-sliced totals because they have no honest
time interval.

For a durable multi-provider registry, put the relative archive output, shared identity, and each
Codex/Claude/Orc source in a strict schema-v1 JSON manifest, then run:

```bash
wrkviz ingest-project --config ./projects/example.json
wrkviz ingest-project --config ./projects/example.json --team codex-example
```

The optional team filter limits provider ingestion; the command still refreshes the monotonic
transcript projection over every normalized archive team. It records zero model calls and does not
build the website. See the user guide for the complete manifest schema.

For transports that omitted sender identity, the registered team can carry audited,
versioned `prompt_authorship_rules`. They match ingress plus optional exact time/message bounds,
never prose, and preserve the original unresolved label alongside the applied rule ID. The default
`timeline prompts` view remains human-only; `--which all` exposes unresolved records too.

Every archive includes its `./timeline` launcher, static site, normalized message JSON, cached
summary data, rendered Markdown, source provenance, and run metadata. `python3 run_stats.py` prints per-run cache,
product, build, and token statistics, followed by the immutable backend receipt ledger. Receipts
are attributed to successful and failed summarize invocations; any usage-less receipt makes the
corresponding actual total explicitly `UNKNOWN` instead of zero. Repeating `summarize` on unchanged
input uses cached results; repeating `build` only regenerates deterministic presentation files. Use
`--backend heuristic --model deterministic-local` for an offline pipeline exercise.

The site can be built before any summaries exist. Agent activity, graph edges, condensed
transcripts, and statistics remain available; sparse phase, lifetime, and calendar records are
explicitly marked unavailable, and only genuine cached summaries receive Markdown files and links.

The default summary configuration is `gpt-5.5` with `--reasoning-effort medium`. A provider or
model failure aborts the summary run and retains its failure receipt; the pipeline never silently
substitutes another model or the heuristic backend.

To use the installed Claude Code CLI as the summary backend, select it explicitly, including the
intended Claude model:

```bash
wrkviz summarize --output ./timelines/example-team --team example-team \
  --backend claude --model sonnet --reasoning-effort medium
```

Claude runs are non-interactive, schema-constrained, and launched in safe mode with tools disabled
and session persistence off. Missing structured output or exact usage is a hard failure; the
pipeline does not reinterpret Claude's plain `result` string or fall back to another backend.

Summarization bounds are independent from ingestion. For example, backfill one exact hour without
truncating durable normalized data:

```bash
wrkviz summarize --output ./timelines/example-team --team example-team \
  --summary-start-time 2026-08-07T02:00:00Z \
  --summary-end-time 2026-08-07T03:00:00Z \
  --rollup-kind hourly --model gpt-5.5 --reasoning-effort medium
```

After those cache entries exist, export the same zero-token website slice elsewhere with
`wrkviz export --archive ./timelines/example-team --output ./hour-site --team
example-team --start-time 2026-08-07T02:00:00Z --end-time 2026-08-07T03:00:00Z --rollup-kind
hourly`. Repeat `--team` to align several independently summarized teams on the same real-time
axis; the generated site keeps team filters, identities, rollups, detail files, and artifact links
collision-safe.

Codex's catalog label **Fast** maps to `--service-tier priority`. Codex runs always override the
child CLI with an effective tier: omission and explicit `default` both mean `default`, while a tier
is rejected for Claude and the offline heuristic backend. The default tier keeps existing summary and
hindsight-name cache identities; priority receives distinct identities. Effective tiers are
retained in batch, invocation, and top-level run provenance.

Run `wrkviz quickstart` for the short tour or
`wrkviz --userguide` for the complete storage, privacy, and rerun contract.
