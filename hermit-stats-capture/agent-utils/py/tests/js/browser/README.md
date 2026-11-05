# wrkviz browser checks

This directory runs the production timeline assets against a small, deterministic
Codex-team fixture. The fixture server uses only Node's standard library and exposes
no repository files outside `py/wrkviz/static/`.

Install and run from this directory:

```bash
npm install
npx playwright install chromium
npm test
node --test benchmark-site.test.cjs
node --test benchmark-budget.test.cjs
```

The browser contract targets Node 22, matching CI. Repository validation installs the pinned
Node 22 release with `scripts/provision-node22` (the official archive, verified against its
published sha256). The `py/wrkviz` Make targets and validation run through `scripts/with-node22`,
which prefers that release, then an active Node 22, then an already-installed nvm Node 22, and
fails with an explicit requirement instead of silently testing against a different runtime.

Use `npm run test:headed` while iterating. The browser download and `node_modules/`
are intentionally untracked; this directory does not vendor generated npm or browser
artifacts.

The suite treats DOM observability as a small UI testing contract. A check skips with
a precise reason until its contract exists, then enforces the behavior. Production UI
code should expose:

- `[data-testid="timeline"]` with `data-view-start-ms`, `data-view-end-ms`,
  `data-selection-scope`, `data-selected-agent-id`, and
  `data-selected-phase-id` as applicable;
- phase groups with `data-phase-id` and `data-agent-id`;
- `#timeline-svg` with `data-track-mode="packed|per-agent"` and a numeric
  `data-lane-count`;
- `[data-testid="timeline-context-menu"]`, with actions using `role="menuitem"`;
- `[data-testid="transcript-role-filters"]`, and transcript cards with a normalized
  `data-role` (`user`, `assistant`, `agent`, `tool`, `system`, or `other`).

The fixture serves no `data/timeline-v3.json`, so this suite now exercises the bundle's
**fallback** path: the page probes schema 3, gets a clean 404, and loads the mocked schema-2
bootstrap without annotating the meta line. That is the state an archive built by an older tool is
in, and it is worth having covered by something that drives a real browser. Schema 3's own read
path is covered by `../test_timeline_v3_ui.js` and by `tests/test_timeline_v3_website.py`, which
serves a real build through the archive's own `serve.py` so the byte ranges are answered by the
server that ships with the archive.

## The second fixture: a generated archive with levels of detail

`synthetic-scale.spec.js` runs against a different fixture, for a question the four-agent one
cannot answer. `timeline-core.semanticZoomLevel` picks `detail`, `lifetime` or `aggregate` from
milliseconds-per-pixel, so an archive spanning one afternoon renders `detail` at every reachable
zoom and the other two branches — the ones that suppress phases, then agents and edges, and keep a
large archive responsive — are never taken.

That fixture is generated rather than committed. `python3 -m wrkviz.synthetic` writes deterministic
Claude-shaped coordinator and subagent transcripts at a requested size, and the ordinary
ingest/summarize/build path turns them into a real archive, served here by the `serve.py` inside
it. The size the spec uses is the `ci` preset — 201 agents, 1,212 phases and about 6,000 tool
calls over eleven days — which takes about ten seconds to build and is cached under a fingerprint
of both the requested size and the package that produced it, so a later run is a few hundred
milliseconds and an edit to the builder invalidates it. The cache lives in `.synthetic-archive/`
and is not tracked: it is 30 MB of content-addressed shards and their sidecars, and committing a
generated tree of that shape would make every builder change a large diff nobody reads.

For a bigger one, by hand:

```bash
make -C ../../../wrkviz synth-archive OUT=/tmp/synth PRESET=large
make -C ../../../wrkviz qa-archive ARCHIVE=/tmp/synth/archive
```

## The small fixture

The four-agent fixture intentionally has: one coordinator, two overlapping agents,
and a third agent whose lifetime starts exactly when the first ends. That makes the
packed-lane assertion cover both overlap and half-open lifetime reuse. Its spawn,
intermediate message, and lifetime result also verify that fork/join edges remain visible
while detailed traffic follows the toolbar toggles.
