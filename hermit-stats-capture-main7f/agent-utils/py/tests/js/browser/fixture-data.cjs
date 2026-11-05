"use strict";

const BASE_MS = Date.UTC(2026, 2, 9, 3, 15, 0);
const minute = 60 * 1000;
const DATA_START_MS = Date.UTC(2026, 2, 9, 3, 10, 54);
const DATA_END_MS = Date.UTC(2026, 2, 9, 20, 28, 31);
const ROLLUP_RANGES = [
  {
    kind: "daily",
    label: "Sun Mar 8",
    start_ms: Date.UTC(2026, 2, 8, 5),
    end_ms: Date.UTC(2026, 2, 9, 4),
    path: "summaries/daily/2026-03-08.md"
  },
  {
    kind: "daily",
    label: "Mon Mar 9 · partial",
    start_ms: Date.UTC(2026, 2, 9, 4),
    end_ms: Date.UTC(2026, 2, 10, 4),
    path: "summaries/daily/2026-03-09.md"
  },
  {
    kind: "weekly",
    label: "2026-W11",
    start_ms: Date.UTC(2026, 2, 9, 4),
    end_ms: Date.UTC(2026, 2, 16, 4),
    path: "summaries/weekly/2026-W11.md"
  },
  {
    kind: "monthly",
    label: "March 2026",
    start_ms: Date.UTC(2026, 2, 1, 5),
    end_ms: Date.UTC(2026, 3, 1, 4),
    path: "summaries/monthly/2026-03.md"
  },
  {
    kind: "quarterly",
    label: "2026 Q1",
    start_ms: Date.UTC(2026, 0, 1, 5),
    end_ms: Date.UTC(2026, 3, 1, 4),
    path: "summaries/quarterly/2026-Q1.md"
  }
];
const FIRST_DAY_ACTIVITY_START_MS = BASE_MS;
const LATEST_ACTIVITY_END_MS = BASE_MS + 100 * minute;
const PHASE_A2_ACTIVITY_START_MS = BASE_MS + 27 * minute;
const PHASE_A2_ACTIVITY_END_MS = BASE_MS + 29 * minute;
const AGENT_A_ACTIVITY_START_MS = BASE_MS + 5 * minute;
const AGENT_A_ACTIVITY_END_MS = PHASE_A2_ACTIVITY_END_MS;
const ROLLUP_EXPECTED_RANGES = [
  { start_ms: FIRST_DAY_ACTIVITY_START_MS, end_ms: ROLLUP_RANGES[0].end_ms },
  { start_ms: ROLLUP_RANGES[1].start_ms, end_ms: LATEST_ACTIVITY_END_MS },
  { start_ms: ROLLUP_RANGES[2].start_ms, end_ms: LATEST_ACTIVITY_END_MS },
  { start_ms: FIRST_DAY_ACTIVITY_START_MS, end_ms: LATEST_ACTIVITY_END_MS },
  { start_ms: FIRST_DAY_ACTIVITY_START_MS, end_ms: LATEST_ACTIVITY_END_MS }
];
const OUTPUT_ARTIFACT_ID = "artifact-pr38";
const REFERENCE_ARTIFACT_ID = "artifact-issue41";
const UNSAFE_ARTIFACT_ID = "artifact-unsafe-link";
const ASSOCIATED_ARTIFACT_IDS = [
  OUTPUT_ARTIFACT_ID,
  REFERENCE_ARTIFACT_ID,
  OUTPUT_ARTIFACT_ID,
  UNSAFE_ARTIFACT_ID
];

function stats(overrides) {
  return Object.assign({
    user_prompts: 1,
    agent_responses: 2,
    inter_agent_messages: 1,
    tool_calls: 1
  }, overrides || {});
}

function state(kind, startMinute, endMinute) {
  return {
    kind: kind,
    start_ms: BASE_MS + startMinute * minute,
    end_ms: BASE_MS + endMinute * minute
  };
}

const agents = [
  {
    id: "coordinator",
    team: "codex-widget",
    parent_id: "",
    depth: 0,
    path: "/root",
    official_name: "/root",
    official_leaf: "root",
    short_name: "Widget coordinator",
    nickname: "Coordinator",
    lifetime_summary: "Coordinated the parser, continuous-integration, and documentation work into one verified result.",
    start_ms: DATA_START_MS,
    end_ms: BASE_MS + 100 * minute,
    status: "complete"
  },
  {
    id: "agent-a",
    team: "codex-widget",
    parent_id: "coordinator",
    depth: 1,
    path: "/root/transcript_auditor/owner_turn_miner/plugin_layout_audit/parser_boundary_regression_audit",
    official_name: "/root/transcript_auditor/owner_turn_miner/plugin_layout_audit/parser_boundary_regression_audit",
    official_leaf: "parser_audit",
    short_name: "Parser audit",
    nickname: "Ada",
    lifetime_summary: "Found the malformed-input parser boundary, repaired it, and verified the focused regression tests.",
    start_ms: BASE_MS + 2 * minute,
    end_ms: BASE_MS + 30 * minute,
    status: "complete",
    artifact_ids: ASSOCIATED_ARTIFACT_IDS,
    output_artifact_ids: [OUTPUT_ARTIFACT_ID, OUTPUT_ARTIFACT_ID]
  },
  {
    id: "agent-b",
    team: "codex-widget",
    parent_id: "coordinator",
    depth: 1,
    path: "/root/ci_audit",
    official_name: "/root/ci_audit",
    official_leaf: "ci_audit",
    short_name: "CI audit",
    nickname: "Turing",
    lifetime_summary: "Verified that continuous integration exercises the repaired parser boundary in every required job.",
    start_ms: BASE_MS + 10 * minute,
    end_ms: BASE_MS + 40 * minute,
    status: "complete"
  },
  {
    id: "agent-c",
    team: "codex-widget",
    parent_id: "coordinator",
    depth: 1,
    path: "/root/docs_audit",
    official_name: "/root/docs_audit",
    official_leaf: "docs_audit",
    short_name: "Documentation audit",
    nickname: "Curie",
    lifetime_summary: "Updated the operator guide to explain the repaired malformed-input behavior.",
    start_ms: BASE_MS + 30 * minute,
    end_ms: BASE_MS + 50 * minute,
    status: "complete"
  }
];

const phases = [
  {
    id: "phase-root",
    agent_id: "coordinator",
    start_ms: DATA_START_MS,
    end_ms: BASE_MS + 100 * minute,
    phrase: "Coordinate Widget work",
    paragraph: "Spawned focused audits and integrated their findings.",
    detail_path: "details/phase-root.json",
    stats: stats({ user_prompts: 2, agent_responses: 4 }),
    states: [
      { kind: "idle", start_ms: DATA_START_MS, end_ms: FIRST_DAY_ACTIVITY_START_MS },
      {
        kind: "active",
        start_ms: FIRST_DAY_ACTIVITY_START_MS,
        end_ms: LATEST_ACTIVITY_END_MS
      }
    ]
  },
  {
    id: "phase-a-1",
    agent_id: "agent-a",
    start_ms: BASE_MS + 10 * minute,
    end_ms: BASE_MS + 25 * minute,
    phrase: "Audit parser invariants",
    paragraph: "Found and tested the parser boundary that caused the regression.",
    detail_path: "details/phase-a-1.json",
    artifact_ids: ASSOCIATED_ARTIFACT_IDS,
    output_artifact_ids: [OUTPUT_ARTIFACT_ID, OUTPUT_ARTIFACT_ID],
    stats: stats({ tool_calls: 3 }),
    states: [
      state("active", 10, 15),
      state("tool", 15, 20),
      state("active", 20, 25)
    ]
  },
  {
    id: "phase-a-2",
    agent_id: "agent-a",
    start_ms: BASE_MS + 26 * minute,
    end_ms: BASE_MS + 30 * minute,
    phrase: "Verify parser fix",
    paragraph: "Confirmed the repaired parser behavior against the focused regression case.",
    detail_path: "details/phase-a-2.json",
    stats: stats({ tool_calls: 1 }),
    states: [
      state("idle", 26, 27),
      state("active", 27, 29),
      state("idle", 29, 30)
    ]
  },
  {
    id: "phase-b-1",
    agent_id: "agent-b",
    start_ms: BASE_MS + 12 * minute,
    end_ms: BASE_MS + 35 * minute,
    phrase: "Verify CI coverage",
    paragraph: "Checked that the regression test runs in the required CI jobs.",
    detail_path: "details/phase-b-1.json",
    stats: stats(),
    states: [state("active", 12, 35)]
  },
  {
    id: "phase-c-1",
    agent_id: "agent-c",
    start_ms: BASE_MS + 32 * minute,
    end_ms: BASE_MS + 48 * minute,
    phrase: "Clarify operator guide",
    paragraph: "Updated the explanation of the repaired parser behavior.",
    detail_path: "details/phase-c-1.json",
    stats: stats(),
    states: [state("active", 32, 48)]
  }
];

const AGGREGATE_HOUR_START_MS = Date.UTC(2026, 2, 9, 3);
const AGGREGATE_BURST_START_MS = AGGREGATE_HOUR_START_MS + 60 * minute;
const AGGREGATE_GAP_START_MS = AGGREGATE_HOUR_START_MS + 2 * 60 * minute;
const AGGREGATE_LATER_START_MS = AGGREGATE_HOUR_START_MS + 3 * 60 * minute;

function activityBin(role, resolution, startMs, endMs, average, peak, coverage, agents) {
  return {
    team: "codex-widget",
    role: role,
    resolution: resolution,
    start_ms: startMs,
    end_ms: endMs,
    avg_active_concurrency: average,
    peak_concurrency: peak,
    activity_coverage_fraction: coverage,
    distinct_active_agents: agents
  };
}

// THE THREE HOURLY BINS ARE A SCALE, not three arbitrary numbers. Team-block height is linear in
// agents present -- coordinator presence plus average worker concurrency -- so the first hour is
// built to hold exactly three agents and the last exactly one, and the browser suite asserts the
// drawn heights are in a 3:1 ratio. Change these numbers and that assertion is what breaks.
//
// The middle hour is a burst past the height cap, and it describes a team larger than this
// fixture's own four-agent roster on purpose: the cap sits at nine agents, so nothing a
// four-agent fixture could honestly report would ever reach it, and the clamped rendering would
// go untested by anything short of a real archive.
const activityBins = [
  activityBin(
    "coordinator", "hourly", AGGREGATE_HOUR_START_MS,
    AGGREGATE_BURST_START_MS, 1, 1, 1, 1
  ),
  activityBin(
    "workers", "hourly", AGGREGATE_HOUR_START_MS,
    AGGREGATE_BURST_START_MS, 2, 3, 0.9, 3
  ),
  activityBin(
    "workers", "hourly", AGGREGATE_BURST_START_MS,
    AGGREGATE_GAP_START_MS, 12.5, 24, 0.5, 26
  ),
  activityBin(
    "workers", "hourly", AGGREGATE_LATER_START_MS,
    AGGREGATE_LATER_START_MS + 60 * minute, 1, 1, 0.25, 1
  ),
  activityBin(
    "coordinator", "daily", Date.UTC(2026, 2, 9), Date.UTC(2026, 2, 10),
    0.2, 1, 0.2, 1
  ),
  activityBin(
    "workers", "daily", Date.UTC(2026, 2, 9), Date.UTC(2026, 2, 10),
    0.6, 3, 0.35, 3
  ),
  activityBin(
    "coordinator", "weekly", Date.UTC(2026, 2, 9), Date.UTC(2026, 2, 16),
    0.03, 1, 0.03, 1
  ),
  activityBin(
    "workers", "weekly", Date.UTC(2026, 2, 9), Date.UTC(2026, 2, 16),
    0.09, 3, 0.05, 3
  )
];

const timeline = {
  schema_version: 1,
  generated_at: "2026-03-09T17:40:00Z",
  source_digest: "playwright-fixture",
  display_timezone: "America/New_York",
  display_timezone_source: "explicit",
  range: {
    start_ms: DATA_START_MS,
    end_ms: DATA_END_MS
  },
  teams: [{
    slug: "codex-widget",
    label: "Codex Widget",
    projects: [
      {
        label: "dev-widget",
        repository_url: "https://github.com/example-org/dev-widget",
        primary: true,
        source: "session_metadata"
      },
      {
        label: "agent-utils",
        repository_url: "https://github.com/example-org/agent-utils",
        primary: false,
        source: "session_metadata"
      },
      {
        label: "widget",
        repository_url: "https://github.com/example-org/widget",
        primary: false,
        source: "session_metadata"
      }
    ],
    hosts: [{ hostname: "devbig014.example.com", source: "explicit" }]
  }],
  agents: agents,
  phases: phases,
  activity_bins: activityBins,
  edges: [
    {
      id: "spawn-a",
      kind: "spawn",
      source_id: "coordinator",
      target_id: "agent-a",
      source_ms: BASE_MS + 5 * minute,
      target_ms: BASE_MS + 5 * minute,
      phrase: "Audit the parser regression",
      paragraph: "The coordinator assigned the parser investigation to Parser audit.",
      full_text: "Audit the parser regression and return concrete test evidence."
    },
    {
      id: "message-a",
      kind: "message",
      source_id: "coordinator",
      target_id: "agent-a",
      source_ms: BASE_MS + 17 * minute,
      target_ms: BASE_MS + 17 * minute,
      phrase: "Check the error boundary",
      paragraph: "The coordinator narrowed the investigation to malformed input.",
      full_text: "Please check the malformed-input error boundary too."
    },
    {
      id: "result-a",
      kind: "result",
      source_id: "agent-a",
      target_id: "coordinator",
      source_ms: BASE_MS + 25 * minute,
      target_ms: BASE_MS + 25 * minute,
      phrase: "Parser invariant verified",
      paragraph: "Parser audit returned a fix and regression-test evidence.",
      full_text: "The parser invariant now holds and all focused tests pass."
    }
  ],
  events: [
    { agent_id: "coordinator", at_ms: BASE_MS + minute, kind: "user_prompt" },
    { agent_id: "agent-a", at_ms: BASE_MS + 16 * minute, kind: "tool_call" },
    { agent_id: "agent-a", at_ms: BASE_MS + 24 * minute, kind: "agent_response" }
  ],
  rollups: ROLLUP_RANGES.map(function (rollup) {
    return Object.assign({}, rollup, {
      technical_path: rollup.path,
      plain_language_path: rollup.path.replace(/\.md$/, "-plain-language.md"),
      artifact_ids: ASSOCIATED_ARTIFACT_IDS,
      output_artifact_ids: [OUTPUT_ARTIFACT_ID, OUTPUT_ARTIFACT_ID]
    });
  }),
  glossary: [
    {
      id: "term-malformed-input-123456789abc",
      term: "malformed-input",
      introduced_at_ms: BASE_MS + minute,
      occurrences: 4,
      context: "Malformed input is data that does not satisfy the parser's required structure.",
      definition: "A malformed-input record with [inline docs](https://attacker.invalid/inline), [reference docs][attacker-ref], bare https://attacker.invalid/bare, <https://attacker.invalid/autolink>, [mail](mailto:attacker@example.invalid), and ![an unverified image](https://attacker.invalid/image.png).\n\n[attacker-ref]: https://attacker.invalid/reference",
      definition_status: "supported",
      week: "2026-W11",
      url: "#glossary/term-malformed-input-123456789abc"
    }
  ],
  artifact_catalog_path: "data/artifacts.json",
  glossary_path: "summaries/glossary/codex-widget-glossary.md",
  summary_files: [
    {
      kind: "daily",
      period: "2026-03-09",
      label: "Monday, March 9",
      path: "summaries/daily/2026-03-09.md"
    },
    {
      kind: "weekly",
      period: "2026-W11",
      label: "Week 11",
      path: "summaries/weekly/2026-W11.md"
    }
  ]
};

const pullUrl = "https://github.com/example-org/dev-widget/pull/38";
const pullText = "I will trace the malformed-input path for " + pullUrl + ".";

const phaseADetail = {
  phrase: "Audit parser invariants",
  paragraph: "Found and tested the parser boundary that caused the regression.",
  stats: stats({ tool_calls: 3 }),
  work_summary: [
    {
      at_ms: BASE_MS + 12 * minute,
      text: "Reproduced the malformed-input failure."
    },
    {
      at_ms: BASE_MS + 24 * minute,
      text: "Added the boundary check and verified focused tests."
    }
  ],
  transcript: [
    {
      role: "user",
      at_ms: BASE_MS + 10 * minute,
      text: "Audit the parser invariant and preserve the original terminology."
    },
    {
      role: "assistant",
      at_ms: BASE_MS + 11 * minute,
      text: pullText,
      pull_requests: [{
        start: pullText.indexOf(pullUrl),
        end: pullText.indexOf(pullUrl) + pullUrl.length,
        text: pullUrl,
        kind: "explicit_url",
        repository: "example-org/dev-widget",
        number: 38,
        url: pullUrl,
        title: "Repair malformed-input handling"
      }]
    },
    {
      role: "tool",
      at_ms: BASE_MS + 16 * minute,
      text: "",
      tools: [{ name: "bash", count: 2 }, { name: "git", count: 1 }]
    },
    {
      role: "agent",
      at_ms: BASE_MS + 20 * minute,
      text: "The coordinator clarified the expected error boundary."
    },
    {
      role: "system",
      at_ms: BASE_MS + 24 * minute,
      text: "Focused tests completed successfully."
    }
  ],
  raw_summary_path: "summaries/phases/phase-a-1.md",
  artifact_ids: ASSOCIATED_ARTIFACT_IDS,
  output_artifact_ids: [OUTPUT_ARTIFACT_ID, OUTPUT_ARTIFACT_ID]
};

function simpleDetail(phase) {
  return {
    phrase: phase.phrase,
    paragraph: phase.paragraph,
    stats: phase.stats,
    work_summary: [],
    transcript: [],
    raw_summary_path: ""
  };
}

const artifactCatalog = {
  schema_version: 1,
  extractor_version: "work-artifacts-v1",
  source_digest: "playwright-fixture",
  artifacts: [
    {
      artifact_id: OUTPUT_ARTIFACT_ID,
      kind: "pull_request",
      locator: pullUrl,
      url: pullUrl,
      label: "example-org/dev-widget PR #38",
      title: "Repair malformed-input handling",
      external_id: "38",
      project_url: "https://github.com/example-org/dev-widget",
      project_slug: "example-org/dev-widget",
      producer_thread_id: "agent-a",
      produced_at_ms: BASE_MS + 12 * minute,
      evidence: [
        {
          evidence_id: "evidence-pr38-produced",
          source_kind: "tool_output",
          source_id: "call-pr38",
          source_line: 73,
          thread_id: "agent-a",
          turn_id: "turn-a-1",
          timestamp_ms: BASE_MS + 12 * minute,
          relation: "produced",
          action: "created_pull_request",
          confidence: "high",
          matched_text: pullUrl,
          extractor: "work-artifacts-v1"
        }
      ]
    },
    {
      artifact_id: REFERENCE_ARTIFACT_ID,
      kind: "issue",
      locator: "https://github.com/example-org/dev-widget/issues/41",
      url: "https://github.com/example-org/dev-widget/issues/41",
      label: "example-org/dev-widget issue #41",
      title: "Harden malformed-input diagnostics",
      external_id: "41",
      project_url: "https://github.com/example-org/dev-widget",
      project_slug: "example-org/dev-widget",
      producer_thread_id: null,
      produced_at_ms: null,
      evidence: [
        {
          evidence_id: "evidence-issue41-reference",
          source_kind: "event_text",
          source_id: "event-issue41",
          source_line: 81,
          thread_id: "agent-a",
          turn_id: "turn-a-1",
          timestamp_ms: BASE_MS + 14 * minute,
          relation: "referenced",
          action: "mentioned",
          confidence: "high",
          matched_text: "https://github.com/example-org/dev-widget/issues/41",
          extractor: "work-artifacts-v1"
        }
      ]
    },
    {
      artifact_id: UNSAFE_ARTIFACT_ID,
      kind: "url",
      locator: "unsafe-fixture",
      url: "javascript:alert('artifact fixture')",
      label: "Unsafe transcript link",
      title: null,
      external_id: null,
      project_url: null,
      project_slug: null,
      producer_thread_id: null,
      produced_at_ms: null,
      evidence: [
        {
          evidence_id: "evidence-unsafe-reference",
          source_kind: "event_text",
          source_id: "event-unsafe",
          source_line: 82,
          thread_id: "agent-a",
          turn_id: "turn-a-1",
          timestamp_ms: BASE_MS + 15 * minute,
          relation: "referenced",
          action: "mentioned",
          confidence: "medium",
          matched_text: "unsafe fixture",
          extractor: "work-artifacts-v1"
        }
      ]
    }
  ],
  projects: [
    {
      project_id: "project-dev-widget",
      host: "github.com",
      slug: "example-org/dev-widget",
      url: "https://github.com/example-org/dev-widget",
      evidence_ids: ["evidence-pr38-produced"]
    }
  ]
};

const virtualFiles = new Map([
  ["/data/timeline.json", {
    contentType: "application/json; charset=utf-8",
    body: JSON.stringify(timeline)
  }],
  ["/data/artifacts.json", {
    contentType: "application/json; charset=utf-8",
    body: JSON.stringify(artifactCatalog)
  }],
  ["/details/phase-root.json", {
    contentType: "application/json; charset=utf-8",
    body: JSON.stringify(simpleDetail(phases[0]))
  }],
  ["/details/phase-a-1.json", {
    contentType: "application/json; charset=utf-8",
    body: JSON.stringify(phaseADetail)
  }],
  ["/details/phase-a-2.json", {
    contentType: "application/json; charset=utf-8",
    body: JSON.stringify(simpleDetail(phases[2]))
  }],
  ["/details/phase-b-1.json", {
    contentType: "application/json; charset=utf-8",
    body: JSON.stringify(simpleDetail(phases[3]))
  }],
  ["/details/phase-c-1.json", {
    contentType: "application/json; charset=utf-8",
    body: JSON.stringify(simpleDetail(phases[4]))
  }],
  ["/summaries/phases/phase-a-1.md", {
    contentType: "text/markdown; charset=utf-8",
    body: "# Parser audit\n\nThe malformed-input boundary is now covered.\n"
  }],
  ["/summaries/daily/2026-03-09.md", {
    contentType: "text/markdown; charset=utf-8",
    body: "# March 9 technical summary\n\nThe team repaired the malformed-input parser boundary.\n"
  }],
  ["/summaries/daily/2026-03-09-plain-language.md", {
    contentType: "text/markdown; charset=utf-8",
    body: "# March 9 plain-language summary\n\nThe parser now safely rejects malformed-input records.\n"
  }],
  ["/summaries/daily/2026-03-08.md", {
    contentType: "text/markdown; charset=utf-8",
    body: "# March 8 technical summary\n\nThe team began tracing the malformed-input parser boundary.\n"
  }],
  ["/summaries/daily/2026-03-08-plain-language.md", {
    contentType: "text/markdown; charset=utf-8",
    body: "# March 8 plain-language summary\n\nThe team began investigating malformed-input parser data.\n"
  }],
  ["/summaries/weekly/2026-W11.md", {
    contentType: "text/markdown; charset=utf-8",
    body: "# Week 11\n\nParser and CI work advanced together.\n"
  }],
  ["/summaries/weekly/2026-W11-plain-language.md", {
    contentType: "text/markdown; charset=utf-8",
    body: "# Week 11 in plain language\n\nThe team made invalid data safer to handle.\n"
  }],
  ["/summaries/glossary/codex-widget-glossary.md", {
    contentType: "text/markdown; charset=utf-8",
    body: "# Project glossary\n\n## malformed-input\n\nData that does not satisfy the parser structure; [unverified](https://attacker.invalid/catalog).\n"
  }]
]);

module.exports = {
  BASE_MS: BASE_MS,
  DATA_START_MS: DATA_START_MS,
  DATA_END_MS: DATA_END_MS,
  AGENT_A_ACTIVITY_START_MS: AGENT_A_ACTIVITY_START_MS,
  AGENT_A_ACTIVITY_END_MS: AGENT_A_ACTIVITY_END_MS,
  AGGREGATE_HOUR_START_MS: AGGREGATE_HOUR_START_MS,
  AGGREGATE_BURST_START_MS: AGGREGATE_BURST_START_MS,
  AGGREGATE_GAP_START_MS: AGGREGATE_GAP_START_MS,
  AGGREGATE_LATER_START_MS: AGGREGATE_LATER_START_MS,
  FIRST_DAY_ACTIVITY_START_MS: FIRST_DAY_ACTIVITY_START_MS,
  PHASE_A_START_MS: BASE_MS + 10 * minute,
  PHASE_A_END_MS: BASE_MS + 25 * minute,
  PHASE_A2_ACTIVITY_START_MS: PHASE_A2_ACTIVITY_START_MS,
  PHASE_A2_ACTIVITY_END_MS: PHASE_A2_ACTIVITY_END_MS,
  ROLLUP_RANGES: ROLLUP_RANGES,
  ROLLUP_EXPECTED_RANGES: ROLLUP_EXPECTED_RANGES,
  AGENT_COUNT: agents.length,
  TIMELINE: timeline,
  virtualFiles: virtualFiles
};
