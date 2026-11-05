# wrkviz user guide

`wrkviz` turns a noisy coordinator/subagent transcript into a durable hierarchy of
summaries and a zoomable local website. It answers both “what was happening at 02:13?” and
“what did this team accomplish this month?” without throwing away the underlying messages.

The importers support Codex multi-agent rollouts, Claude Code coordinator lineages, the Claude
Code prompt history that outlives those lineages, and Orc SQLite coordinator/task history. The archive and browser schema is provider-neutral so additional adapters
do not require site changes.

## What the site shows

- Packed lanes are the default: non-overlapping agent lifetimes share the first available lane, so
  a long run does not grow one row per completed agent. Click a packed lane label to list every
  named agent assigned to that lane and select one directly. “Per-agent tracks” restores the full
  fork-tree view when that is more useful.
- Every agent has a hindsight short name based on its completed work, ancestor context, official
  coordinator path, role, and nickname. The short name is primary; hover, search, and detail views
  retain the full official path and coordinator nickname. The same hindsight pass writes a concise
  lifetime paragraph from all of the agent's work phases; hover shows that paragraph without another
  model call. Nested descendants are not depth-limited.
- A whole spawned interval is an **agent lifetime**; each summarized sub-block is a **work phase**.
  Work-phase boxes carry a short phrase at useful zoom levels. Their bottom strip distinguishes active,
  tool-running, waiting, idle, and explicitly blocked time.
- Semantic zoom keeps long histories responsive. At one minute per pixel or closer, the site shows
  phases, state strips, and applicable message edges. From one to fifteen minutes per pixel it shows
  one lifetime block per agent plus structural fork/join edges. Farther out, each team becomes a
  compact activity heat strip with no tiny agents or edges: height represents average worker
  concurrency and saturation represents observed active coverage. The strip selects hourly, local
  daily, or local Monday-to-Sunday weekly bins, leaves truly inactive intervals blank, and does not
  count time before the first or after the last captured record as inactivity. Local day/week bins
  follow daylight-saving boundaries.
- Thick curved edges are structural forks (parent-to-child spawns), joins (terminal
  child-to-parent results), and explicitly configured coordinator-session continuations, so all
  remain visible. A continuation does not invent a return arrow for the successor coordinator.
  Detailed intermediate message edges are hidden globally by default and appear for the selected
  agent or work phase; both detailed-message behaviors have toolbar toggles.
- Hovering a phase or edge shows its paragraph summary and statistics.
- Single-clicking selects an agent; a later single click on the same work phase narrows selection to
  that phase. Clicking a different phase selects that phase directly; clicking the selected phase
  again returns to the whole-agent selection. Click empty track background (or press Escape) to
  clear selection. Double-click opens three views: the cultivated Agent Work Summary, the full
  prompt/response transcript with tool use condensed to one line and role filters, and its rendered
  Markdown summary.
- Single-clicking a day, week, month, or quarter selects it; double-click opens rendered Markdown
  with **Technical** and **Plain Language** tabs. The latter introduces the project and explains
  specialized terms for a newcomer. Right-clicking a rollup, agent lifetime, or work phase offers
  range-appropriate zoom-to-fit actions that trim empty leading and trailing time without leaving
  the selected range. Horizontal trackpad gestures pan the time axis.
- The fixed footer recomputes user prompts, agent responses, inter-agent messages, tool calls, and
  active agents for the visible time range.
- Daily, weekly, monthly, and quarterly markers link the visible range to long-term summaries.
- Explicit GitHub pull-request URLs and `owner/repository#number` references become safe links in
  work summaries and transcripts. Ambiguous naked `#number` text stays plain unless importer input
  supplies repository context for that exact message.
- **Project glossary** is reserved for the all-time semantic concept catalog. The renderer accepts
  only supported, evidence-backed semantic concepts and never accepts model-authored link targets.
  During the semantic migration, definition-only records remain in the immutable cache for
  provenance and cost accounting, but they are not publication authority: a zero-token build leaves
  the catalog empty rather than link unclassified strings.

The browser is self-contained SVG/HTML/CSS/JavaScript. A pinned MIT-licensed `markdown-it` browser
bundle renders summary headings, lists, tables, blockquotes, and code with raw HTML disabled.
Markdown links, images, autolinks, and linkification are suppressed; after rendering, the UI adds
only schema-validated glossary, pull-request, and work-artifact links. It is vendored into every
archive, so the generated directory has no CDN or runtime package-manager dependency and remains
usable when copied offline.

## Install

```bash
python3 -m pip install wrkviz
```

Python 3.10+ is required. Summarization uses an installed `codex` CLI by default; ingestion,
formatting, serving, and the deterministic `heuristic` backend use only the Python standard library.

## Find the coordinator session

Codex rollouts live under `~/.codex/sessions/YYYY/MM/DD/`. The first JSON line is `session_meta` and
contains `payload.id`, `payload.cwd`, and the source. A coordinator root has `source: "cli"`; child
metadata names `parent_thread_id`, `agent_path`, depth, and nickname.

For example, list recent roots for one checkout:

```bash
rg -l -m1 '"cwd":"/path/to/project"' ~/.codex/sessions/2026/08/05 \
  | while read -r file; do head -1 "$file"; done \
  | jq -r 'select(.payload.source == "cli") | [.payload.id,.payload.cwd] | @tsv'
```

Pass the coordinator `payload.id` to `--root-session`. Descendants are discovered by their stable
lineage `session_id`; no list of child files is needed.

If the same logical coordinator work restarted in a new root session, add its root UUID explicitly:

```bash
wrkviz ingest \
  --sessions-root ~/.codex/sessions \
  --root-session ORIGINAL_ROOT_UUID \
  --continuation-session NEXT_ROOT_UUID \
  --continuation-session LATER_ROOT_UUID \
  --team example-team --output ./timelines/example-team
```

The option is repeatable in chronological order. Time proximity alone never links sessions. On the
first successful ingest, the manifest freezes the exact predecessor source line/timestamp,
successor start, and gap for each link. Later reruns may omit all continuation options and reuse the
recorded chain, or repeat its exact prefix and append another successor. Removing, reordering, or
replacing a recorded successor is rejected; use another archive if the chain was specified
incorrectly. `refresh` accepts the same option and applies it during its ingest stage before any
summary or build work.

Claude Code root sessions live under `~/.claude/projects/<encoded-project>/` as
`<session-uuid>.jsonl`. Pass that file to `--session-file`; the importer discovers nested
`<session-uuid>/subagents/agent-*.jsonl` files and their metadata sidecars recursively. It reads
assistant text, user prompts, tool calls/results, Agent spawns, and SendMessage edges while omitting
private thinking blocks.

### Claude prompt history: the prompts that outlive their transcripts

Claude Code removes any transcript under `~/.claude/projects/` that has not been modified within
its `cleanupPeriodDays` setting -- thirty days unless `~/.claude/settings.json` says otherwise --
and it does so without a prompt. What it never removes is `~/.claude/history.jsonl`: one line per
prompt the owner submitted, with the verbatim text, pasted attachments, a millisecond timestamp,
the working directory, and the session id. Archive it as its own team:

```bash
wrkviz ingest-claude-history \
  --history-file ~/.claude/history.jsonl \
  --project-pattern '/work/example-project|/worktrees/example-' \
  --covered-session SESSION_UUID \
  --team claude-history-host01 --output ./timelines/example-team
```

`--project-pattern` is a Python regular expression searched against each prompt's recorded
working directory; only matching prompts join the team, and the run says on stderr how many it
left out. Nothing is inferred from the prompt text. The pattern is the team's identity -- a
re-ingest under a different pattern is refused, because that would be a different selection
under the same slug; register another team instead.

`--covered-session` names a Claude session whose full transcript is already archived as a
`claude` team. Its prompts are left out of the history team so the prompt projection does not
carry them twice, and the exclusion is counted in the receipt. The history team therefore holds
exactly the prompts the archive has no other record of. In a project manifest the list is derived
from the manifest's own `claude` teams:

```json
{
  "slug": "claude-history-host01",
  "provider": "claude-history",
  "source": {
    "history_file": "../host01/.claude/history.jsonl",
    "project_pattern": "/work/example-project|/worktrees/example-",
    "covered_sessions": "config"
  }
}
```

`covered_sessions` is `config` (the default) or `none`. It is read off the whole manifest, not the
run's `--team` filter, so ingesting the history team alone gives the same answer as ingesting
everything.

What the team contains is deliberately narrow and deliberately labelled. Every event is a
`user_prompt` whose `ingress_kind` is `claude_history` and whose `author_kind` is `owner_human` --
the history is written only for what the owner typed. Each session is one coordinator agent whose
lifetime is the span of its prompts, so the transcript projection, which reads coordinator threads,
sees all of them; a prompt with no session id (the oldest releases wrote none) is filed under the
`unattributed` thread rather than dropped. There are no responses, tool calls, or edges, and each
prompt's one-second turn marks an instant rather than measuring anything -- the same convention an
Orc task note follows. Pasted attachments are appended to the prompt text under their
`[Pasted text #N]` label. The snapshot is held to append-only like every other vendor source.

For Orc, pass the project directory containing `.orc/` and `.tg/` as `--source-root`. The root
coordinator UUID names a directory under `.orc/sessions/`; child sessions with an explicit parent
identifier are followed as nested coordinators. Whole-root restarts use repeatable plain
`--continuation-session UUID` values. When Orc reused a pre-existing root that contains earlier,
unrelated work, use a source-native message boundary instead:

```bash
wrkviz ingest-orc \
  --source-root ~/agent_logs_archive/devbig014 \
  --root-session ORIGINAL_UUID \
  --continuation-session NEXT_UUID \
  --continuation-session '{"session_id":"REUSED_UUID","start_message_id":"MESSAGE_UUID"}' \
  --team example-orc --output ./timelines/example-orc
```

The JSON form is deliberately explicit. Its `start_message_id` must resolve exactly once in the
successor's append-only `messages` table. The snapshot manifest freezes both that native identity
and its resolved source row/timestamp. Normalization excludes pre-boundary events, inactive agent
history, spawns, and unrelated task databases; a display `--start-time` is not a substitute because
display windows do not redefine source lineage.

## Register a multi-team project

Use `ingest-project` when a durable project archive draws from several machines or providers. The
schema-v1 JSON manifest is the source of truth for team slugs, coordinator roots, source paths,
continuations, timezone, and site identity:

```json
{
  "schema_version": 1,
  "output": "../summary/example-project",
  "timezone": "America/New_York",
  "projects": [
    {
      "label": "example-project",
      "repository_url": "https://github.com/example/example-project"
    }
  ],
  "teams": [
    {
      "slug": "codex-example-014",
      "provider": "codex",
      "source_hosts": ["build014.example.com"],
      "source": {
        "sessions_root": "../devbig014/.codex/sessions",
        "root_session": "ROOT_UUID",
        "continuation_sessions": ["NEXT_ROOT_UUID"]
      }
    },
    {
      "slug": "claude-example-176",
      "provider": "claude",
      "source_hosts": ["build176.example.com"],
      "source": {
        "session_file": "../devbig176/.claude/projects/PROJECT/SESSION.jsonl"
      }
    },
    {
      "slug": "orc-example-014",
      "provider": "orc",
      "source_hosts": ["build014.example.com"],
      "prompt_authorship_rules": [
        {
          "id": "owner-web-bootstrap",
          "ingress_kind": "submitted_web",
          "author_kind": "owner_human",
          "reason": "The audited Web channel was owner-only before relay automation.",
          "end_time": "2026-08-10T02:57:56.854Z"
        }
      ],
      "source": {
        "source_root": "../devbig014",
        "root_session": "ORC_SESSION_UUID",
        "continuation_sessions": [
          "NEXT_ORC_SESSION_UUID",
          {
            "session_id": "REUSED_ORC_SESSION_UUID",
            "start_message_id": "SOURCE_NATIVE_MESSAGE_UUID"
          }
        ]
      }
    }
  ]
}
```

Run it from any directory; relative paths are anchored to the manifest, not the shell's working
directory:

```bash
wrkviz ingest-project --config ~/agent_logs_archive/projects/example.json
# Refresh only one registered provider lineage while testing:
wrkviz ingest-project --config ~/agent_logs_archive/projects/example.json \
  --team codex-example-014
```

`output` must be relative to the config file. Provider source paths may be relative to that file or
absolute. Top-level `timezone`, `projects`, `source_hosts`, and `window` values are defaults; a team
may replace any of them. `projects` contains exact `label` and `repository_url` objects, while
`source_hosts` is an array of validated hostnames. A `window` may contain `start_date` or
`start_time`, plus `end_date` or `end_time`, using the same inclusive/exclusive rules as the CLI.

The parser rejects missing and unknown keys, unknown providers, duplicate team slugs, duplicate
continuations, absolute output paths, invalid identity, and invalid time bounds. Provider `source`
objects are exact: Codex accepts `sessions_root`, `root_session`, and optional ordered
`continuation_sessions`; Claude accepts only `session_file`; `claude-history` accepts
`history_file`, `project_pattern`, and optional `covered_sessions`; Orc accepts `source_root` and
`root_session`, plus optional ordered `continuation_sessions`. An Orc continuation is either a
whole-root session-ID string or an exact object containing `session_id` and `start_message_id`.

Some Web or terminal transports store no sender identity. A team may add
`prompt_authorship_rules` to refine only those unresolved records. Each rule requires a unique
`id`, exact `ingress_kind`, target `author_kind` (`owner_human`, `other_human`, `agent`, or
`system`), and an audit `reason`; optional `start_time`/`end_time` bounds are half-open RFC3339
instants, and optional `source_native_ids` restrict the rule to exact messages. Rules never inspect
message prose, cannot override provider-attributed records, and may not overlap. Extraction writes
the normalized active set to `extracted/transcripts/authorship-rules.json` and records the rule ID
and original source classification on every corrected prompt. A config-driven ingest replaces the
complete active rule set; a later direct `extract-transcripts` reuses it.

A rule belonging to a team the archive has no data for yet — the normal state of a newly registered
team whose first ingest has not succeeded — is **set aside, not fatal**. It is named on stderr and
in `dropped_prompt_authorship_rules` in both the run receipt and the transcript manifest, and it
starts applying by itself as soon as that team ingests. Nothing needs re-running by hand.

This command is entirely mechanical. It snapshots and normalizes the selected registered teams,
then refreshes `extracted/transcripts/` over **every** normalized team already in the archive so the
global prompt database remains monotonic. A `--team` filter limits ingestion, not that final global
projection. Its run receipt binds the exact config-file SHA-256, records zero model calls/tokens,
and confirms that no website build ran; use `build` or `export` explicitly when presentation files
should change.

One unreadable team no longer stops the rest. Each configured team is ingested independently: a
team that fails is reported and skipped, the remaining teams still ingest, and transcript
extraction still runs — a failed team simply keeps contributing the records it contributed on its
last good run, because extraction reads the archive's durable normalized snapshots rather than this
run's results. The failure is never silent. Every failed team is printed to stderr with its
provider and exact error, the summary line says how many teams succeeded and how many failed, the
run receipt records `status`, `teams_succeeded`, `teams_failed`, and a `failed_teams` array with
each team's slug, provider, error type, and message, and the command exits non-zero:

```
codex-example-014 (codex): ingest: 34 agents, 5182 messages/events, ...
wrkviz: team orc-example-014 (orc): OrcParseError: Orc session existing append prefix was rewritten for sessions/SESSION_UUID.db
teams: 2 succeeded, 1 failed
transcripts: 91422 prompts, 88190 responses, 4310 system inputs across 12 normalized archive teams; 6 files changed
```

### A team the projection could not read is carried, not fatal

The same isolation holds one level down, inside extraction. A team whose stored snapshot cannot be
*loaded* — most often one left torn by a crash between two of the ingest's writes, which the
generation validator then correctly refuses — is skipped rather than ending the projection. It
keeps contributing everything it contributed on its last good extraction, and its authorship rules
stay in force; what is missing is only what has happened to it since.

That is stated four times over, because a partial projection that reads as complete is worse than
an outright failure. The stdout count does not absorb the carried team, every skipped team gets its
own stderr line, the run receipt gains `skipped_teams` / `teams_skipped`, and
`extracted/transcripts/manifest.json` records the same for whoever opens the archive later:

```
wrkviz: transcripts: team orc-example-014: ValueError: incomplete Orc normalized generation for 'orc-example-014'; rerun ingest -- carried forward unchanged from its last good extraction; nothing since is projected
transcripts: 91422 prompts, 88190 responses, 4310 system inputs across 11 normalized archive teams (1 further team(s) carried forward unread); 6 files changed
```

Both `ingest-project` and `extract-transcripts` exit 2 when the projection came out partial, even
if every team they ingested succeeded — extraction covers the whole archive, so a clean ingest
proves nothing about the projection. Fix the named team and re-run; the projection picks its rows
up with no other intervention. Extraction fails outright only when **no** team can be read at all,
and then it names every team and every cause.

Fix the reported team and rerun; ingestion is idempotent, so the healthy teams do no work the
second time and the repaired team's rows join the projection. If extraction itself fails — which a
partially-updated archive can still cause — the per-team results are kept and reported anyway, and
the receipt records `transcript_extraction_error`.

## End-to-end refresh

```bash
wrkviz refresh \
  --sessions-root ~/.codex/sessions \
  --root-session 019fcfe7-0f68-7301-8aab-c2f90a7026c7 \
  --team example-team \
  --output ./timelines/example-team \
  --timezone America/New_York \
  --project example-project=https://github.com/example/example-project \
  --source-host build-host-01 \
  --backend codex \
  --model gpt-5.5 \
  --reasoning-effort medium \
  --service-tier priority \
  --summary-workers 3 \
  --name-batch-size 12
```

`refresh` is exactly `ingest`, then `summarize`, then `build`. It records a new immutable JSON run
receipt under `runs/` and updates `manifest.json` with the latest run and source digest.

The upper-left site title identifies what the archive contains. `--project LABEL=REPOSITORY_URL`
and `--source-host HOSTNAME` are repeatable; the first explicit project is the primary title link,
while secondary repositories and full hostnames remain available from the adjacent identity
disclosure. A combined dataset with no single primary uses the compact `multi-repo` label; more
than one execution host uses `multi-host`. Codex ingestion also reads `cwd`,
`git.repository_url`, and structured hostname fields
from session metadata. It never guesses identity from free-form prompt or response prose. Explicit
values and later structured discoveries are accumulated rather than discarding already recorded
projects or hosts. Orc backups commonly lack host metadata, so pass `--source-host` for them.

For Claude Code, use the provider-specific source selector; the remaining summary and build options
are identical:

```bash
wrkviz refresh-claude \
  --session-file ~/.claude/projects/PROJECT/SESSION_UUID.jsonl \
  --team claude-project --output ./claude-project \
  --timezone America/New_York \
  --start-date 2026-07-31 --end-date 2026-08-03 \
  --backend heuristic --model deterministic-local
```

For one bounded Orc day, use its provider-specific source selector with the same summary and build
options:

```bash
wrkviz refresh-orc \
  --source-root /path/to/project \
  --root-session SESSION_UUID \
  --team orc-project --output ./orc-project \
  --timezone America/New_York \
  --start-date 2026-07-21 --end-date 2026-07-22 \
  --backend heuristic --model deterministic-local
```

`--start-date` is inclusive and `--end-date` is exclusive in `--timezone`. Earlier transcript
context remains available to phase summarization, but the website, statistics, naming, and calendar
rollups are bounded to the requested dates. A daylight-saving boundary may therefore span 23 or 25
hours. Reuse the same bounds when refreshing an archive; use another output directory for different
bounds.

For a non-calendar slice, use RFC3339 instants with an explicit offset or `Z`:

```bash
wrkviz ingest-orc \
  --source-root /path/to/project --root-session SESSION_UUID \
  --team orc-project --output ./orc-project \
  --timezone America/New_York \
  --start-time 2026-08-06T22:00:00-04:00 \
  --end-time 2026-08-07T07:00:00-04:00
```

`--start-time` is inclusive and `--end-time` is exclusive. A date and exact time may be mixed at
opposite bounds, but the date and time forms for the same bound are mutually exclusive. Instants
are canonicalized to UTC milliseconds in archive metadata.

### Start the website

Every output archive includes its own launcher:

```bash
cd ./timelines/example-team
make serve                 # http://127.0.0.1:8765/
make open                  # also ask Python to open the browser
make run-stats             # every pipeline run plus exact model-token accounting
PORT=9000 make serve
```

Or use the installed command:

```bash
wrkviz serve --output ./timelines/example-team --port 8765 --open
```

Do not open `index.html` directly with `file://`; browsers block its JSON fetch. The launcher uses
Python's built-in loopback HTTP server and exposes nothing on the network. Builds retain each
ordinary identity file and add deterministic gzip-6 companions for browser-facing files of at
least 1 KiB. The bundled server negotiates those companions, emits representation-specific strong
ETags, and lets the browser revalidate mutable files.

It also answers a single `Range` header with a `206`, and that is now load-bearing rather than a
nicety: the page reads the timeline out of `data/timeline-v3/`, one gzip member of one shard at a
time, at the byte offsets that shard's `.index.jsonl` sidecar published. A first paint downloads
the 169 KB bootstrap and two line ranges out of each team's spine — everything except the phase
cards and the zoom bounds, which are fetched when you open an agent lifetime or zoom to one. On a
twelve-team archive that is **2,155,957 bytes against the previous format's 4,502,381**, in 34
requests against 2. The parse is the same either way; what halves is the transfer, and the extra
requests are the price of asking for parts of files instead of whole ones. Copying the archive to a generic static
server remains compatible — it can ignore the `.gz` companions and serve the identity files, and
byte ranges are something every static server already does. A server that ignores `Range` entirely
is still correct: the page detects the whole-file answer and slices the member out of it, which
costs bandwidth and nothing else.

Transcript search in the browser follows the *corpus*, which is not always the same generation as
the timeline. An archive built after the schema-3 timeline and before the schema-3 search streams
has its corpus in `data/timeline-v2.json` and nowhere else; the page reads it from there, exactly
as `wrkviz search` does, and says so in the line under the title. It refuses instead
of falling back when the two bootstraps disagree about their `source_digest` — that is two
different builds, and answering phases out of one and messages out of the other would show
messages inside phases they never occurred in. A rebuild moves the corpus into schema 3 and the
note goes away.

## Separate stages

The separation is a hard cost-control boundary, not just an implementation detail.

### 1. Ingest — no model calls

```bash
wrkviz ingest \
  --root-session SESSION_UUID --team example-team --output ./timelines/example-team
```

Ingestion follows the whole session lineage, removes forked-history duplicates, joins tool calls to
their completions, and writes canonical UTC timestamps. Verbatim user/assistant message text is
retained. Bulky tool stdout, command bodies, and patches stay in the authoritative Codex JSONL;
the archive stores their name, interval, status, and nested tool counts, which is enough for the
condensed transcript and statistics.

For an explicitly continued Codex history, pass ordered `--continuation-session` values as shown
above. Each successor must be a root session. The original session's normalized IDs and existing
summary cache inputs stay unchanged; common per-session event, turn, and tool IDs in successor
sessions are namespaced. The browser shows one structural continuation edge rather than pretending
that the new coordinator was a spawned worker.

Every ingest first copies the vendor logs it is about to parse into a snapshot store *outside*
`--output`, defaulting to `<output>.sources/`. Those copies are ingest input rather than published
output and are usually the largest thing here; see
[Where the source snapshots live](#where-the-source-snapshots-live) for the `--snapshot-root` flag
and for how an existing archive moves them.

Use `ingest-claude --session-file FILE` for a Claude lineage. Like Codex ingestion, it is offline and
does not invoke a model.

The Orc equivalent is `ingest-orc --source-root ROOT --root-session UUID`. It opens source
databases read-only, uses SQLite's online backup API for consistent copies, and parses only those
archive-local snapshots. Coordinator content blocks provide prompts, responses, and condensed tool
counts; agent blocks and task notes provide incarnation lifetimes and timestamped work updates.

### 2. Extract prompts and responses — no model calls

After one or more teams have been ingested into the same durable archive, materialize the global
chronological transcript projection:

```bash
wrkviz extract-transcripts --output ./timelines/example-team
# repeat --team to select teams explicitly; omission selects every ingested team
```

This reads only normalized coordinator events. It writes
`extracted/transcripts/occurrences.jsonl`, `prompts.jsonl`, `messages.jsonl`,
`system-inputs.jsonl`, and a digest-bound manifest. `occurrences.jsonl` retains every physical
provider/team occurrence, while
`prompts.jsonl` collapses only source-identical logical fork copies and contains verbatim authored
or externally submitted prompts;
`messages.jsonl` adds coordinator responses associated by provider turn identity. Synthetic Claude
notifications, scheduled inputs, and inter-agent messages are retained separately rather than
counted as the user's prompts. Every record has a stable occurrence ID and source provenance.

Reruns take a monotonic union: a provider-side rewrite or rotation cannot delete an occurrence that
was already extracted. Prompt ordinals are a convenient current 1-based chronological projection,
not durable identity; adding newly discovered older history can renumber later prompts. The run
receipt records zero model calls and zero model tokens.

A team that cannot be loaded is carried forward and reported rather than ending the run, and the
command exits 2 — see "A team the projection could not read is carried, not fatal" above. Naming a
team with `--team` still cannot *narrow* the projection: leaving out a team the projection already
covers is refused, because that is a decision rather than a fault.

### 3. Summarize — the only token-spending stage

```bash
wrkviz summarize \
  --team example-team --output ./timelines/example-team \
  --backend codex --model gpt-5.5 --reasoning-effort medium \
  --service-tier priority
```

The CLI defaults to `gpt-5.5` at `medium` reasoning effort. A provider or model failure stops the
run and leaves its failure receipt; it never silently changes models or switches to the heuristic
backend. Use `--model` or the
`WRKVIZ_MODEL` environment variable only when intentionally overriding that policy.

The summary provider is independent of the transcript source provider. To summarize any normalized
team through the installed Claude Code CLI, run:

```bash
wrkviz summarize \
  --team example-team --output ./timelines/example-team \
  --backend claude --model sonnet --reasoning-effort medium
```

The Claude child runs in print-only safe mode with all tools disabled, no session persistence, and
the same strict JSON schema used by the Codex backend. Only `structured_output` is accepted; a
plain `result` string is never treated as a schema response. Any process, provider, schema, or
usage-accounting failure aborts without selecting a different model or backend. `--claude-command`
can select a wrapper or alternate executable; it does not change the cache identity.

Each stable time window gets a content-addressed cache key over:

- its transcript input;
- the substantial ancestor/coordinator scroll-back window;
- supported semantic concepts available by that point in history (currently empty);
- model, reasoning effort, optional service tier, backend, prompt version, and summary schema.

Codex's catalog label **Fast** maps to the canonical service-tier value `priority`. Passing
`--service-tier priority` adds `-c service_tier="priority"` to every Codex summary and hindsight
naming invocation. Omitting the flag and passing `--service-tier default` are the same canonical
choice: every Codex child receives `-c service_tier="default"`, while its cache hash remains
compatible with summaries generated before tier support. Priority has a distinct summary and
hindsight-name cache identity. The effective value appears in immutable batch, invocation, and
top-level run receipts. A service tier is rejected with the deterministic heuristic backend.
Claude does not expose this Codex scheduling option, so it also rejects `--service-tier`.

Every calendar period has two distinct jobs and cache identities. The technical summary remains
content-led and must explain what a pull request, task, or phase changed instead of using its number
as an opaque referent. The plain-language summary is separately generated for a reader unfamiliar
with the project: it introduces the product from supplied evidence, explains specialized terms,
and treats work-management identifiers as supplementary evidence. Both jobs have independent batch
receipts and are included in the command's exact token accounting.

Before those calendar jobs, one separately cached knowledge pass reads up to 48,000 characters of
early root conversation and creates a durable newcomer project overview. Its first successful run
records an immutable source cutoff plus event IDs and a context digest, without copying the raw
48,000-character transcript into version-controlled `summary_data/`. It excludes the half-open
archive end boundary. Mutation or truncation of its retained evidence fails before any model call,
while ordinary events appended beyond the cutoff reuse the exact overview cache key.

The retired glossary-definition pass is disabled because deterministic candidate strings were too
broad to serve as a project ontology. New runs do not inject them into phase or rollup prompts,
do not launch definition calls, and do not rewrite `summary_data/glossary.json`. Old definition
caches, projections, and usage receipts remain on disk for audit and accounting. Builds ignore the
retired projection and publish no glossary links until an evidence-bounded semantic discovery pass
is implemented.

Audit those immutable records without taking the archive writer lock, changing a byte, or calling a
model:

```bash
wrkviz audit-glossary --output ./timelines/example-team
wrkviz audit-glossary --output ./timelines/example-team \
  --team example-team --format json --details
```

The audit deterministically rejects known transcript-markup fields, durations and command examples,
opaque record identifiers, code/configuration literals, ordinary language, and transient process
actions. Plausible projects, systems, workstreams/tasks, and milestones are reported as
`semantic-review-required`, never promoted. This distinction is deliberate: a cached definition can
show that a string is explainable, but schema 3 did not ask what semantic kind it has. A future
versioned discovery contract must classify and evidence those concepts before they can enter summary
prompts or become site-wide links. If an older generated site still shows retired glossary links,
run the zero-token `build` or `export` again; `summary_data/glossary.json` itself remains unchanged.

Unchanged keys are never sent to the model again. A changed live window creates a new cache record while the previous valuable record
remains on disk. Each batch is committed only after every response in that batch validates against
the strict JSON schema; a failed batch cannot corrupt existing cache data, while other validated
batches remain reusable.

Phase work bullets use half-open time bounds: the start timestamp is valid and the end timestamp is
not. A model response that is otherwise completely valid may contain out-of-range phase bullets;
the pipeline drops those bullets without shifting or inventing timestamps. It then derives the
phrase and paragraph only from remaining valid bullet text, or uses the existing transcript-only
fallback when every bullet was rejected. A genuinely empty model bullet list keeps the model's
prose. This exception is phase-only: malformed fields and out-of-range calendar-rollup bullets
still fail validation, and it does not alter the prompt version or cache key.

After phase summaries exist, a separate hindsight pass names every agent and writes its lifetime
paragraph in the same response. It sees the agent's
official path, coordinator nickname and role, arbitrary-depth parent path, cross-spawn ancestor
context, and the complete set of phase summaries describing what the agent ultimately did. This
means a reused or misleading coordinator name does not become the permanent UI label. Naming has a
separate content-addressed cache, so only an agent whose summarized work or context changed is sent
back to the model. Archives created before lifetime summaries incur one naming-only cache miss per
agent (batched normally); phase and calendar summaries remain cache hits. Later unchanged runs are
all-hit again. The whole summarize transaction holds the archive writer lock, preventing two
simultaneous refreshes from buying the same cache miss.

Every backend batch writes an immutable usage receipt with its model, reasoning effort, service
tier, input, cached-input, cache-write-input, output, reasoning-output, and total token counts.
For Claude, normalized input is the sum of its direct input, cache-creation input, and cache-read
input counters; cache creation and reads are also retained in their dedicated fields. Claude does
not expose a separate reasoning-token count, so that field is zero rather than guessed.
Cache records link to that receipt. The command prints both tokens newly spent by this run and the
deduplicated original generation cost of all returned artifacts; an all-hit rerun therefore
reports zero new tokens without losing the original cost. Older cache entries remain valid and are
reported explicitly as having unknown original usage rather than being regenerated or counted as zero.
Failed backend final messages and timestamp-repaired phase responses are retained beside those
receipts under `_usage/backend_outputs/<receipt-id>.json`, with a SHA-256 hash, job bounds, and each
rejected bullet's index, timestamp, and action. These audit records store neither the prompt nor
captured Codex CLI stdout/stderr. A nonzero-exit receipt and displayed failure retain the safe exit
code but omit captured stream detail.

For compatibility, the cache reader accepts a paid artifact whose last bullet is exactly at its end
boundary. That artifact stays a cache hit and its bytes remain unchanged; newly generated
responses and deterministic fallbacks remain half-open.

Summary selection is independent from ingestion. To backfill one exact hour while retaining the
archive's complete normalized source, run:

```bash
wrkviz summarize \
  --team example-team --output ./timelines/example-team \
  --summary-start-time 2026-08-07T02:00:00Z \
  --summary-end-time 2026-08-07T03:00:00Z \
  --rollup-kind hourly --model gpt-5.5 --reasoning-effort medium
```

Repeat `--rollup-kind` to request several levels. Omitting it retains the daily, weekly, monthly,
and quarterly defaults. Each generated artifact records whether it begins the project, extends a
contiguous same-level frontier, or is an isolated backfill, plus a component-level context score.
`summary_data/artifacts.json` retains every indexed model/version variant by logical key.
Aggregate accounting and receipt paths are also retained in the top-level `runs/*.json` metadata.
From the archive directory, `make run-stats` formats those top-level records as an oldest-to-newest
run history, including cache hits/misses, generated products, build counts, newly spent tokens, and
returned-artifact provenance costs. Backend receipts are attributed only when the team matches and
one top-level summarize/refresh command window contains their timestamps, so a failed run still
shows its completed and failed batches and their known token subtotal. Concurrent top-level
windows can overlap while waiting for the summary lock; ambiguous receipts remain unattributed
rather than being guessed. Receipt schemas and content hashes are validated before a record enters
the ledger. If even one attributed receipt lacks usage, the actual total is labeled `UNKNOWN`
rather than presenting that subtotal as a zero or complete cost.
The report separately scans the immutable backend receipt ledger and groups actual attempts by
backend, model, reasoning effort, and service tier. This archive-wide ledger includes a failed
attempt when the backend returned usage before failing; any usage-less receipt remains
explicitly unknown. Returned-artifact generation cost is never added to new spend.

For offline development or tests:

```bash
wrkviz summarize \
  --team example-team --output ./timelines/example-team \
  --backend heuristic --model deterministic-local
```

The heuristic backend is intentionally less capable, but exercises the complete cache and rendering
pipeline without network or token use. It labels the overview and definitions as insufficient
evidence and retains first-use context instead of pretending to synthesize model-quality
explanations. Its cache keys are distinct from Codex and Claude summaries.

### 4. Build — guaranteed zero model calls

```bash
wrkviz build --team example-team --output ./timelines/example-team
```

`build` reads normalized JSON and any structured summary data, then regenerates the detail JSON and
website plus Markdown only for summaries that actually exist. Identical bytes are not rewritten.
Use it freely after CSS, layout, or Markdown formatting changes. Because those presentation files
live at the output root, the tool refuses to use a non-empty directory unless it already contains
its archive marker or a recognized existing `teams/<slug>/raw/team.json`; a mistaken `--output`
cannot replace another project's README or Makefile.

Build fails closed on definition-only glossary projections: it ignores those retired records and
omits them from links and rendered catalogs. This removes stale glossary links and weekly generated
files without rewriting or deleting original definition caches, projections, artifact catalogs, or
usage receipts. `audit-glossary` is the read-only diagnostic for those retained records; it is not a
migration command and its semantic-review list is not linkable data.

To create a separate zero-token website package for a selected interval, use `export` after the
needed summaries are cached:

```bash
wrkviz export \
  --archive ./timelines/example-team --output ./exports/overnight-hour \
  --team example-team \
  --start-time 2026-08-07T02:00:00Z --end-time 2026-08-07T03:00:00Z \
  --rollup-kind hourly
```

The export has its own archive marker, run receipt, `./timeline` query CLI, and local server launcher. It reads
the durable archive but does not copy or truncate normalized source data and cannot invoke a model.
If a selected phase or rollup has no cached summary, its projection carries an explicit false
availability flag and still exposes normalized transcript and statistics. It does not masquerade as
a Markdown summary or appear in the summary-file menu. These build-only placeholders never enter
the model cache; rerunning `summarize` later replaces them with paid summaries without discarding
already compatible cached work.

Repeat `--team` to combine providers or machines in one aligned site. Every team receives the
same half-open interval and rollup selection:

```bash
wrkviz export \
  --archive ~/agent_logs_archive/summary/example-project \
  --output ./exports/example-project-overnight \
  --team codex-project --team claude-project --team orc-project \
  --start-time 2026-08-06T22:00:00-04:00 \
  --end-time 2026-08-07T07:00:00-04:00 \
  --rollup-kind hourly --timezone America/New_York
```

Combined exports namespace provider-owned identifiers and phase-detail paths by team, merge
project and host identity, and preserve team labels for filters, rollups, summary files, events,
and statistics. If archived teams disagree about their display timezone, pass one explicit
`--timezone` for date-bound parsing and the shared axis. Existing daily and higher rollups retain
each archive's calendar timezone rather than being relabeled as a different cached computation;
hourly rollup keys are UTC-stable.

### 5. Optional GitHub pull metadata — conditional and cached

```bash
wrkviz github-metadata \
  --team example-team --output ./timelines/example-team
```

This scans only evidenced pull links, conditionally fetches their title, state, branches, author,
and a bounded body excerpt, then rebuilds the detail JSON. Public repositories need no token. For
private repositories, set `GH_TOKEN` or `GITHUB_TOKEN`, or name another environment variable with
`--github-token-env`; credentials are request-only and never enter the cache. ETags make unchanged
reruns byte-stable. `refresh --github-metadata` performs the same optional step after its normal
build. Successful records are saved individually, so a rate limit or unavailable repository cannot
discard metadata already fetched.

## Summary hierarchy

The pipeline performs real multilevel reduction:

```text
verbatim messages + condensed tools
     -> fixed, append-stable agent phases
     -> durable project overview
     -> daily technical + plain-language summaries
        -> weekly technical + plain-language summaries
           -> monthly technical + plain-language summaries
              -> quarterly technical + plain-language summaries
```

Each level keeps a phrase, a paragraph, and timestamped substantive work bullets. Calendar
boundaries are computed in `--timezone`, including daylight-saving transitions. UTC instants remain
canonical in JSON; the chosen IANA timezone controls labels and day/week membership. That choice is
stored in `raw/site-identity.json` with its provenance and is used by the browser independently of
the browser or web-server machine's local timezone. Archives without that file retain the
timezone in normalized team data; malformed or missing browser data falls back visibly to UTC,
never silently to ambient browser time.

The retired deterministic terminology scan remains as migration/test code only. It never supplies
new prompt context or creates model work. A future replacement must classify bounded chronological
evidence into projects, systems, subsystems, sustained workstreams, named tasks, and milestones,
and must establish each concept's availability before it can enter later prompts or links.

## On-disk format

An archive is ordinary text designed for version control:

```text
example-team/
├── .agent-team-timeline.json
├── index.html, timeline-core.js, app.js, style.css
├── vendor/markdown-it-15.0.0.min.js  # pinned offline Markdown renderer + license
├── timeline, Makefile, serve.py, run_stats.py, README.md
├── manifest.json
├── runs/<timestamp>-<hash>.json
├── extracted/transcripts/
│   ├── manifest.json                # source generations + file digests; zero-token contract
│   ├── occurrences.jsonl            # every physical provider/team source occurrence
│   ├── prompts.jsonl                # global chronological authored prompt report
│   ├── messages.jsonl               # prompts plus mechanically linked responses
│   └── system-inputs.jsonl          # retained scheduled/synthetic coordinator inputs
├── data/
│   ├── timeline-v3.json             # the only bootstrap a build writes; shards under timeline-v3/
│   │                                #   read by `./timeline` and by the browser alike
│   │                                #   timeline-v3/search{,-bloom,-links}/ is transcript search
│   └── details/<phase-id>.json[.gz]
├── snapshot-root.json               # where this archive keeps its vendor source snapshots
└── teams/example-team/
    ├── payloads/                      # gitignored tool arguments and command output
    │   ├── <xx>.jsonl                 # content-addressed, sharded by digest prefix
    │   └── manifest.json              # records, bytes, and each shard's content address
    ├── raw/
    │   ├── team.json
    │   ├── task-notes.jsonl          # verbatim task notes, one per line; append-only
    │   ├── site-identity.json        # projects, repositories, hosts, archive timezone
    │   ├── source-manifest.json      # versioned path/byte/hash/update provenance
    │   ├── normalized-generation.json # Orc manifest/team/catalog commit marker
    │   └── source-snapshot.json
    ├── summary_data/
    │   ├── artifacts.json             # logical-key/version/model/context catalog
    │   ├── cache/<content-hash>.json
    │   ├── cache/_usage/backend_outputs/<receipt-id>.json # failed/repaired raw output audit
    │   ├── name_cache/<content-hash>.json
    │   ├── agents/<thread-id>.json   # hindsight name, lifetime summary + provenance
    │   ├── phases/<phase-id>.json
    │   ├── project_overview.json     # immutable evidence epoch + generated overview
    │   ├── rollups/{hourly,daily,weekly,monthly,quarterly}/... # both audiences
    │   ├── github/pulls.json         # ETag-backed bounded PR title/hover metadata
    │   └── glossary.json             # optional retired definition projection; audit-only
    └── summaries/
        ├── agents/<thread-id>.md
        ├── phases/<phase-id>.md
        ├── hourly/YYYY-MM-DD/YYYY-MM-DDTHHZ-<team>-hourly.md
        ├── daily/<ISO-week>/YYYY-MM-DD-<team>-daily.md
        ├── daily/<ISO-week>/YYYY-MM-DD-<team>-daily-plain-language.md
        ├── weekly/<year>/YYYY-Www-<team>-weekly.md
        ├── monthly/<year>/YYYY-MM-<team>-monthly.md
        ├── quarterly/<year>/YYYY-Qn-<team>-quarterly.md
        ├── glossary/<year>/YYYY-Www-<team>-glossary.md
        └── glossary/<team>-glossary.md             # all-time catalog
```

`summary_data/cache/` is valuable generated data: keep it under version control if the archive is
versioned. It is what prevents a future refresh from buying the same summary twice. Run receipts are
append-only and make the last attempted/completed refresh, model work, cache hits, source size, and
source digest auditable.

`raw/task-notes.jsonl` is the archive's own copy of every task note the lineage has ever shown it,
one canonical JSON object per line, sorted and keyed on the note's source database and id. The same
notes also appear in `team.json` as the messages they render into, but that rendering packs the task
id and title into a text prefix and omits notes outside the ingest window; this file keeps each
field separately, keeps every note, and records where the text came from and whether it can still be
re-read upstream. Task trackers delete notes, and when one is deleted this file is the only copy
left — so it is append-only by construction. An ingest adds notes it has not seen before and touches
nothing else, which is why a repeat ingest changes no bytes at all, and it refuses rather than
merges if a stored note and the live source disagree about a note's task, text, or creation time.

`payloads/` is where a tool call's arguments and its command output now live. Earlier versions kept
neither: ingest set both fields to `null` and left the vendor log as the only copy, so an archive
could tell you that a command ran and not what it printed. Each payload is stored once under the
SHA-256 of its own text, in the shard named by the first byte of that digest, and `team.json` keeps
only the digest and the byte count. That split is deliberate and worth understanding before you
decide how to back the directory up. Command output is the biggest and the most sensitive thing an
archive holds — file contents, absolute paths, whatever a log line happened to include — so it is
gitignored alongside the source snapshots, while the model that points at it stays small and
version-controlled. Prune, encrypt or relocate `payloads/` and the archive still says precisely
which text is missing and how large it was, which is more than it could say before.

Before you delete anything, ask the archive what it would lose:

```bash
wrkviz audit-losslessness --output ./timelines/example-team
wrkviz audit-losslessness --output ./timelines/example-team \
  --team example-team --format json --require-lossless
```

This re-reads every row of the vendor snapshots and requires each one to fall under a rule that
says what became of it, then checks that claim: that an event really is recorded at that line, that
a stored payload really is byte-identical to the output the row carries. A row shape no rule covers
— a new record type in a newer Codex — exits 1 rather than passing quietly, which is the point of
running it at all. Rows whose content the archive genuinely does not keep are listed with their
sizes and the reason; that inventory is what `--require-lossless` refuses on, and it is the check
to put in front of an actual `rm -rf` of `source_snapshots/`. Only Codex teams are covered today;
Claude and Orc teams are reported as uncovered by name, and an archive containing one is never
reported lossless.

The audit is defined against `raw/source-manifest.json`, not against whatever files happen to be in
the directory, because an audit whose subject is "what survived" cannot detect loss — delete a
rollout and its rows simply stop being counted, and the report gets *greener* the more you remove.
A recorded rollout that is gone, shorter than its record, or no longer hashing to it fails the gate.

**Deleting `source_snapshots/` makes the team read-only, permanently.** Reading it afterwards is
fine and is the promise the audit is about: `team.json`, `raw/task-notes.jsonl` and `payloads/` are
all that a build, a query or a summary needs. But every ingest path still reads the vendor tree to
establish append-only continuity with the last run, so the next `ingest` for that team fails: Codex
reports a missing source snapshot for a rollout its manifest records, and Orc reports a snapshot
that does not match its manifest. Delete the snapshots for a lineage that is finished, not for one
that is still running, and understand that the decision is not reversible by re-ingesting.

Ingest first copies every newline-complete rollout in the selected lineage into the snapshot
store described in [Where the source snapshots live](#where-the-source-snapshots-live); all parsing
after that point uses these copies rather than the live Codex files, while
`raw/source-manifest.json` remains versionable and records each original path, copied byte count,
SHA-256 digest, line count, and last snapshot update. Reruns permit only exact reuse, an append to an
existing prefix, or a newly discovered child rollout. A disappeared file, shorter complete prefix,
or rewritten prefix fails the ingest before replacing any prior snapshot. An incomplete trailing
JSONL line is left for the next run.

For Codex histories with explicit successor roots, the same schema-1 manifest has an optional
ordered `continuation_sessions` array. Each entry retains the predecessor/successor IDs and source
paths, exact predecessor source line and UTC timestamp, successor start, and millisecond gap. Old
manifests without this field remain valid single-session archives. Once written, the evidence is
reused rather than recomputed as source files continue to append.

The snapshot, manifest, and normalized `team.json` update run under an archive-local Linux file
lock, so concurrent refreshes cannot publish an older validated copy over a newer one. Parsing is
restricted to the exact paths in that refresh's validated source set; leftover files from an
interrupted run are not silently adopted. Snapshot traversal and replacement reject symlinked
roots, directories, and targets, and durable replacements fsync both file content and its parent
directory before the transaction proceeds.

### Where the source snapshots live

The vendor copies are ingest **input**, not published output. They are typically the largest thing
an archive touches — on one measured archive, 995 files and 6,524,876,923 bytes against a
9,662,206,589-byte archive, 67.5% of it — and nothing a reader, a query or the website ever opens.
So they are kept outside `--output`:

    /path/to/summary/widget            <- --output: published, served, cloned
    /path/to/summary/widget.sources/   <- the snapshot store
    /path/to/summary/widget.sources/<team-slug>/...

Set it with `--snapshot-root DIR` on any ingest or refresh command, or with a top-level
`"snapshot_root"` in a project config. It names the *store*, not one team's directory: teams are
slug-named subdirectories inside it, so an archive with twelve teams sets it once. The archive
records the choice in `snapshot-root.json` at its root, so later runs do not need the flag —
without that record, forgetting the flag once would silently start a second store beside the
first, and both would look valid.

The store keeps a `.gitignore` containing `/*`, which ignores every entry including itself, so a
store that lands inside a repository is invisible to `git status` rather than being offered as
several gigabytes of additions.

**An existing archive is not moved by a build.** If `teams/<team>/source_snapshots/` is populated,
every ingest keeps using it exactly as before, indefinitely, with no flag. Moving it is something
you ask for:

```bash
wrkviz migrate-snapshots --output ./timelines/example-team
wrkviz migrate-snapshots --output ./timelines/example-team --move
```

The first prints what would move, from where, to where, how many files and bytes, and anything it
refuses to touch. The second does it — a rename per team when the store is on the same filesystem,
which the default sibling always is. It refuses rather than merging into an occupied target, and it
refuses a cross-filesystem move unless you add `--copy`, because that path is a copy-verify-delete:
interruptible, and the bytes exist twice while it runs.

The layout is recorded *before* the first byte moves, so an interrupted migration is resumable on
both paths: the next ingest recognises the half-moved state by name and refuses for the teams
still inside the archive — the ones already moved keep ingesting normally — and re-running
`migrate-snapshots --move` finishes it. That holds for `--copy` too, because the copy assembles
each team under `<store>/.migrating/<team>` and publishes it with a rename, so a target directory
never exists in a half-written state and an interruption leaves nothing to disentangle: re-running
discards the scratch copy and starts that team again. `wrkviz inspect` prints the
current layout under `source_snapshots`, including `store_root`.

Two things the resolver refuses rather than guessing at, both of which used to end badly:

- **`--snapshot-root` while any team's snapshots are still inside the archive.** One archive has
  one layout, so the flag is refused for a *new* team as well, naming the directories that are
  still in the old place. Accepting it would have split the archive across two layouts and then
  recorded the new one archive-wide, after which every already-ingested team refuses.
- **A recorded store that is not there.** If the archive and its store were renamed together, the
  sibling `<output>.sources` is adopted — it carries a marker saying which archive it belongs to,
  so that is a reading and not a guess. Otherwise the run stops and says so, rather than creating
  an empty store and losing sight of the snapshots that exist somewhere else. Renaming the archive
  directory alone is fine and always was; the store's marker is updated to the new name.

**Nothing under the snapshot root is reachable over HTTP.** For a migrated archive that is true by
construction — the files are not under the served root. For an archive that has not been migrated,
`make serve` refuses any path with a `source_snapshots` component with a 404, so the bytes are not
fetchable even while they are still physically inside the tree. (An archive built by an older tool
carries that tool's `serve.py`; rebuild it to pick up the refusal.)

Back the store up with the archive if you want incremental ingest to keep working. Without it the
archive still reads, queries, summarizes and builds — see the paragraph above on what deleting the
snapshots costs — but the next ingest for that team cannot establish append-only continuity.

### The retired `raw/messages/` directory

Older archives contain `teams/<team>/raw/messages/<thread-id>.json`, one file per agent thread. It
was a re-serialization of the records already in `raw/team.json`, split by thread, and nothing ever
read it — not the website, not `./timeline`, not `serve.py`, not any earlier version of any of them.
It was also large: on one twelve-team archive it was 675,371,783 bytes across 2,932 files — 50.9% of
everything under `teams/` that is not a gitignored source snapshot, so it roughly doubled the size
of the version-controlled archive — and every ingest rewrote all of it.

The tool no longer writes it, and the next ingest of a team removes it. That happens once; later
runs find nothing and say nothing. When it happens you get a line on stderr naming the team, the
file count, and the megabytes reclaimed, and the same counts land in the run receipt under
`ingest.retired_message_projections` and `ingest.retired_message_projection_bytes`, so the deletion
is still explicable months later from the archive alone.

Nothing is lost. Every record in those files is in `raw/team.json`, which is written and flushed
before the sweep begins, and if your archive is versioned the deleted bytes remain in its history —
expect a large deletion in `git status` after the first ingest.

The sweep removes regular files directly inside that directory named `<thread-id>.json`, matching
the *shape* of a thread id rather than only the threads in your current `raw/team.json`. That is
deliberate: the old writer never cleaned up after itself, so a lineage re-ingested through a
narrower `--team` selection or date window left orphaned projections behind, and those are the ones
most worth reclaiming. The cost is that `raw/messages/` is an ingest-owned directory and is swept
as one — do not keep your own files in it, because anything you leave there named `something.json`
goes too. A symlink, a subdirectory, a `.gz` sidecar, and any other name are left alone, and
anything left behind keeps the directory itself in place.

### The retired schema-1 monolith, and `gc`

`data/timeline.json` was the whole presentation timeline in one JSON document — 246,973,399
bytes, plus a 33,997,790-byte gzip twin, on one twelve-team archive. `data/timeline-v2/` was the
generation after it, 1,503,831,881 bytes across 661 content-addressed objects. **A published build
now writes neither.** Both readers reach them only as fallbacks: `./timeline` and the browser both
try schema 3 first, then schema 2, then the monolith, and every build writes a complete schema 3,
so nothing an up-to-date tool produces needs either of them.

**A rebuild does not delete the copies you already have.** Not writing a file and deleting it are
separate decisions, and only the first one belongs to a build you ran for another reason. Both
older generations are recorded in `data/export.json` under `retired_files` and left exactly where
they are, so an older `./timeline`, or the archive's own older `app.js`, still finds what it looks
for. That last one is not hypothetical: the browser bundle is *copied into* the archive by the
build that made it, so an archive you built before this change has a bundle that has never heard of
schema 3, and it will keep that bundle until you rebuild.

That is also the second condition on reclaiming the monolith itself, alongside a complete
`data/timeline-v3.json`: if your archive's own `app.js` has no schema-3 mode **and** there is no
`data/timeline-v2.json` beside it, `data/timeline.json` is the last thing that bundle can open, and
`gc` holds it. Rebuild and it goes.

Reclaiming either is `gc`, and `gc` defaults to telling you rather than doing anything:

```bash
wrkviz gc --output .            # dry run: every category, by bytes
wrkviz gc --output . --delete   # move the reclaimable ones to the trash
wrkviz gc --output . --empty-trash   # the irreversible pass
```

The dry run classifies every file in the archive as live, reclaimable or held, and prints the
reason for each category even when it is empty — a zero that does not explain itself is not an
answer. Nothing is ever reclaimed because "the last build did not write it": a file goes only
when a named superseding artifact is present *and complete* — schema 3's completeness is judged
by the same five-clause rule the reader uses before it will answer a question — or when a
manifest authoritative over its directory disowns it.

`--delete` moves files into `.agent-team-timeline-trash/<generation>/files/`, mirroring their
archive paths, and writes a `receipt.json` beside them saying what went and why. The receipt is
written before the first file moves and rewritten after the last one, gaining `"complete": true`,
so a sweep that fails partway still leaves a list of what may be in the trash rather than an
unexplained directory. Putting it back is one copy from the archive root:

```bash
cp -a .agent-team-timeline-trash/<generation>/files/. ./
```

`--empty-trash` is a separate invocation, and asking for both at once is refused. That is the
whole design: this archive costs hours of model time to rebuild, so the first destructive
operation is reversible and the second one is a decision you have to type out on purpose.

### The schema-2 generation, and the last fallback

`data/timeline-v2/` is the other whole generation an archive carries, and it is the big one: on a
measured archive, 1,434.2 MiB in 658 objects beside a 5.4 MiB bootstrap, against 37 MiB for all of
schema 3. `gc` splits it in two and only ever offers one of the halves.

The half it may not offer is `schema-2-search-corpus` — 509.1 MiB of it, 35%. The corpus is a set
of content-addressed day shards with trigram blooms, catalogued in the `search` section of
`data/timeline-v2.json`, and for a long time it was the *only* copy: `timeline search` and
`timeline show` read it out of there even on an archive whose schema 3 was perfect. Reclaiming it
then would not have made anything slower, it would have deleted those two commands.

Schema 3 now publishes a corpus of its own, in three more streams — the records under
`data/timeline-v3/search/`, the trigram prefilter under `search-bloom/`, the prompt/response
relationships under `search-links/` — and on the measured archive that is 67.0 MiB against 509.1,
answering the same queries with byte-identical results while reading 6.4 times fewer bytes. So the
question `gc` asks is **about your archive, not about your tool**: does the `data/timeline-v3.json`
here name `search` shards. If it does, these objects are a duplicate like everything else in the
generation and go with it. If it does not — which is every archive built before the streams
existed — they are held, in every world including the one where the presentation format is
retired, because that flip is about the timeline and not about search. One rebuild, which costs no
tokens, is what moves an archive from the second case to the first.

The half it does offer is `superseded-schema-2`, the presentation objects and the manifest, and it
holds those under **four** preconditions rather than the monolith's two.

The first is the same: a `data/timeline-v3.json` that passes the reader's own five-clause
completeness rule. The second is that **this tool no longer writes schema 2 at all** — and that is
read out of the code, from `timeline_shards.SCHEMA_2_IS_PUBLISHED`, never out of the archive.
Modification times cannot answer it and will actively mislead you: schema-2 objects are content
addressed and the bootstrap is only rewritten when its bytes change, so a build that republishes an
identical schema 2 touches nothing. On the measured archive the newest schema-2 object was thirteen
hours *older* than the schema-3 bootstrap while that same build's `data/export.json` listed all 661
schema-2 files as its own output. "The last build did not write it" is not evidence, here or
anywhere else in `gc`; "no build can write it" is, and the writer enforces it by refusing to run
while that constant is false.

**That constant is now false**, so this precondition is satisfied for every archive. It was true
for a long time because the website was the reason schema 2 existed: `static/app.js` loaded
`data/timeline-v2.json` and had no schema-3 mode, so retiring the format would have retired the
graphical surface. The bundle now loads `data/timeline-v3.json` and reads its shards a gzip member
at a time over HTTP Range, which is what let the writer stop.

The third and fourth can be satisfied and unsatisfied by an ordinary accident, so they are worth
knowing about before you meet one.

The third is that the schema 3 in front of schema 2 has to be a **successor to it** rather than a
bystander from another build. Completeness cannot tell the difference: all five of its clauses
compare schema 3 against itself. A build publishes schema 2 and then schema 3, so one that dies in
between — OOM, a full disk — leaves a new schema 2 beside an untouched old schema 3 that is
flawless and a team short. `gc` compares the two `source_digest` values, which is the same
comparison the reader refuses on when a search makes it open both generations, and holds
everything if they disagree. The remedy is to re-run the build.

The fourth is your own copy of `app.js`, and it is the one that will actually stop you. A build
*copies* the browser bundle into the archive it builds; `gc` does not rewrite it, and should not.
So an archive built before this change carries a bundle with no schema-3 mode, and sweeping without
rebuilding — the whole convenience `gc` sells — would leave `index.html` fetching a file that is now
in the trash and falling back to another one that is also in the trash. `gc` therefore asks whether
your archive's `app.js` mentions `data/timeline-v3.json` and holds the generation until it does.
One rebuild fixes it, per archive: a tool upgrade cannot reach into an archive you built last month,
which is exactly why this gate is per archive and permanent rather than a step in a migration.

**When all four are satisfied, read the reason before you type `--delete`.** Reclaiming schema 2's
presentation half removes the last fallback. The reader tries schema 3, then schema 2, then
`data/timeline.json`, and the monolith is offered for collection under conditions that are already
satisfied whenever these are — so one `--delete` can take everything below schema 3 in a single
pass. After that, a schema-3 generation the reader *declines* is not a slow archive but an
unreadable one, and it declines a whole generation over a single shard-shaped file under
`data/timeline-v3/` that the catalogue does not name, which is exactly what an interrupted build
leaves behind. The way back in the next ten minutes is the trash, until `gc --empty-trash`; after
that it is a rebuild, and **check where that rebuild would have to run**. An ingest archive holds
`teams/*/raw` and rebuilds from itself for no tokens. A combined export does not — it carries
`teams/<slug>/summaries/` and nothing else — so its raw turns are back in the ingest archive it
came from, which may be on another machine. `gc` reads the difference off the archive and says
which case you are in, in the reason itself.

Four things `gc` reports and deliberately never touches.

The transcript search corpus, above, in an archive whose own `data/timeline-v3.json` publishes no
`search` streams: the schema-2 bootstrap and the objects its `search` section names. It is held for
a reason no release can change on your behalf, because the successor has to be *in this archive*
and only a rebuild puts it there — the same shape as the `app.js` precondition below.

The schema-2 objects listed as `retained_objects` are the previous generation's, kept alive for
one more generation so that a browser which already loaded the previous bootstrap can still fetch
what it names; the next build that supersedes them reclaims them. That grace lapses with the format
itself: once nothing writes schema 2, no browser is holding a bootstrap to protect, and these stop
being a category of their own and go with the rest of the presentation half.

`teams/*/source_snapshots/`, usually the largest thing in the archive, is vendor input rather than
published output and has a purpose-built gate of its own: run `audit-losslessness
--require-lossless` and act on that. To get those bytes out of the published tree without deleting
anything, run `migrate-snapshots` instead; after that this category is empty, because `gc`
classifies the directory it was given and there is nothing of the kind left in it.

And **schema-3 shards the bootstrap does not name**, which are reported and never swept. Absence
from the catalogue cannot distinguish a retired team's leftover shard from a live team's shard
that an interrupted build published before it got to its bootstrap — the answer depends only on
which of the two is newer, and nothing on disk says. The publisher settles it instead: a build
clears `data/timeline-v3/` of everything outside its own plan once its bootstrap is written, so a
completed build always leaves this category empty. If it is *not* empty, a build was interrupted;
the archive is meanwhile reading through the slower generation behind schema 3, and the fix is to
re-run the build, which clears the residue. `gc` says exactly that in the category's reason.

## Claude source semantics and limitations

Claude root and subagent JSONL files remain on disk across context compaction. The importer copies
their newline-complete prefixes plus immutable subagent metadata sidecars before parsing. Reruns
accept unchanged files, monotonic JSONL appends, and newly discovered descendants; disappearance,
truncation, prefix rewrites, or metadata changes fail without replacing the prior validated copy.

Subagent metadata supplies spawn depth, parent agent ID, role, description, and the spawning Agent
tool-use ID. That supports nested lineages and links the exact spawn prompt to its child. If metadata
is absent, the importer falls back to the root parent and the source agent ID rather than inventing
lineage. Turn boundaries are reconstructed heuristically from timestamped user messages because
Claude logs do not expose the same explicit turn lifecycle records as Codex. Text prompts in a
subagent log are counted as parent-to-child messages rather than human prompts, and a subagent's
final response is counted as a child-to-parent message while still driving its result edge.

## Orc source semantics and limitations

Orc's older `content_blocks` and current append-only `messages` table jointly describe
coordinator conversation and tool execution. The importer preserves existing content-block
identities, then unions in modern blocks absent from that table by stable `v2-block-<id>`
identity. Overlapping records must agree; both tables participate in monotonicity and semantic-cache
validation so changing either source cannot silently reuse a paid summary.
Conversation state supplies stable agent spawn records. Local task notes are mapped to the matching
owner incarnation available at that time. Reused names become separate incarnation IDs while the
official name remains visible. A local note with no owner is preserved beneath an `Unattributed
Task Work` worker. A note carrying a server author is shown as an external/server-authored message
on the coordinator, counted separately from user prompts, and does not invent an agent or edge.
Each note row appears once, including an empty-content row; notes are not labeled terminal results
because they can represent incremental progress.

The TaskGraph database set is reference-driven. A provider-initial session uses its named database,
or its session UUID when unnamed. Delegated sessions use their UUID even when a name was inherited;
`associated_dbs` adds any other referenced databases. References may precede file creation, so a
never-seen missing database is skipped. Once observed, a still-referenced disappearance fails
closed. A deliberately detached database remains frozen in the archive, and a replacement receives
a stable source ordinal so old event IDs and history remain intact. With an Orc session index, only
the selected subtree is inspected; without it, only the explicit root is inspected.

Orc may compact or rewrite its auxiliary conversation-state JSON even while content blocks keep
appending. TaskGraph may also hard-delete a task and cascade-delete its old notes. The archive keeps
a cumulative content-addressed note projection: immutable note identity/body fields and their first
observed author, owner, and title survive provider deletion, while genuinely new IDs above the
recorded note maximum and prior SQLite allocation sequence extend the history. Reused IDs, changed
note bodies, or a regressed sequence fail closed. Current presence/tombstone and never-captured-gap
hashes remain auditable without
silently changing old timeline events or paid-summary cache keys.

A conversation rewrite is accepted only when every recorded AgentBlock spawn identity and value
remains an unchanged subset; missing or modified spawn evidence still fails before replacing any
prior snapshot. Schema-v2 manifests record bounded message/spawn digests, cumulative rewrite
counts, and explicit degraded flags—never the raw conversation payload. Existing schema-v1 archives
validate their preserved byte baseline and retain their exact raw-byte summary-cache identity until
a real semantic change moves them to deterministic schema-v2 identity.

### Accepting an in-place append-prefix rewrite

Separately from that conversation projection, the archive holds each session's `content_blocks` and
`messages` rows to a byte-exact append-only contract at or below the watermark it last recorded.
That contract is occasionally broken by something harmless: an upstream backfill writing a
`token_count` into a row captured minutes earlier. No record is lost, but the digest moves and the
ingest refuses.

The refusal now says exactly what moved—which table, which row, which column, and which JSON field
inside that column, as a before/after excerpt anchored on the difference rather than truncated from
the start:

```
wrkviz: Orc session existing append prefix was rewritten for
  .orc/sessions/SESSION_UUID/session.db: 1 row(s) changed at or below the append watermark
  [messages row 878292: message_json .token_count: text:..."token_count":null} ->
  text:..."token_count":445}]; re-run with --accept-orc-prefix-rewrite SESSION_UUID to
  record this rewrite and re-baseline the digest -- that authorizes this one session and
  no other, so another rewritten session in this lineage refuses again with its own
  evidence -- accepting supersedes the pre-rewrite snapshot .objects/16/1617869e....db
  (kept for comparison until the next accepted override on this source, then reclaimed)
  and records only a bounded summary -- at most 20 of those 1 changed row(s), at most
  160 characters per column value
```

The second half of that message is the price, stated where the decision is made. Accepting is not
free and not fully reversible, so the refusal names the snapshot object it will stop trusting and
admits that the diff kept in exchange is a summary, not a transcript.

Read that first, decide, and only then override — with the session id the refusal printed:

```bash
wrkviz ingest-orc \
  --output ../summary/widget --team orc-example-014 \
  --source-root ../raw/devbig014 --root-session SESSION_UUID \
  --accept-orc-prefix-rewrite SESSION_UUID
```

**The flag authorizes one session, not the run.** A lineage is a session tree — a root coordinator
plus every nested session — and the guard runs once per session in it. If a second session was also
rewritten, this command refuses again, naming that session and printing *its* diff, and you repeat
the flag once you have read it:

```bash
wrkviz ingest-orc ... \
  --accept-orc-prefix-rewrite SESSION_UUID \
  --accept-orc-prefix-rewrite NESTED_SESSION_UUID
```

Two refusals for two rewrites is the intended cost. The alternative is one flag that re-baselines
sessions you were never shown, because the ingest stops at the first refusal and so can only ever
print one of them before you decide.

For a project manifest, name the team and the session together as `TEAM:SESSION`, because one
project run ingests every registered team and each Orc team is a whole session tree — either half
alone is a blanket switch over the other. A slug that is not registered, is not an Orc team, or is
not in this run's `--team` selection is rejected before any ingest starts, as is a bare session id
with no team on it:

```bash
wrkviz ingest-project --config configs/widget.json \
  --accept-orc-prefix-rewrite orc-example-014:SESSION_UUID
```

The session ids you were permitted to re-baseline — whether or not any of them needed it — are
recorded in the run receipt as `accepted_prefix_rewrite_sessions`, so a flag left switched on in a
scheduled job is visible rather than indistinguishable from a clean run.

The override is narrow, loud, and permanent in the record:

- **It never covers a lost or added record.** A row that disappeared from the prefix, a row that
  appeared inside it, and a shrinking history all still refuse, flag or no flag. Only values
  changing in rows that were already there are accepted, and the new snapshot is a full copy of the
  current source, so nothing is dropped.
- **It never covers a session you did not name.** A session id that is not in the discovered
  lineage is rejected before a single database is copied, so a typo cannot leave you believing an
  override applied when it authorized nothing.
- **It re-baselines the record, and re-baselines nothing but the record.** The digest becomes that
  of the source as it now stands. If the session was still running — as the one this exists for
  was — then `append_count` and `append_max_id` move by exactly the rows that arrived while you
  were deciding, and every one of those rows is ingested; the re-baselined digest is then a value
  equal to neither digest in the override record, which reports the before and after of the span
  that was actually compared. The source manifest, the frozen task `.projections` pointer, and
  ownership carry through unchanged, and the next run needs no flag.
- **It reports itself.** The changed rows and columns are printed to stderr (so they survive
  redirecting stdout to a log), recorded in the run receipt under `ingest.orc_prefix_overrides`,
  and stored beside the source in the manifest as `append_prefix_override` with `degraded: true`
  and the reason `append-prefix-rewritten-operator-accepted-rows-preserved`. That record is sticky
  and counts up: an archive that was ever re-baselined keeps saying so.
- **It keeps the bytes it stopped trusting.** The pre-rewrite snapshot object is *not* collected by
  the run that supersedes it. Its path and digest are recorded as `superseded_snapshot_path` and
  `superseded_sha256`, printed as the last line of the acceptance report, and honoured by object
  GC on every later run, so an accepted rewrite always leaves a route back.

Every part of the *report* is bounded—at most 20 rows, windowed value excerpts, at most 8 JSON paths
per column—and any bound that was applied is flagged rather than hidden. Those caps are a summary of
an event whose two sides both survive on disk, so more detail is always a `sqlite3` session away:

```bash
cd ../summary/widget.sources/orc-example-014
sqlite3 .objects/16/1617869e....db \
  "SELECT message_json FROM messages WHERE id = 878292"
```

That path is the snapshot store, which for an archive that has not been through
`migrate-snapshots` is still `../summary/widget/teams/orc-example-014/source_snapshots` instead.
`wrkviz inspect` prints the one this archive uses, as `source_snapshots.store_root`.

Retention is one deep per source, and only another *accepted* override reclaims it: the second
acceptance supersedes the first's pointer, and the next GC takes the older object. So the bytes are
never reclaimed by a routine run—only by a second deliberate decision that carries the same warning
as the first. Copy the archive if you need to keep more than the latest generation. Deleting a
retained object by hand does not break anything either: it is evidence, not an input, so a later
ingest still runs and the manifest still records that the rewrite happened.

SQLite backups are staged, integrity-checked, published into an immutable content-addressed object
store, and fsynced before the manifest is updated. `raw/normalized-generation.json` is written last
and binds the source manifest, normalized team, artifact catalog, and semantic source digest. A
reader rejects a stale/missing marker after an interrupted write; rerunning ingest repairs it and
then garbage-collects unreachable managed objects. Exact schema validation and component-wise
symlink rejection apply to both live log paths and archive paths.

Some Orc installations discover nested coordinator sessions without persisting a parent identifier.
The importer preserves nested lineage when that field exists and does not invent a parent when it is
absent. Agent-close timestamps are also not always persisted, so a lifetime ends at its last
attributed event/tool/turn/spawn or at the next reuse of the same official name. Child end times
propagate upward so parent lifetimes contain all recorded descendant activity; mutable Orc
`updated_at` values never extend a lifetime.

Separate parentless Orc roots remain separate unless their ordered relationship is configured.
Whole-root continuation strings include the complete successor lineage. A bounded continuation
object starts at one frozen native message inside a reused root. It retains only post-boundary
lineage evidence and relevant task databases, clamps retained agent lifetimes to the boundary, and
adds one structural continuation edge from the predecessor. Proximity, matching database names,
or a display window never infer this relationship.

## Codex source semantics and limitations

Codex disk logs are append-only across context compaction: old user, assistant, and tool records are
not deleted when the in-memory context is compacted. The importer deliberately ignores compaction
summaries and uses the original records.

Separate coordinator root sessions are separate lineages unless the operator supplies ordered
`--continuation-session` values. This explicit-only rule avoids silently merging nearby but
unrelated work. A configured successor receives a collision-safe path beneath
`/root/continuation-<root-uuid>` and collision-safe normalized event/turn/tool IDs, while all IDs
and cache inputs from the original lineage stay unchanged. Its one continuation edge is structural;
it is neither a worker spawn nor evidence for a fabricated lifetime return.

Child rollout files begin with a copied parent-history prefix whose timestamps are rewritten to the
spawn time. The importer finds the child's first incoming task message and treats earlier records as
context only. Counting every raw line would otherwise duplicate much of the coordinator history and
create false activity spikes.

Codex persists spawn instructions, follow-up prompts, and mid-turn collaboration messages
as encrypted `gAAAA...` content in both sender and receiver rollouts. An offline exporter cannot
decrypt it. The archive preserves the availability state and ciphertext in normalized message data;
the UI clearly says the exact body is unavailable and shows an inferred paragraph from the receiving
agent's subsequent work. Automatic final answers and each agent's own commentary/final response are
plaintext; encrypted instruction bodies remain unavailable to the archive.

## Inspect and troubleshoot

Agents and shell scripts can navigate the same archive without starting the website. Every archive
ships a dependency-free executable named `timeline`; it locates the archive from its own path, so it
also works when called from another directory. Prompt and message output defaults to readable text.
`--format jsonl` streams one result per line, while `--format json` and `--format markdown` serve
structured and rendered consumers.

```bash
cd ./timelines/example-team
./timeline --help
./timeline teams
./timeline agents --team example-team \
  --start-time 2026-08-07T02:00:00Z --end-time 2026-08-07T03:00:00Z
./timeline show 'agent:example-team::SESSION_OR_AGENT_ID'
./timeline show 'phase:example-team::phase-0123456789abcdef' --transcript
./timeline search 'reproducible build' --scope all --team example-team --limit 20
./timeline search 'backend maturity B3' --in owner-prompts
./timeline search '50% ptrace' --in agent-responses --prompt-author owner
./timeline search KVM --in all-transcript --sort newest --limit 20
./timeline prompts --range 200-300
./timeline prompts --which all --format jsonl > all-prompts.jsonl
./timeline prompts --format jsonl > prompts.jsonl
./timeline messages --range 200-300 --format jsonl
./timeline stats
./timeline stats --team example-team --format json
```

`prompts` is ordered globally by timestamp and accepts one 1-based inclusive ordinal or `N-M` via
`--range`. It defaults to `--which human`: durable `owner_human` and `other_human` labels are human,
while `--which bot` selects `agent` and `system`, and `--which all` also includes unknown or
externally unattributed records. The query never infers authorship from prose. `messages` returns the
same selected prompts plus responses mechanically linked to them. Both also accept repeatable
`--team` and half-open RFC3339 time bounds. They read the digest-validated
`extracted/transcripts/` projection and therefore work even before a website has been built.
`./timeline prompts --help` documents the range, team, time, and output controls.

`stats` performs no write and no model call. It reports record, whitespace-delimited word, and
UTF-8 text-byte totals for mechanically identified human, bot/agent, and unattributed
prompts, mechanically linked and total responses, and generated summaries, followed by
available/unavailable summary coverage for each summary surface. Authorship counts use only the
durable `author_kind` label; ambiguous provider inputs are reported as unattributed rather than
guessed from prose. It accepts the same repeatable team and half-open RFC3339 time filters; `--kind`
can restrict rollup accounting. Time-sliced statistics omit project overviews because an overview
describes the whole project and has no honest time interval. Summary bytes count the user-visible
projection text (including referenced rollup Markdown verbatim), not CLI or JSON serialization.

The `teams`, `agents`, `phases`, and `rollups` commands return records carrying canonical
references that `show` accepts directly:

- `team:TEAM`
- `agent:TEAM::SOURCE_AGENT_ID`
- `phase:TEAM::PHASE_ID`
- `rollup:TEAM::KIND::START_MS`

The references deliberately remove the compositor's presentation-only team prefix, so they remain
the same when an individual team is later included in a combined export. `show` adds parent,
children, and work-phase references for an agent; phase transcripts remain excluded unless
`--transcript` is explicit. Showing a rollup returns each available technical or plain-language
Markdown audience and reports false availability without trying to open a missing file.

Use `--in owner-prompts`, `--in agent-responses`, or `--in all-transcript` for the phase-independent
search corpus. The default `smart` matcher treats unquoted word-like terms as whole terms and
requires every term, so `B3` does not match a hexadecimal hash fragment. Quote a phrase inside a
smart query, or select `--match phrase`/`--match literal` when substring behavior is desired.
`--sort relevance|newest|oldest`, `--offset`, `--limit`, repeatable `--role`, `--prompt-author`, and
`--linkage` provide deterministic filtering and pagination; the result envelope reports both total
and returned matches. Search results carry stable `message:` or `tool:` references. `show` resolves
one of those references and adds the enclosing phase and mechanically linked prompt/responses when
present. Parent-to-child instructions and child-to-parent final replies are directionally classified
before linkage and attributed to the child workstream. Same-thread source order breaks timestamp
ties; cross-rollout parent/child links require a strictly earlier instruction and leave equal-time
ties unlinked rather than guessing. Each team/day
catalog entry has a false-positive-only ASCII trigram filter. Eligible queries fetch only candidate
day objects; short or non-ASCII terms never reject an object on their own. Matching is
ASCII-case-insensitive and non-ASCII-exact so it stays byte-for-byte compatible with that filter.
Compact linkage sidecars preserve prompt excerpts and linked-response counts across UTC-day
boundaries even when the prompt's text shard does not match the query. The website loads the full
linked messages on demand, debounces normal typing, and bounds concurrent shard requests. On a
schema-3 archive it reads the same corpus out of `data/timeline-v3/search{,-bloom,-links}/`, and
the trigram filter is fetched **only when the query has a term a trigram can be built from** — so a
two-byte query such as `B3` transfers no filter data at all, where the older layout carried every
filter in the bootstrap and then used none of them.

The older `--scope summaries|transcripts|all` form remains available for compatibility with phase
summary/detail search. Search-v2-only flags require `--in`; they are rejected rather than silently
ignored. `--agent` restricts work-phase and transcript results to one canonical agent reference.
Time bounds are half-open RFC3339 instants. Timeline queries read deterministic static projection
files, while prompt/message queries read the manifest-bound extracted JSONL. Neither invokes a model
or alters the archive. `./timeline` is the sole generated query launcher. The Make targets are
documented convenience wrappers, and plain `make` prints their help instead of starting a server.
Older archives with the retired, byte-identical `query.py` alias remain readable; a rebuild removes
that generated alias without changing the internal `wrkviz.query` implementation.

```bash
wrkviz inspect --output ./timelines/example-team
wrkviz gc --output ./timelines/example-team
```

`inspect` prints track/phase/edge/event/rollup counts and the current manifest. It reads them out
of the schema-3 catalogue when the archive has one, which is why it is now instant: on one
twelve-team archive the old whole-monolith parse cost 2.4 seconds and 1.44 GiB resident for six
integers the 169 KB bootstrap already publishes.

`gc` accounts for disk rather than records: every file classified as live, reclaimable or held,
with bytes and a reason per category. It changes nothing without `--delete`; see "The retired
schema-1 monolith, and `gc`" above for what it will and will not reclaim.

Common errors:

- **Root not found:** verify the UUID and `--sessions-root`; pass the root coordinator, not a child.
- **Missing phase or rollup summary:** run `summarize` with the same `--phase-minutes` used by `build`.
- **Codex backend failure:** the command prints a concise stderr excerpt and leaves all existing
  cache files untouched. Retry; independently validated batches from the failed invocation remain
  cached, while the failed or cancelled batches are regenerated.
- **Website is blank under `file://`:** launch `make serve`.
- **Wrong day boundary:** choose the desired IANA `--timezone`; never pass a fixed abbreviation such
  as EDT because it cannot model winter or daylight-saving transitions.

## Security and privacy

Transcripts may contain proprietary text, paths, prompts, and encrypted collaboration payloads.
Treat the archive like the source logs. The bundled server binds to `127.0.0.1`, the browser uses no
external scripts, and transcript text is inserted with safe DOM text APIs rather than interpreted as
HTML. Review repository visibility before committing an archive.

## Supported inputs and deployment boundaries

- Codex provider: implemented.
- Claude Code provider: implemented, including nested subagents and bounded local-date archives.
- Orc provider: implemented, including read-only SQLite snapshots, agent incarnations, nested
  coordinators when lineage is recorded, and bounded local-date archives.
- Multiple teams: the website schema and team filter are ready; one refresh currently writes one
  team archive. Merging team indexes is a follow-up.
- Additional adapters such as Gas Town should emit the same provider-neutral
  agents/turns/events/tools/edges model.
- Hosted serving: deliberately deferred. The local archive is complete and portable first.
