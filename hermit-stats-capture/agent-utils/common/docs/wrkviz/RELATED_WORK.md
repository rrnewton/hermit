# Agent team timeline related work

Research checked 2026-08-05 against project repositories, source code, release metadata, and
package registries. The longer Gemini report that prompted this review is preserved in
[`reviews/research/wrkviz-comparative-analysis.md`](../../../reviews/research/wrkviz-comparative-analysis.md).

## Direct answer

There is now a genuinely comparable off-the-shelf application:
[AgentsView](https://github.com/kenn-io/agentsview). Someone who mainly wants a local, polished
browser for Codex and Claude history should try it before installing this project. It is MIT
licensed, supports more than forty agent formats, and already provides live transcripts, tools,
search, usage statistics, nested Codex subagent navigation, and generated session/date-range
insights.

It is not a drop-in replacement for `wrkviz`. No project we found combines all of
these properties:

- one wall-clock lane per coordinator or nested agent, with fork/join and message edges;
- active, tool, waiting, and idle intervals within each lane;
- hindsight phase clustering and short names grounded in parent context;
- phrase, paragraph, work-summary, and condensed-transcript levels of detail;
- daily through quarterly reductions over the same underlying history;
- monotonic raw-log snapshots, content-addressed model caches, and Git-friendly Markdown/JSON;
- a static, offline website that overlays multiple teams on real time.

The closest alternative architecture would combine AgentsView or `llm-wiki` for ingestion,
Perfetto or AgentPrism for trace rendering, and a new summarization/archive layer. Most of the
project's distinctive implementation would still be custom.

## At a glance

`Partial` includes features that require an adapter or have materially different semantics.

| Project | Existing coding-agent logs | Nested agents | Wall-clock lanes and causal edges | Hierarchical summaries | Static, versioned archive |
| --- | --- | --- | --- | --- | --- |
| `wrkviz` | Yes | Yes | Yes | Yes | Yes |
| [AgentsView](https://github.com/kenn-io/agentsview) | Yes | Yes | Partial | Partial | Partial |
| [Engineering Notebook](https://github.com/prime-radiant-inc/engineering-notebook) | Yes | Partial | Partial | Partial | No |
| [`llm-wiki`](https://github.com/Pratiyush/llm-wiki) | Yes | No | No | Partial | Yes |
| [Claude Code Agent Monitor](https://github.com/hoangsonww/Claude-Code-Agent-Monitor) | Yes | Yes | Partial | No | No |
| [Langfuse with its Codex plugin](https://github.com/langfuse/codex-observability-plugin) | Partial | Yes | Partial | No | No |
| [Phoenix with ATIF](https://arize.com/docs/phoenix/tracing/how-to-tracing/importing-and-exporting-traces/importing-atif-trajectories) | Partial | Yes | Partial | No | No |
| [Perfetto](https://github.com/google/perfetto) | Adapter | Adapter | Yes | No | Partial |

## Closest applications

### AgentsView

[AgentsView](https://github.com/kenn-io/agentsview) is the strongest substitute and the most
important project missing from the Gemini report. At review time it had about 4,700 GitHub stars
and had released v0.40.1 on 2026-08-04.

Its Codex parser recognizes `spawn_agent`, collaboration events, `parent_thread_id`, and official
agent paths. Its database repair logic and tests cover nested parentage deeper than one hop. The UI
supports full transcripts and tools, live sessions, search, analytics, inline subagent drilldown,
daily and date-range AI insights, and experimental provenance-linked recall.

The differences are structural:

- Its concurrency chart is aggregate bucket bars, not a lane for every agent.
- It does not draw spawn, follow-up-message, and result edges over the time axis.
- It does not infer semantic work phases or active/tool/waiting/idle subblocks.
- Insights live in SQLite and do not form an append-only Markdown/JSON rollup tree.
- Multi-session insight generation samples at most fifty session records and first-message
  previews; it is not the cross-edge, full-context hierarchical reduction used here.
- The current inline call display deliberately limits recursive expansion, although deeper sessions
  remain individually navigable.

Useful primary references are the
[insights design](https://github.com/kenn-io/agentsview/blob/main/docs/insights.md),
[Codex parser](https://github.com/kenn-io/agentsview/blob/main/internal/parser/codex.go),
[nested-parent tests](https://github.com/kenn-io/agentsview/blob/main/internal/db/link_subagent_nested_test.go),
and
[concurrency timeline](https://github.com/kenn-io/agentsview/blob/main/frontend/src/lib/components/activity/ConcurrencyTimeline.svelte).

### Engineering Notebook and `llm-wiki`

[Engineering Notebook](https://github.com/prime-radiant-inc/engineering-notebook) is an
Apache-2.0 local history application for Claude and Codex. It combines transcript search with LLM
daily summaries, journals, project timelines, calendars, and a Gantt-like view. It is the closest
application to the long-term progress-report side of this project. Its view is organized around
projects and days rather than agent lanes; detected subagent sessions are excluded from daily
summarization and are not presented as topology.

[`llm-wiki`](https://github.com/Pratiyush/llm-wiki), published on PyPI as `llm-notebook`, is the
closest archive component. It imports existing Claude, Codex, Cursor, Gemini, and Copilot history;
creates immutable clean Markdown; optionally synthesizes entities, concepts, sources, and wiki
pages; emits static HTML and machine-readable siblings; and uses SHA-256 manifests for idempotency.
It has no coordinator/subagent lineage, state intervals, delegation edges, temporal phase model, or
calendar reduction hierarchy. Its provider adapters and archive conventions merit another look
when this project adds providers.

### Live agent-team dashboards

[Claude Code Agent Monitor](https://github.com/hoangsonww/Claude-Code-Agent-Monitor) is the closest
live team monitor. The MIT-licensed Node/React/SQLite application supports Claude and Codex,
pre-existing-session import, agent hierarchy and state views, conversations, tool events,
orchestration DAGs, collaboration views, and multi-machine collection. It is a substantial service
for watching current execution, not a static semantic history or durable report archive.

[AgentWatch](https://github.com/mishanefedov/agentwatch) provides an MIT-licensed local SQLite event
ledger for several coding agents, with live updates, search, cost views, and Markdown/JSON session
exports. Its current changelog documents a daemon and persistent store despite a stale README
limitation saying otherwise. Codex subagent drilldown remains explicitly unsupported, and it has no
generative summary hierarchy.

Other useful transcript viewers include
[Codex Trace](https://github.com/PixelPaw-Labs/codex-trace),
[Agent Log Viewer](https://github.com/Latand/live-log-viewer-next),
[Agent Sessions](https://github.com/jazzyalex/agent-sessions),
[claude-devtools](https://github.com/matt1398/claude-devtools),
[Clawd Insights](https://github.com/yx0716/clawd-insights), and
[Brain0](https://github.com/Brain0-ai/brain0). They are good parser and interaction references, but
none supplies the combined temporal topology and longitudinal summary artifact tree.

## Trace observability platforms

### Langfuse

[Langfuse](https://github.com/langfuse/langfuse) plus its MIT-licensed
[Codex observability plugin](https://github.com/langfuse/codex-observability-plugin) is the closest
ready-made trace backend. The plugin parses Codex rollouts, reconstructs tool calls and nested
subagents, backdates span timings, records model/token information, and deduplicates uploaded
turns. Langfuse renders nested parallel traces and supports sessions, evaluations, and self-hosting.

It is principally a hook for new sessions rather than a packaged historical scanner. A turn is the
main trace unit rather than one continuous team history, fields are truncated by default, and
self-hosting replaces a copied static directory with database services. Langfuse core is MIT; its
enterprise directories use a separate license.

### Phoenix and generic tracing systems

[Arize Phoenix](https://github.com/Arize-ai/phoenix) can import the Agent Trajectory Interchange
Format (ATIF), nest referenced subagent trajectories under delegation calls, merge continuation
files, and derive deterministic identifiers so repeated imports do not duplicate traces. It also
has a span waterfall and conversation sessions. Phoenix itself currently uses the Elastic License
2.0 and is source-available, not OSI-approved open source; the related
[coding-harness-tracing](https://github.com/Arize-ai/coding-harness-tracing) project is Apache-2.0.

[AgentOps](https://github.com/AgentOps-AI/agentops),
[OpenLIT](https://github.com/openlit/openlit),
[OpenLLMetry](https://github.com/traceloop/openllmetry), Jaeger, Zipkin, and Grafana Tempo all
offer useful instrumented trace storage and waterfalls. They are aimed at observing applications
that emit spans, not reconstructing and narrating arbitrary historical coding-agent files.

Workflow studios such as LangGraph/LangSmith Studio, AutoGen Studio, Langflow, Flowise, Airflow,
Dagster, Argo, and Temporal display workflows defined in their own systems. They are architectural
analogues, not importers or substitutes for this history tool.

## Reusable visualization and transcript components

### Perfetto

[Perfetto](https://github.com/google/perfetto) remains the strongest off-the-shelf timeline engine.
It is Apache-2.0, handles very large traces, supports duration slices, custom/nested tracks,
flow arrows, and SQL analysis, and reads Chrome Trace Event JSON. Contrary to the older Gemini
report, Perfetto now has an official
[iframe embedding API](https://perfetto.dev/docs/visualization/embedding-the-ui) and can be
self-hosted. The Apache-2.0
[`perfetto-embed`](https://github.com/LalitMaganti/perfetto-embed) wrapper makes loading and zooming
an embedded trace straightforward.

Perfetto still replaces only the rendering kernel. The documented
[embedding API](https://perfetto.dev/docs/visualization/embedding-api-reference) accepts trace and
viewport commands but does not expose a host-facing slice-selection callback. Opening custom
summary/transcript modals would require a Perfetto plugin, a maintained customization, or a second
interaction layer. Semantic phase labels, viewport statistics, summarization, and archive
navigation remain application work. An optional Chrome Trace export and “Open in Perfetto” action
would provide value without replacing the purpose-built UI.

### Other components

[AgentPrism](https://github.com/evilmartians/agent-prism) is the closest agent-specific trace UI:
it has OTLP and Langfuse adapters, a hierarchical timeline/tree, selection callbacks, and a detail
pane. Its UI package is private/unpublished; the documented installation method vendors its React
and Tailwind source. It is a reference or source-vendoring choice, not a normal package dependency.

[OpenAI Euphony](https://github.com/openai/euphony) provides Apache-2.0 static Web Components that
parse and render Codex JSONL. It is worth evaluating for the full-transcript tab. It does not
provide team topology or summaries.

Generic libraries cover only portions of the interaction:

- [`vis-timeline`](https://github.com/visjs/vis-timeline) and
  [`react-calendar-timeline`](https://github.com/namespace-ee/react-calendar-timeline) provide
  mature grouped ranges and zooming but no causal arrows.
- [Frappe Gantt](https://github.com/frappe/gantt) has dependency arrows, but its project-task model
  is awkward for overlapping agent-state segments.
- [React Flow](https://github.com/xyflow/xyflow) provides excellent DAG interactions but no
  wall-clock X axis.

These libraries reduce drawing code, but integrating nested rows, semantic zoom, viewport
aggregation, edge routing, and transcript selection remains custom. The prototype's dependency-free
SVG renderer is therefore defensible; Perfetto becomes attractive if trace scale overwhelms it.

## Interchange standards

[ATIF](https://www.harborframework.com/docs/agents/trajectory-format), whose Harbor schema is
[Apache-2.0](https://github.com/harbor-framework/harbor), represents complete agent trajectories
and nested subagent references. Import and export would make archives portable to Phoenix and
other trajectory tools. The internal schema should retain richer state intervals, cross-agent
messages, and summaries.

[OpenInference](https://github.com/Arize-ai/openinference) is an Apache-2.0 vocabulary over
OpenTelemetry for `AGENT`, `LLM`, `TOOL`, `CHAIN`, and related spans. It is useful as an export
target. The Gemini report overstates its fit: OpenInference does not itself define active,
waiting, idle, and blocked scheduler states. Those must be reconstructed from events and gaps.

## Summary research and reports

[RAPTOR](https://github.com/parthsarthi03/raptor) is useful inspiration for recursive summary
rollups, but it is a semantic retrieval tree rather than a temporal viewer. Temporal weighting and
cross-spawn context are adaptations proposed for this project, not existing RAPTOR features.
STRACE, TRACE, and TokenSqueeze similarly concern causal or token-efficient trajectory compression;
they are research techniques, not deployable transcript archives or viewers. No standard repository
license was found for the checked STRACE and TRACE repositories, so their code should not be
reused without clarification.

Smaller report generators—
[AI Report to Me](https://github.com/FakeHank/ai-report-to-me),
[Dailywork Matters](https://github.com/highword/dailywork-matters-mcp), and
[`daily-ai-summary`](https://github.com/jexmarc/daily-ai-summary)—demonstrate demand for incremental
daily or longer-period Markdown summaries, but none retains the detailed agent-team execution
graph.

## Corrections to the Gemini report

The Gemini report contains useful architectural ideas, but it is not a sufficient package survey:

- It misses AgentsView, Engineering Notebook, `llm-wiki`, Claude Code Agent Monitor, Clawd
  Insights, Agent Log Viewer, and several direct Codex tracing plugins.
- Perfetto embedding is now officially supported. A documented host-side selection-event API is
  not, so custom modal integration is harder than the report implies.
- AgentWatch now has persistent SQLite storage and a daemon.
- OpenInference span kinds do not encode idle, waiting, or blocked states.
- RAPTOR temporal weighting and cross-edge context are proposed adaptations, not RAPTOR features.
- Polygentic appears to be a hosted product; no licensed OSS implementation was identified.
- Several citations are secondary articles or unrelated directories where primary project sources
  are available.

## Project decision

Had AgentsView been identified before implementation, it deserved a prototype evaluation and may
have been sufficient for users willing to trade the report archive for a SQLite history browser.
It could also supply parser ideas, although adopting its Go backend now would add a second runtime
and would not eliminate the summary pipeline or custom UI work.

For the stated requirements, continue the custom archive and summarization pipeline. Revisit these
specific reuse opportunities:

1. Export Chrome Trace Event JSON for an optional expert Perfetto view.
2. Add ATIF and OpenInference/OpenTelemetry import or export.
3. Evaluate Euphony for the verbatim transcript tab.
4. Review `llm-wiki` adapters when adding Claude and other providers.
5. Re-evaluate AgentsView periodically, especially if it adds per-agent lanes, causal edges, or
   durable hierarchical summaries.

The clearest positioning is: **a local-first semantic history and archival layer for agent teams**.
It complements trace observability systems; it is differentiated by durable, multilevel narrative
over arbitrary historical logs rather than tracing alone.
