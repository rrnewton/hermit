"use strict";

const { test, expect } = require("@playwright/test");
const { createHash } = require("crypto");
const {
  AGGREGATE_BURST_START_MS,
  AGGREGATE_GAP_START_MS,
  AGGREGATE_HOUR_START_MS,
  AGGREGATE_LATER_START_MS,
  AGENT_A_ACTIVITY_END_MS,
  AGENT_A_ACTIVITY_START_MS,
  AGENT_COUNT,
  BASE_MS,
  DATA_END_MS,
  DATA_START_MS,
  PHASE_A_START_MS,
  PHASE_A_END_MS,
  PHASE_A2_ACTIVITY_END_MS,
  PHASE_A2_ACTIVITY_START_MS,
  ROLLUP_EXPECTED_RANGES,
  ROLLUP_RANGES,
  TIMELINE
} = require("./fixture-data.cjs");

const phaseSelector = '[data-phase-id="phase-a-1"]';
const secondPhaseSelector = '[data-phase-id="phase-a-2"]';

function contentAddressedJson(value) {
  const body = JSON.stringify(value);
  const bytes = Buffer.byteLength(body, "utf8");
  const sha256 = createHash("sha256").update(body, "utf8").digest("hex");
  return {
    body: body,
    reference: {
      url: "data/timeline-v2/objects/" + sha256 + ".json",
      sha256: sha256,
      bytes: bytes
    }
  };
}

async function requireContract(locator, reason) {
  test.skip((await locator.count()) === 0, reason);
  return locator.first();
}

async function readView(timeline) {
  const start = Number(await timeline.getAttribute("data-view-start-ms"));
  const end = Number(await timeline.getAttribute("data-view-end-ms"));
  expect(Number.isFinite(start)).toBeTruthy();
  expect(Number.isFinite(end)).toBeTruthy();
  expect(end).toBeGreaterThan(start);
  return { start: start, end: end, span: end - start };
}

async function waitForViewChange(timeline, previousStart, previousEnd) {
  await expect.poll(async function () {
    const current = await readView(timeline);
    return current.start !== previousStart || current.end !== previousEnd;
  }).toBeTruthy();
  return readView(timeline);
}

function statValue(page, label) {
  return page.locator("#stats-values .stat")
    .filter({ hasText: label })
    .locator(".stat-value");
}

function singleDaySchema2Fixture(globalDigest, detailDigest) {
  const dayStart = Date.UTC(2026, 2, 9);
  const globalUrl = "data/timeline-v2/objects/" + globalDigest + ".json";
  const detailUrl = "data/timeline-v2/objects/" + detailDigest + ".json";
  const globalData = JSON.parse(JSON.stringify(TIMELINE));
  [
    "generated_at",
    "source_digest",
    "display_timezone",
    "display_timezone_source",
    "range",
    "teams",
    "activity_bins",
    "phases",
    "edges",
    "events"
  ].forEach(function (field) { delete globalData[field]; });
  globalData.schema_version = 2;
  globalData.kind = "timeline-global";
  globalData.source_digest = TIMELINE.source_digest;
  globalData.edges = TIMELINE.edges.filter(function (edge) {
    return edge.kind === "spawn" || edge.kind === "continuation" || edge.kind === "result";
  });
  return {
    globalDigest: globalDigest,
    detailDigest: detailDigest,
    bootstrap: {
      schema_version: 2,
      kind: "timeline-bootstrap",
      generated_at: TIMELINE.generated_at,
      source_digest: TIMELINE.source_digest,
      display_timezone: TIMELINE.display_timezone,
      display_timezone_source: TIMELINE.display_timezone_source,
      range: {
        start_ms: BASE_MS - 36 * 60 * 60 * 1000,
        end_ms: BASE_MS + 36 * 60 * 60 * 1000
      },
      teams: TIMELINE.teams,
      activity_bins: TIMELINE.activity_bins,
      global: { url: globalUrl, sha256: globalDigest },
      detail_shards: [{
        kind: "utc-day",
        day: "2026-03-09",
        start_ms: dayStart,
        end_ms: dayStart + 24 * 60 * 60 * 1000,
        url: detailUrl,
        sha256: detailDigest
      }]
    },
    globalData: globalData,
    detail: {
      schema_version: 2,
      kind: "timeline-detail-day",
      range: {
        start_ms: dayStart,
        end_ms: dayStart + 24 * 60 * 60 * 1000
      },
      phases: TIMELINE.phases,
      edges: TIMELINE.edges.filter(function (edge) { return edge.kind === "message"; }),
      events: TIMELINE.events
    }
  };
}

async function routeSingleDaySchema2Fixture(page, fixture, detailHandler) {
  const storedGlobal = contentAddressedJson(fixture.globalData);
  const storedDetail = contentAddressedJson(fixture.detail);
  fixture.globalDigest = storedGlobal.reference.sha256;
  fixture.detailDigest = storedDetail.reference.sha256;
  fixture.bootstrap.global = storedGlobal.reference;
  fixture.bootstrap.detail_shards[0] = Object.assign(
    {},
    fixture.bootstrap.detail_shards[0],
    storedDetail.reference
  );
  await page.route("**/data/timeline-v2.json", function (route) {
    return route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.bootstrap)
    });
  });
  await page.route("**/" + fixture.globalDigest + ".json", function (route) {
    return route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: storedGlobal.body
    });
  });
  await page.route("**/" + fixture.detailDigest + ".json", detailHandler);
  await page.route("**/data/timeline.json", function (route) {
    return route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(TIMELINE)
    });
  });
}

async function zoomOutToLod(page, timeline, target) {
  const axis = page.locator("#time-axis");
  const box = await axis.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  for (let attempt = 0; attempt < 24; attempt += 1) {
    if (await timeline.getAttribute("data-render-lod") === target) {
      return;
    }
    const revision = Number(await timeline.getAttribute("data-render-revision"));
    await page.mouse.wheel(0, 500);
    await expect.poll(async function () {
      return Number(await timeline.getAttribute("data-render-revision"));
    }).toBeGreaterThan(revision);
  }
  expect(await timeline.getAttribute("data-render-lod")).toBe(target);
}

test.beforeEach(async function ({ page }) {
  await page.goto("/");
  await expect(page.locator("#dataset-meta")).not.toContainText("Loading timeline");
  await expect(page.locator("#load-error")).toBeHidden();
  await expect(page.locator(".phase-group" + phaseSelector)).toBeVisible();
});

test("schema 2 keeps immutable legacy globals without a source digest readable", async function ({
  page
}) {
  const fixture = singleDaySchema2Fixture("7".repeat(64), "8".repeat(64));
  delete fixture.globalData.source_digest;
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });

  await page.reload();
  await expect(page.locator(".timeline-card")).toHaveAttribute(
    "data-timeline-schema-mode",
    "schema2"
  );
  await expect(page.locator("#dataset-meta")).not.toContainText("schema 2 fallback");
});

test("schema 2 rejects a global object from a different source generation", async function ({
  page
}) {
  const fixture = singleDaySchema2Fixture("9".repeat(64), "a".repeat(64));
  fixture.globalData.source_digest = "different-generation";
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });

  await page.reload();
  await expect(page.locator(".timeline-card")).toHaveAttribute(
    "data-timeline-schema-mode",
    "schema1-fallback"
  );
  await expect(page.locator("#dataset-meta")).toContainText(
    "global source digest does not match its bootstrap"
  );
});

test("schema 2 rejects global team and range records outside its bootstrap", async function ({
  page
}) {
  const teamFixture = singleDaySchema2Fixture("3".repeat(64), "4".repeat(64));
  teamFixture.globalData.agents[0].team = "stale-team";
  await routeSingleDaySchema2Fixture(page, teamFixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(teamFixture.detail)
    });
  });

  await page.reload();
  await expect(page.locator(".timeline-card")).toHaveAttribute(
    "data-timeline-schema-mode",
    "schema1-fallback"
  );
  await expect(page.locator("#dataset-meta")).toContainText(
    "contains a team absent from its bootstrap"
  );

  const rangeFixture = singleDaySchema2Fixture("5".repeat(64), "6".repeat(64));
  rangeFixture.bootstrap.range.start_ms = rangeFixture.globalData.agents[0].start_ms + 1;
  await routeSingleDaySchema2Fixture(page, rangeFixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(rangeFixture.detail)
    });
  });

  await page.reload();
  await expect(page.locator(".timeline-card")).toHaveAttribute(
    "data-timeline-schema-mode",
    "schema1-fallback"
  );
  await expect(page.locator("#dataset-meta")).toContainText(
    "global agent range is outside its bootstrap"
  );
});

test("schema 2 loads visible detail shards once and expands search on demand", async function ({ page }) {
  const firstDayStart = Date.UTC(2026, 2, 9);
  const remoteDayStart = Date.UTC(2026, 2, 19);
  const finalDayStart = Date.UTC(2026, 2, 29);
  const expandedEnd = Date.UTC(2026, 2, 30);
  const globalData = JSON.parse(JSON.stringify(TIMELINE));
  [
    "generated_at",
    "source_digest",
    "display_timezone",
    "display_timezone_source",
    "range",
    "teams",
    "activity_bins",
    "phases",
    "edges",
    "events"
  ].forEach(function (field) { delete globalData[field]; });
  globalData.schema_version = 2;
  globalData.kind = "timeline-global";
  globalData.source_digest = TIMELINE.source_digest;
  globalData.stats = {
    user_prompts: 41,
    agent_responses: 42,
    inter_agent_messages: 43,
    external_messages: 44,
    tool_calls: 45,
    active_agents: 4
  };
  globalData.edges = TIMELINE.edges.filter(function (edge) {
    return edge.kind === "spawn" || edge.kind === "continuation" || edge.kind === "result";
  });
  const remotePhase = {
    id: "phase-remote-search",
    agent_id: "agent-a",
    start_ms: remoteDayStart + 60 * 60 * 1000,
    end_ms: remoteDayStart + 2 * 60 * 60 * 1000,
    phrase: "Remote search needle",
    paragraph: "This phase exists only in the shard outside the visible range.",
    detail_path: "details/phase-a-1.json",
    stats: {},
    states: []
  };
  const bootstrap = {
    schema_version: 2,
    kind: "timeline-bootstrap",
    generated_at: TIMELINE.generated_at,
    source_digest: TIMELINE.source_digest,
    display_timezone: TIMELINE.display_timezone,
    display_timezone_source: TIMELINE.display_timezone_source,
    range: { start_ms: DATA_START_MS, end_ms: expandedEnd },
    teams: TIMELINE.teams,
    activity_bins: TIMELINE.activity_bins,
    global: {},
    detail_shards: [
      {
        kind: "utc-day",
        day: "2026-03-09",
        start_ms: firstDayStart,
        end_ms: firstDayStart + 24 * 60 * 60 * 1000,
      },
      {
        kind: "utc-day",
        day: "2026-03-19",
        start_ms: remoteDayStart,
        end_ms: remoteDayStart + 24 * 60 * 60 * 1000,
      },
      {
        kind: "utc-day",
        day: "2026-03-29",
        start_ms: finalDayStart,
        end_ms: expandedEnd,
      }
    ]
  };
  const detail = {
    schema_version: 2,
    kind: "timeline-detail-day",
    range: {
      start_ms: firstDayStart,
      end_ms: firstDayStart + 24 * 60 * 60 * 1000
    },
    phases: TIMELINE.phases,
    edges: TIMELINE.edges.filter(function (edge) { return edge.kind === "message"; }),
    events: TIMELINE.events
  };
  const remoteDetail = {
    schema_version: 2,
    kind: "timeline-detail-day",
    range: {
      start_ms: remoteDayStart,
      end_ms: remoteDayStart + 24 * 60 * 60 * 1000
    },
    phases: [remotePhase],
    edges: [],
    events: []
  };
  const finalDetail = {
    schema_version: 2,
    kind: "timeline-detail-day",
    range: {
      start_ms: finalDayStart,
      end_ms: expandedEnd
    },
    phases: [],
    edges: [],
    events: [{
      agent_id: "agent-a",
      at_ms: expandedEnd,
      kind: "user_prompt"
    }]
  };
  const storedGlobal = contentAddressedJson(globalData);
  const storedFirst = contentAddressedJson(detail);
  const storedRemote = contentAddressedJson(remoteDetail);
  const storedFinal = contentAddressedJson(finalDetail);
  Object.assign(bootstrap.global, storedGlobal.reference);
  Object.assign(bootstrap.detail_shards[0], storedFirst.reference);
  Object.assign(bootstrap.detail_shards[1], storedRemote.reference);
  Object.assign(bootstrap.detail_shards[2], storedFinal.reference);
  const globalDigest = storedGlobal.reference.sha256;
  const firstDigest = storedFirst.reference.sha256;
  const remoteDigest = storedRemote.reference.sha256;
  const finalDigest = storedFinal.reference.sha256;
  const requests = new Map();
  async function fulfillJson(route, name, value) {
    requests.set(name, (requests.get(name) || 0) + 1);
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(value)
    });
  }
  await page.route("**/data/timeline-v2.json", function (route) {
    return fulfillJson(route, "bootstrap", bootstrap);
  });
  await page.route("**/" + globalDigest + ".json", function (route) {
    return fulfillJson(route, "global", globalData);
  });
  await page.route("**/" + firstDigest + ".json", function (route) {
    return fulfillJson(route, "first", detail);
  });
  await page.route("**/" + remoteDigest + ".json", function (route) {
    return fulfillJson(route, "remote", remoteDetail);
  });
  await page.route("**/" + finalDigest + ".json", function (route) {
    return fulfillJson(route, "final", finalDetail);
  });
  await page.route("**/data/timeline.json", function (route) {
    return fulfillJson(route, "schema1", TIMELINE);
  });

  await page.reload();
  const card = page.locator(".timeline-card");
  const timeline = page.getByTestId("timeline");
  await expect(page.locator("#dataset-meta")).toContainText("schema 2");
  await expect(card).toHaveAttribute("data-timeline-schema-mode", "schema2");
  await expect(timeline).toHaveAttribute("data-render-lod", "aggregate");
  await expect(card).toHaveAttribute("data-loaded-shard-count", "0");
  await expect(page.locator("#stats-range-label")).not.toContainText(
    "event counts unavailable"
  );
  await expect(statValue(page, "User prompts")).toHaveText("41");
  await expect(statValue(page, "Agent responses")).toHaveText("42");
  await expect(statValue(page, "Inter-agent msgs")).toHaveText("43");
  await expect(statValue(page, "External msgs")).toHaveText("44");
  await expect(statValue(page, "Tool calls")).toHaveText("45");
  expect(requests.get("bootstrap")).toBe(1);
  expect(requests.get("global")).toBe(1);
  expect(requests.get("first") || 0).toBe(0);
  expect(requests.get("remote") || 0).toBe(0);
  expect(requests.get("final") || 0).toBe(0);
  expect(requests.get("schema1") || 0).toBe(0);

  await page.locator(
    '.activity-bin-group[data-activity-resolution="daily"]'
  ).first().dispatchEvent("dblclick", { detail: 2 });
  await expect(timeline).toHaveAttribute("data-render-lod", "lifetime");
  await expect(card).toHaveAttribute("data-loaded-shard-count", "0");
  await expect(page.locator('.edge-group[data-edge-id="spawn-a"]')).toHaveCount(0);
  await expect(page.locator('.edge-group[data-edge-id="result-a"]')).toHaveCount(0);

  await timeline.press("ArrowLeft");
  await expect(timeline).toHaveAttribute("data-render-lod", "lifetime");
  const agentLifetime = page.locator('.agent-lifetime-group[data-agent-id="agent-a"]');
  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(page.getByTestId("modal")).toBeVisible();
  await expect(page.getByRole("tab", { name: "Work Phases" })).toHaveAttribute(
    "aria-selected",
    "true"
  );
  await expect(
    page.getByTestId("modal").locator('.agent-lifetime-phase[data-phase-id="phase-a-1"]')
  ).toBeVisible();
  await expect(card).toHaveAttribute("data-loaded-shard-count", "1");
  expect(requests.get("first")).toBe(1);
  expect(requests.get("remote") || 0).toBe(0);
  expect(requests.get("final") || 0).toBe(0);
  await page.locator("#modal-close").click();

  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(
    page.getByTestId("modal").locator('.agent-lifetime-phase[data-phase-id="phase-a-2"]')
  ).toBeVisible();
  expect(requests.get("first")).toBe(1);
  await page.locator("#modal-close").click();

  await page.getByTestId("fit").click();
  await expect(timeline).toHaveAttribute("data-render-lod", "aggregate");
  await expect(page.locator("#stats-range-label")).not.toContainText(
    "event counts unavailable"
  );
  await expect(statValue(page, "User prompts")).toHaveText("41");
  await expect(statValue(page, "Agent responses")).toHaveText("42");
  await expect(statValue(page, "Inter-agent msgs")).toHaveText("43");
  await expect(statValue(page, "External msgs")).toHaveText("44");
  await expect(statValue(page, "Tool calls")).toHaveText("45");
  expect(requests.get("first")).toBe(1);
  expect(requests.get("remote") || 0).toBe(0);
  expect(requests.get("final") || 0).toBe(0);

  const firstRollup = ROLLUP_RANGES[0];
  const marker = page.locator(
    '.rollup-marker.rollup-daily[data-start-ms="' + firstRollup.start_ms + '"]'
  );
  await marker.dispatchEvent("contextmenu", {
    button: 2,
    clientX: 240,
    clientY: 140
  });
  await page.getByTestId("timeline-context-menu")
    .getByRole("menuitem", { name: "Zoom to day", exact: true }).click();
  await expect(card).toHaveAttribute("data-loaded-shard-count", "1");
  await expect(page.locator(".phase-group" + phaseSelector)).toBeVisible();
  expect(requests.get("first")).toBe(1);
  expect(requests.get("remote") || 0).toBe(0);

  await page.getByTestId("fit").click();
  await marker.dispatchEvent("contextmenu", {
    button: 2,
    clientX: 240,
    clientY: 140
  });
  await page.getByTestId("timeline-context-menu")
    .getByRole("menuitem", { name: "Zoom to day", exact: true }).click();
  await expect(card).toHaveAttribute("data-loaded-shard-count", "1");
  expect(requests.get("first")).toBe(1);

  await page.getByTestId("fit").click();
  await page.getByTestId("search").fill("remote search needle");
  await expect(card).toHaveAttribute("data-search-shard-state", "ready");
  await expect(card).toHaveAttribute("data-loaded-shard-count", "3");
  expect(requests.get("first")).toBe(1);
  expect(requests.get("remote")).toBe(1);
  expect(requests.get("final")).toBe(1);
  await page.getByTestId("search").fill("");
  await page.getByTestId("fit").click();
  await expect(statValue(page, "User prompts")).toHaveText("1");
  await page.getByTestId("search").fill("remote search needle");
  await expect(card).toHaveAttribute("data-search-shard-state", "ready");
  expect(requests.get("remote")).toBe(1);
  expect(requests.get("final")).toBe(1);
  expect(requests.get("schema1") || 0).toBe(0);
});

test("transcript search uses search shards and opens safe linked message context", async function ({ page }) {
  const fixture = singleDaySchema2Fixture("6".repeat(64), "7".repeat(64));
  const searchDigest = "8".repeat(64);
  const searchUrl = "data/timeline-v2/objects/" + searchDigest + ".json";
  const day = fixture.bootstrap.detail_shards[0];
  fixture.bootstrap.search = {
    schema_version: 1,
    strategy: "transcript-message-shards",
    shards: [{
      kind: "utc-day",
      day: day.day,
      team: "codex-widget",
      start_ms: day.start_ms,
      end_ms: day.end_ms,
      url: searchUrl,
      sha256: searchDigest,
      counts: {
        records: 7,
        prompts: 2,
        responses: 1,
        inter_agent: 1,
        tools: 0
      }
    }]
  };
  const promptARef = "message:codex-widget::prompt-a";
  const promptBRef = "message:codex-widget::prompt-b";
  const responseARef = "message:codex-widget::response-a";
  const responseBRef = "message:codex-widget::response-b";
  function record(overrides) {
    return Object.assign({
      schema_version: 1,
      record_type: "response",
      role: "assistant",
      team: "codex-widget",
      agent_id: "agent-a",
      agent_ref: "agent:codex-widget::agent-a",
      agent_path: "/root/agent-a",
      event_id: "event",
      turn_id: "turn",
      at_ms: BASE_MS + 12 * 60 * 1000,
      text: "B3",
      author_kind: "agent",
      ingress_kind: "assistant",
      prompt_ref: promptARef,
      prompt_author_kind: "owner_human",
      content_fidelity: "verbatim"
    }, overrides);
  }
  const searchShard = {
    schema_version: 1,
    kind: "timeline-search-day",
    team: "codex-widget",
    range: { start_ms: day.start_ms, end_ms: day.end_ms },
    records: [
      record({
        ref: promptARef,
        record_type: "prompt",
        role: "user",
        event_id: "prompt-a",
        at_ms: BASE_MS + 11 * 60 * 1000,
        text: "How mature is the DBI backend?",
        author_kind: "owner_human",
        ingress_kind: "owner",
        prompt_ref: promptARef
      }),
      record({ ref: responseARef, event_id: "response-a" }),
      record({
        ref: promptBRef,
        record_type: "prompt",
        role: "user",
        agent_id: "agent-b",
        agent_ref: "agent:codex-widget::agent-b",
        agent_path: "/root/agent-b",
        event_id: "prompt-b",
        at_ms: BASE_MS + 31 * 60 * 1000,
        text: "How mature is the KVM backend?",
        author_kind: "owner_human",
        ingress_kind: "owner",
        prompt_ref: promptBRef
      }),
      record({
        ref: responseBRef,
        record_type: "inter_agent_response",
        role: "agent",
        agent_id: "agent-b",
        agent_ref: "agent:codex-widget::agent-b",
        agent_path: "/root/agent-b",
        event_id: "response-b",
        at_ms: BASE_MS + 34 * 60 * 1000,
        text: "KVM reached B3. <img src=x onerror=window.__unsafeSearch=1>",
        prompt_ref: promptBRef
      }),
      record({
        ref: "message:codex-widget::hash-only",
        agent_id: "agent-c",
        agent_ref: "agent:codex-widget::agent-c",
        agent_path: "/root/agent-c",
        event_id: "hash-only",
        at_ms: BASE_MS + 40 * 60 * 1000,
        text: "Recorded artifact hash 12ab3cdef456.",
        prompt_ref: null,
        prompt_author_kind: null
      }),
      record({
        ref: "message:codex-widget::system-checkpoint",
        record_type: "system",
        role: "system",
        event_id: "system-checkpoint",
        at_ms: BASE_MS + 41 * 60 * 1000,
        text: "Sentinel system checkpoint.",
        prompt_ref: null,
        prompt_author_kind: null
      }),
      record({
        ref: "message:codex-widget::lifecycle-event",
        record_type: "subagent_started",
        role: "event",
        event_id: "lifecycle-event",
        at_ms: BASE_MS + 42 * 60 * 1000,
        text: "Sentinel lifecycle event.",
        prompt_ref: null,
        prompt_author_kind: null
      })
    ]
  };
  const storedSearch = contentAddressedJson(searchShard);
  Object.assign(fixture.bootstrap.search.shards[0], storedSearch.reference);
  let detailRequests = 0;
  let searchRequests = 0;
  const consoleErrors = [];
  const failedRequests = [];
  page.on("console", function (message) {
    if (message.type() === "error") {
      consoleErrors.push(message.text());
    }
  });
  page.on("requestfailed", function (request) {
    failedRequests.push(request.url());
  });
  // `requestfailed` fires only for NETWORK-level failures, so an HTTP 404 never reaches it --
  // it surfaces as a console error whose text carries no URL. Recording status and URL here is
  // what lets the assertion below name which resource failed instead of reporting that some
  // unnamed one did.
  const httpErrors = [];
  page.on("response", function (response) {
    if (response.status() >= 400) {
      httpErrors.push(response.status() + " " + response.url());
    }
  });
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    detailRequests += 1;
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  await page.route("**/" + storedSearch.reference.sha256 + ".json", async function (route) {
    searchRequests += 1;
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: storedSearch.body
    });
  });

  await page.reload();
  // Playwright reports the superseded page's in-flight bootstrap request as aborted
  // when this test replaces the already-loaded beforeEach page with its routed fixture.
  failedRequests.length = 0;
  const card = page.getByTestId("timeline");
  await page.getByTestId("search-scope").selectOption("agent-responses");
  await page.getByTestId("search").fill("B3");
  await expect(card).toHaveAttribute("data-transcript-search-state", "ready");
  await expect(card).toHaveAttribute("data-loaded-search-shard-count", "1");
  await expect(card).toHaveAttribute("data-transcript-search-result-count", "2");
  expect(searchRequests).toBe(1);
  expect(detailRequests).toBe(0);

  const drawer = page.getByTestId("search-results");
  await expect(drawer).toBeVisible();
  await expect(drawer.locator(".search-results-count")).toHaveText("2 matches");
  await expect(drawer).not.toContainText("12ab3cdef456");
  await expect(page.locator('.agent-lifetime-group[data-agent-id="agent-a"]')).toHaveClass(
    /is-search-match/
  );
  await expect(page.locator('.agent-lifetime-group[data-agent-id="agent-b"]')).toHaveClass(
    /is-search-match/
  );
  await expect(page.locator('.agent-lifetime-group[data-agent-id="agent-c"]')).toHaveCount(0);

  await expect(drawer.locator(".search-result").first()).toHaveAttribute(
    "data-message-ref",
    responseARef
  );
  await page.getByTestId("search-sort").selectOption("newest");
  const newest = drawer.locator('.search-result[data-message-ref="' + responseBRef + '"]');
  await expect(drawer.locator(".search-result").first()).toHaveAttribute(
    "data-message-ref",
    responseBRef
  );
  await newest.getByTestId("search-result-main").click();
  await expect(card).toHaveAttribute("data-selected-agent-id", "agent-b");
  await expect(card).toHaveAttribute("data-selected-phase-id", "phase-b-1");
  await expect(newest).toHaveClass(/is-active/);
  expect(detailRequests).toBe(1);

  await newest.getByRole("button", { name: "Open" }).click();
  const modal = page.getByTestId("modal");
  await expect(modal).toBeVisible();
  const exactMessage = modal.locator(
    '.search-message-card[data-message-ref="' + responseBRef + '"]'
  );
  await expect(exactMessage).toContainText("KVM reached B3");
  await expect(exactMessage).toContainText("<img src=x onerror=window.__unsafeSearch=1>");
  await expect(modal.locator("img")).toHaveCount(0);
  expect(await page.evaluate(function () { return window.__unsafeSearch; })).toBeUndefined();

  await page.getByRole("tab", { name: "Prompt & responses" }).click();
  await expect(modal.locator(
    '.search-message-card[data-message-ref="' + promptBRef + '"]'
  )).toContainText("How mature is the KVM backend?");
  await expect(modal.locator(
    '.search-message-card[data-message-ref="' + responseBRef + '"]'
  )).toContainText("KVM reached B3");
  await page.locator("#modal-close").click();

  await page.getByTestId("search-scope").selectOption("all-transcript");
  await page.getByTestId("search").fill("sentinel");
  await expect(card).toHaveAttribute("data-transcript-search-result-count", "2");
  await expect(drawer.locator(".search-result-role")).toHaveText(["event", "system"]);
  // This fixture deliberately serves no `data/timeline-v3.json` so the suite exercises the
  // schema-2 fallback an older archive is in (see README). The page handles that 404 by design,
  // but Chromium still logs it, so asserting an empty console asserted against the fixture's own
  // premise -- and did so with a message naming no URL, which is why it stayed unexplained.
  //
  // Named exactly, so any OTHER 404 is still a failure rather than being absorbed by a blanket
  // allowance.
  expect(httpErrors).toEqual([
    "404 " + new URL("data/timeline-v3.json", page.url()).href
  ]);
  expect(consoleErrors.filter(function (text) {
    return !text.startsWith("Failed to load resource:");
  })).toEqual([]);
  expect(failedRequests).toEqual([]);
});

test("linked transcript context loads across Bloom-pruned day shards", async function ({ page }) {
  const fixture = singleDaySchema2Fixture("0".repeat(64), "1".repeat(64));
  const dayMs = 24 * 60 * 60 * 1000;
  const firstStart = fixture.bootstrap.detail_shards[0].start_ms;
  const secondStart = firstStart + dayMs;
  const promptRef = "message:codex-widget::cross-day-prompt";
  const responseRef = "message:codex-widget::cross-day-response";
  const promptBRef = "message:codex-widget::same-day-prompt";
  const responseBRef = "message:codex-widget::same-day-response";
  const promptAt = firstStart + 23 * 60 * 60 * 1000;
  const responseAt = secondStart + 60 * 60 * 1000;
  const promptBAt = secondStart + 30 * 60 * 1000;
  const responseBAt = secondStart + 2 * 60 * 60 * 1000;
  const promptDigest = "2".repeat(64);
  const responseDigest = "3".repeat(64);
  const promptLinksDigest = "4".repeat(64);
  const responseLinksDigest = "5".repeat(64);
  function reference(digest) {
    return {
      url: "data/timeline-v2/objects/" + digest + ".json",
      sha256: digest
    };
  }
  function catalogEntry(day, start, searchDigest, linksDigest, bloom, counts) {
    return Object.assign({
      kind: "utc-day",
      day: day,
      team: "codex-widget",
      start_ms: start,
      end_ms: start + dayMs,
      counts: Object.assign({ records: 1 }, counts),
      linkage: Object.assign(reference(linksDigest), {
        counts: {
          prompts: counts.prompts || 0,
          responses: counts.responses || 0
        }
      }),
      trigram_bloom: bloom
    }, reference(searchDigest));
  }
  fixture.bootstrap.search = {
    schema_version: 1,
    strategy: "transcript-message-shards",
    shards: [
      catalogEntry("2026-03-09", firstStart, promptDigest, promptLinksDigest, {
        algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
        bit_count: 128,
        hash_count: 7,
        bits_base64: "CpJJrKcNEMHROrAlwPydPQ==",
        trigram_count: 10
      }, { prompts: 1, responses: 0 }),
      catalogEntry("2026-03-10", secondStart, responseDigest, responseLinksDigest, {
        algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
        bit_count: 128,
        hash_count: 7,
        bits_base64: "VIrlptEOBb6e2UEwG9lmrQ==",
        trigram_count: 11
      }, { records: 3, prompts: 1, responses: 2 })
    ]
  };
  function searchDay(start, records) {
    return {
      schema_version: 1,
      kind: "timeline-search-day",
      source_digest: TIMELINE.source_digest,
      team: "codex-widget",
      range: { start_ms: start, end_ms: start + dayMs },
      records: records
    };
  }
  const promptShard = searchDay(firstStart, [{
    schema_version: 1,
    ref: promptRef,
    record_type: "prompt",
    role: "user",
    team: "codex-widget",
    agent_id: "agent-a",
    agent_ref: "agent:codex-widget::agent-a",
    event_id: "cross-day-prompt",
    at_ms: promptAt,
    text: "Original maturity question",
    author_kind: "owner_human",
    prompt_ref: promptRef,
    prompt_at_ms: promptAt,
    prompt_in_scope: true
  }]);
  const responseShard = searchDay(secondStart, [{
    schema_version: 1,
    ref: promptBRef,
    record_type: "prompt",
    role: "user",
    team: "codex-widget",
    agent_id: "agent-b",
    agent_ref: "agent:codex-widget::agent-b",
    event_id: "same-day-prompt",
    at_ms: promptBAt,
    text: "Second maturity question",
    author_kind: "owner_human",
    prompt_ref: promptBRef,
    prompt_at_ms: promptBAt,
    prompt_in_scope: true
  }, {
    schema_version: 1,
    ref: responseRef,
    record_type: "response",
    role: "assistant",
    team: "codex-widget",
    agent_id: "agent-a",
    agent_ref: "agent:codex-widget::agent-a",
    event_id: "cross-day-response",
    at_ms: responseAt,
    text: "The backend reached B3",
    prompt_ref: promptRef,
    prompt_at_ms: promptAt,
    prompt_in_scope: true
  }, {
    schema_version: 1,
    ref: responseBRef,
    record_type: "response",
    role: "assistant",
    team: "codex-widget",
    agent_id: "agent-b",
    agent_ref: "agent:codex-widget::agent-b",
    event_id: "same-day-response",
    at_ms: responseBAt,
    text: "The backend remains B3",
    prompt_ref: promptBRef,
    prompt_at_ms: promptBAt,
    prompt_in_scope: true
  }]);
  function linksDay(start, prompts, responses) {
    return {
      schema_version: 1,
      kind: "timeline-search-links-day",
      source_digest: TIMELINE.source_digest,
      team: "codex-widget",
      range: { start_ms: start, end_ms: start + dayMs },
      prompts: prompts,
      responses: responses
    };
  }
  const promptLinks = linksDay(firstStart, [{
    ref: promptRef,
    excerpt: "Original maturity question"
  }], []);
  const responseLinks = linksDay(secondStart, [{
    ref: promptBRef,
    excerpt: "Second maturity question"
  }], [{
    ref: responseRef,
    prompt_ref: promptRef,
    at_ms: responseAt,
    agent_ref: "agent:codex-widget::agent-a"
  }, {
    ref: responseBRef,
    prompt_ref: promptBRef,
    at_ms: responseBAt,
    agent_ref: "agent:codex-widget::agent-b"
  }]);
  const storedPrompt = contentAddressedJson(promptShard);
  const storedResponse = contentAddressedJson(responseShard);
  const storedPromptLinks = contentAddressedJson(promptLinks);
  const storedResponseLinks = contentAddressedJson(responseLinks);
  Object.assign(fixture.bootstrap.search.shards[0], storedPrompt.reference);
  Object.assign(
    fixture.bootstrap.search.shards[0].linkage,
    storedPromptLinks.reference
  );
  Object.assign(fixture.bootstrap.search.shards[1], storedResponse.reference);
  Object.assign(
    fixture.bootstrap.search.shards[1].linkage,
    storedResponseLinks.reference
  );
  const requests = new Map();
  let releasePrompt = function () {};
  const promptGate = new Promise(function (resolve) {
    releasePrompt = resolve;
  });
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  await Promise.all([
    [storedPrompt, "prompt"],
    [storedResponse, "response"],
    [storedPromptLinks, "prompt-links"],
    [storedResponseLinks, "response-links"]
  ].map(function (entry) {
    return page.route("**/" + entry[0].reference.sha256 + ".json", async function (route) {
      requests.set(entry[1], (requests.get(entry[1]) || 0) + 1);
      if (entry[1] === "prompt") {
        await promptGate;
      }
      await route.fulfill({
        contentType: "application/json; charset=utf-8",
        body: entry[0].body
      });
    });
  }));

  await page.reload();
  await page.getByTestId("search-scope").selectOption("agent-responses");
  await page.getByTestId("search").fill("backend");
  const card = page.getByTestId("timeline");
  await expect(card).toHaveAttribute("data-transcript-search-state", "ready");
  expect(requests.get("prompt") || 0).toBe(0);
  expect(requests.get("response")).toBe(1);
  expect(requests.get("prompt-links")).toBe(1);
  expect(requests.get("response-links")).toBe(1);

  const resultA = page.locator(
    '.search-result[data-message-ref="' + responseRef + '"]'
  );
  const resultB = page.locator(
    '.search-result[data-message-ref="' + responseBRef + '"]'
  );
  await resultA.getByRole("button", { name: "Open" }).click();
  await expect.poll(function () { return requests.get("prompt") || 0; }).toBe(1);
  await resultB.getByRole("button", { name: "Open" }).click();
  await expect(page.getByTestId("modal")).toBeVisible();
  await expect(page.getByTestId("modal")).toContainText("The backend remains B3");
  releasePrompt();
  await page.waitForTimeout(50);
  await expect(page.getByTestId("modal")).toContainText("The backend remains B3");
  await expect(page.getByTestId("modal")).not.toContainText("The backend reached B3");
  await page.locator("#modal-close").click();

  await resultA.getByRole("button", { name: "Open" }).click();
  await expect(page.getByTestId("modal")).toBeVisible();
  await expect(page.getByTestId("modal")).toContainText("The backend reached B3");
  await page.getByRole("tab", { name: "Prompt & responses" }).click();
  await expect(page.getByTestId("modal")).toContainText("Original maturity question");
  await expect(page.getByTestId("modal")).toContainText("The backend reached B3");
});

test("transcript search rejects a shard whose record count disagrees with its catalog", async function ({ page }) {
  const fixture = singleDaySchema2Fixture("9".repeat(64), "a".repeat(64));
  const searchDigest = "b".repeat(64);
  const day = fixture.bootstrap.detail_shards[0];
  fixture.bootstrap.search = {
    schema_version: 1,
    strategy: "transcript-message-shards",
    shards: [{
      kind: "utc-day",
      day: day.day,
      team: "codex-widget",
      start_ms: day.start_ms,
      end_ms: day.end_ms,
      url: "data/timeline-v2/objects/" + searchDigest + ".json",
      sha256: searchDigest,
      counts: { records: 2 }
    }]
  };
  const invalidCountShard = {
    schema_version: 1,
    kind: "timeline-search-day",
    team: "codex-widget",
    range: { start_ms: day.start_ms, end_ms: day.end_ms },
    records: [{
      schema_version: 1,
      ref: "message:codex-widget::one",
      record_type: "response",
      role: "assistant",
      team: "codex-widget",
      agent_id: "agent-a",
      at_ms: BASE_MS + 12 * 60 * 1000,
      text: "Only one record"
    }]
  };
  const storedInvalidCount = contentAddressedJson(invalidCountShard);
  Object.assign(fixture.bootstrap.search.shards[0], storedInvalidCount.reference);
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  await page.route("**/" + storedInvalidCount.reference.sha256 + ".json", async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: storedInvalidCount.body
    });
  });

  await page.reload();
  const card = page.getByTestId("timeline");
  await page.getByTestId("search-scope").selectOption("all-transcript");
  await page.getByTestId("search").fill("record");
  await expect(card).toHaveAttribute("data-transcript-search-state", "error");
  await expect(card).toHaveAttribute("data-loaded-search-shard-count", "0");
  await expect(card).toHaveAttribute("data-transcript-search-result-count", "0");
  await expect(page.getByTestId("search-results")).toContainText(
    "does not match its catalog entry"
  );
});

test("transcript search rejects a shard from a different source generation", async function ({
  page
}) {
  const fixture = singleDaySchema2Fixture("c".repeat(64), "d".repeat(64));
  const searchDigest = "e".repeat(64);
  const day = fixture.bootstrap.detail_shards[0];
  fixture.bootstrap.search = {
    schema_version: 1,
    strategy: "transcript-message-shards",
    shards: [{
      kind: "utc-day",
      day: day.day,
      team: "codex-widget",
      start_ms: day.start_ms,
      end_ms: day.end_ms,
      url: "data/timeline-v2/objects/" + searchDigest + ".json",
      sha256: searchDigest,
      counts: { records: 1 }
    }]
  };
  const staleSearchShard = {
    schema_version: 1,
    kind: "timeline-search-day",
    source_digest: "different-generation",
    team: "codex-widget",
    range: { start_ms: day.start_ms, end_ms: day.end_ms },
    records: [{
      schema_version: 1,
      ref: "message:codex-widget::stale",
      record_type: "response",
      role: "assistant",
      team: "codex-widget",
      agent_id: "agent-a",
      at_ms: BASE_MS + 12 * 60 * 1000,
      text: "Stale B3 response"
    }]
  };
  const storedStaleSearch = contentAddressedJson(staleSearchShard);
  Object.assign(fixture.bootstrap.search.shards[0], storedStaleSearch.reference);
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  await page.route("**/" + storedStaleSearch.reference.sha256 + ".json", async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: storedStaleSearch.body
    });
  });

  await page.reload();
  const card = page.getByTestId("timeline");
  await page.getByTestId("search-scope").selectOption("all-transcript");
  await page.getByTestId("search").fill("B3");
  await expect(card).toHaveAttribute("data-transcript-search-state", "error");
  await expect(card).toHaveAttribute("data-loaded-search-shard-count", "0");
  await expect(page.getByTestId("search-results")).toContainText(
    "source digest does not match the timeline generation"
  );
});

test("transcript search verifies content-addressed shard bytes", async function ({ page }) {
  const fixture = singleDaySchema2Fixture("7".repeat(64), "8".repeat(64));
  const day = fixture.bootstrap.detail_shards[0];
  const expectedShard = {
    schema_version: 1,
    kind: "timeline-search-day",
    source_digest: TIMELINE.source_digest,
    team: "codex-widget",
    range: { start_ms: day.start_ms, end_ms: day.end_ms },
    records: [{
      schema_version: 1,
      ref: "message:codex-widget::verified",
      record_type: "response",
      role: "assistant",
      team: "codex-widget",
      agent_id: "agent-a",
      agent_ref: "agent:codex-widget::agent-a",
      at_ms: BASE_MS + 12 * 60 * 1000,
      text: "B3 good"
    }]
  };
  const stored = contentAddressedJson(expectedShard);
  const legacyReference = Object.assign({}, stored.reference);
  delete legacyReference.bytes;
  fixture.bootstrap.search = {
    schema_version: 1,
    strategy: "transcript-message-shards",
    shards: [Object.assign({
      kind: "utc-day",
      day: day.day,
      team: "codex-widget",
      start_ms: day.start_ms,
      end_ms: day.end_ms,
      counts: { records: 1 }
    }, legacyReference)]
  };
  const tamperedShard = JSON.parse(JSON.stringify(expectedShard));
  tamperedShard.records[0].text = "B3 evil";
  expect(Buffer.byteLength(JSON.stringify(tamperedShard), "utf8")).toBe(
    stored.reference.bytes
  );
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  await page.route("**/" + stored.reference.sha256 + ".json", async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(tamperedShard)
    });
  });

  await page.reload();
  const card = page.getByTestId("timeline");
  await page.getByTestId("search-scope").selectOption("all-transcript");
  await page.getByTestId("search").fill("B3");
  await expect(card).toHaveAttribute("data-transcript-search-state", "error");
  await expect(page.getByTestId("search-results")).toContainText("SHA-256 mismatch");
});

test("transcript Bloom filters defer negative shards and load them for a later positive query", async function ({ page }) {
  const fixture = singleDaySchema2Fixture("0".repeat(64), "1".repeat(64));
  const firstDigest = "2".repeat(64);
  const secondDigest = "3".repeat(64);
  const firstStart = fixture.bootstrap.detail_shards[0].start_ms;
  const secondStart = firstStart + 24 * 60 * 60 * 1000;
  function catalogEntry(day, start, digest, bloom) {
    return {
      kind: "utc-day",
      day: day,
      team: "codex-widget",
      start_ms: start,
      end_ms: start + 24 * 60 * 60 * 1000,
      url: "data/timeline-v2/objects/" + digest + ".json",
      sha256: digest,
      counts: { records: 1 },
      trigram_bloom: bloom
    };
  }
  fixture.bootstrap.search = {
    schema_version: 1,
    strategy: "transcript-message-shards",
    shards: [
      catalogEntry("2026-03-09", firstStart, firstDigest, {
        algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
        bit_count: 256,
        hash_count: 7,
        bits_base64: "FIplgEEIAL6PsAAgGYlApEgLw+bQDgUgEEtBEDPQJhk=",
        trigram_count: 14
      }),
      catalogEntry("2026-03-10", secondStart, secondDigest, {
        algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
        bit_count: 128,
        hash_count: 7,
        bits_base64: "CpJJrKcNEMHROrAlwPydPQ==",
        trigram_count: 10
      })
    ]
  };
  function searchShard(start, eventId, at, value) {
    return {
      schema_version: 1,
      kind: "timeline-search-day",
      team: "codex-widget",
      range: { start_ms: start, end_ms: start + 24 * 60 * 60 * 1000 },
      records: [{
        schema_version: 1,
        ref: "message:codex-widget::" + eventId,
        record_type: "response",
        role: "assistant",
        team: "codex-widget",
        agent_id: "agent-a",
        agent_ref: "agent:codex-widget::agent-a",
        event_id: eventId,
        at_ms: at,
        text: value
      }]
    };
  }
  const firstShard = searchShard(
    firstStart,
    "backend",
    BASE_MS + 12 * 60 * 1000,
    "alpha backend B3"
  );
  const secondShard = searchShard(
    secondStart,
    "ptrace",
    secondStart + 60 * 60 * 1000,
    "gamma ptrace"
  );
  const storedFirstSearch = contentAddressedJson(firstShard);
  const storedSecondSearch = contentAddressedJson(secondShard);
  Object.assign(fixture.bootstrap.search.shards[0], storedFirstSearch.reference);
  Object.assign(fixture.bootstrap.search.shards[1], storedSecondSearch.reference);
  let firstRequests = 0;
  let secondRequests = 0;
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  await page.route("**/" + storedFirstSearch.reference.sha256 + ".json", async function (route) {
    firstRequests += 1;
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: storedFirstSearch.body
    });
  });
  await page.route("**/" + storedSecondSearch.reference.sha256 + ".json", async function (route) {
    secondRequests += 1;
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: storedSecondSearch.body
    });
  });

  await page.reload();
  const card = page.getByTestId("timeline");
  const drawer = page.getByTestId("search-results");
  await page.getByTestId("search-scope").selectOption("all-transcript");
  await page.getByTestId("search").pressSequentially("backend B3", { delay: 5 });
  expect(firstRequests).toBe(0);
  expect(secondRequests).toBe(0);
  await expect(card).toHaveAttribute("data-transcript-search-state", "ready");
  await expect(card).toHaveAttribute("data-loaded-search-shard-count", "1");
  await expect(drawer.locator(".search-result")).toHaveCount(1);
  await expect(drawer).toContainText("alpha backend B3");
  expect(firstRequests).toBe(1);
  expect(secondRequests).toBe(0);

  await page.getByTestId("search").fill("ptrace");
  await expect(card).toHaveAttribute("data-loaded-search-shard-count", "2");
  await expect(drawer.locator(".search-result")).toHaveCount(1);
  await expect(drawer).toContainText("gamma ptrace");
  expect(firstRequests).toBe(1);
  expect(secondRequests).toBe(1);
});

test("superseded searches share one global shard-load limit", async function ({ page }) {
  const fixture = singleDaySchema2Fixture("6".repeat(64), "7".repeat(64));
  const dayMs = 24 * 60 * 60 * 1000;
  const baseStart = fixture.bootstrap.detail_shards[0].start_ms;
  const storedObjects = [];
  fixture.bootstrap.search = {
    schema_version: 1,
    strategy: "transcript-message-shards",
    shards: Array.from({ length: 4 }, function (_value, index) {
      const start = baseStart + index * dayMs;
      const eventId = "bounded-" + index;
      const search = contentAddressedJson({
        schema_version: 1,
        kind: "timeline-search-day",
        source_digest: TIMELINE.source_digest,
        team: "codex-widget",
        range: { start_ms: start, end_ms: start + dayMs },
        records: [{
          schema_version: 1,
          ref: "message:codex-widget::" + eventId,
          record_type: "response",
          role: "assistant",
          team: "codex-widget",
          agent_id: "agent-a",
          agent_ref: "agent:codex-widget::agent-a",
          event_id: eventId,
          at_ms: start + 1000,
          text: index % 2 === 0 ? "B3 result" : "B4 result"
        }]
      });
      const links = contentAddressedJson({
        schema_version: 1,
        kind: "timeline-search-links-day",
        source_digest: TIMELINE.source_digest,
        team: "codex-widget",
        range: { start_ms: start, end_ms: start + dayMs },
        prompts: [],
        responses: []
      });
      storedObjects.push(search, links);
      return Object.assign({
        kind: "utc-day",
        day: "2026-03-" + String(9 + index).padStart(2, "0"),
        team: "codex-widget",
        start_ms: start,
        end_ms: start + dayMs,
        counts: { records: 1 },
        linkage: Object.assign({}, links.reference, {
          counts: { prompts: 0, responses: 0 }
        })
      }, search.reference);
    })
  };
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  let active = 0;
  let maximum = 0;
  let release = function () {};
  const gate = new Promise(function (resolve) { release = resolve; });
  await Promise.all(storedObjects.map(function (stored) {
    return page.route("**/" + stored.reference.sha256 + ".json", async function (route) {
      active += 1;
      maximum = Math.max(maximum, active);
      await gate;
      await route.fulfill({
        contentType: "application/json; charset=utf-8",
        body: stored.body
      });
      active -= 1;
    });
  }));

  await page.reload();
  await page.getByTestId("search-scope").selectOption("agent-responses");
  await page.getByTestId("search").fill("B3");
  await expect.poll(function () { return active; }).toBe(6);
  await page.getByTestId("search").fill("B4");
  await page.waitForTimeout(300);
  expect(maximum).toBeLessThanOrEqual(6);
  release();
  const card = page.getByTestId("timeline");
  await expect(card).toHaveAttribute("data-transcript-search-state", "ready");
  await expect(page.getByTestId("search-results").locator(".search-result")).toHaveCount(2);
  await expect(page.getByTestId("search-results")).toContainText("B4 result");
  await expect(page.getByTestId("search-results")).not.toContainText("B3 result");
  expect(maximum).toBeLessThanOrEqual(6);
});

test("schema 2 retries a transiently failed detail shard", async function ({ page }) {
  const fixture = singleDaySchema2Fixture("d".repeat(64), "e".repeat(64));
  let detailRequests = 0;
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    detailRequests += 1;
    if (detailRequests === 1) {
      await route.fulfill({
        status: 503,
        contentType: "application/json; charset=utf-8",
        body: JSON.stringify({ error: "transient fixture failure" })
      });
      return;
    }
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });

  await page.reload();
  const card = page.locator(".timeline-card");
  const timeline = page.getByTestId("timeline");
  const agentLifetime = page.locator('.agent-lifetime-group[data-agent-id="agent-a"]');
  await expect(card).toHaveAttribute("data-timeline-schema-mode", "schema2");
  await expect(timeline).toHaveAttribute("data-render-lod", "lifetime");
  await expect(card).toHaveAttribute("data-loaded-shard-count", "0");

  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(page.getByTestId("modal").locator(".error-message")).toContainText(
    "HTTP 503"
  );
  expect(detailRequests).toBe(1);
  await expect(card).toHaveAttribute("data-loaded-shard-count", "0");
  await page.locator("#modal-close").click();

  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(
    page.getByTestId("modal").locator('.agent-lifetime-phase[data-phase-id="phase-a-1"]')
  ).toBeVisible();
  expect(detailRequests).toBe(2);
  await expect(card).toHaveAttribute("data-loaded-shard-count", "1");
  await expect(page.locator("#load-error")).toBeHidden();
  await page.locator("#modal-close").click();

  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(
    page.getByTestId("modal").locator('.agent-lifetime-phase[data-phase-id="phase-a-2"]')
  ).toBeVisible();
  expect(detailRequests).toBe(2);
});

test("schema 2 lifetime modal uses the phase index instead of every day shard", async function ({
  page
}) {
  const fixture = singleDaySchema2Fixture("4".repeat(64), "5".repeat(64));
  const indexedPhases = TIMELINE.phases.map(function (phase) {
    const projected = Object.assign({}, phase);
    delete projected.states;
    projected.activity_start_ms = phase.start_ms;
    projected.activity_end_ms = phase.end_ms;
    return projected;
  });
  const storedPhaseIndex = contentAddressedJson({
    schema_version: 2,
    kind: "timeline-phase-index",
    phases: indexedPhases
  });
  fixture.bootstrap.phase_index = storedPhaseIndex.reference;
  let phaseIndexRequests = 0;
  let detailShardRequests = 0;
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    detailShardRequests += 1;
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  await page.route("**/" + storedPhaseIndex.reference.sha256 + ".json", async function (route) {
    phaseIndexRequests += 1;
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: storedPhaseIndex.body
    });
  });

  await page.reload();
  const agentLifetime = page.locator('.agent-lifetime-group[data-agent-id="agent-a"]');
  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(
    page.getByTestId("modal").locator('.agent-lifetime-phase[data-phase-id="phase-a-1"]')
  ).toBeVisible();
  await expect(
    page.getByTestId("modal").locator('.agent-lifetime-phase[data-phase-id="phase-a-2"]')
  ).toBeVisible();
  expect(phaseIndexRequests).toBe(1);
  expect(detailShardRequests).toBe(0);

  await page.locator("#modal-close").click();
  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(
    page.getByTestId("modal").locator('.agent-lifetime-phase[data-phase-id="phase-a-2"]')
  ).toBeVisible();
  expect(phaseIndexRequests).toBe(1);
  expect(detailShardRequests).toBe(0);
});

test("schema 2 rejects a phase index from a different source generation", async function ({
  page
}) {
  const fixture = singleDaySchema2Fixture("a".repeat(64), "b".repeat(64));
  const storedStalePhaseIndex = contentAddressedJson({
    schema_version: 2,
    kind: "timeline-phase-index",
    source_digest: "different-generation",
    phases: []
  });
  fixture.bootstrap.phase_index = storedStalePhaseIndex.reference;
  let detailShardRequests = 0;
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    detailShardRequests += 1;
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });
  await page.route("**/" + storedStalePhaseIndex.reference.sha256 + ".json", async function (route) {
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: storedStalePhaseIndex.body
    });
  });

  await page.reload();
  const agentLifetime = page.locator('.agent-lifetime-group[data-agent-id="agent-a"]');
  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(page.getByTestId("modal").locator(".error-message")).toContainText(
    "source digest does not match the timeline generation"
  );
  expect(detailShardRequests).toBe(0);
});

test("a delayed lifetime shard refreshes a detail view after the modal closes", async function ({
  page
}) {
  const fixture = singleDaySchema2Fixture("f".repeat(64), "1".repeat(64));
  let detailRequests = 0;
  let releaseDetail = function () {};
  const detailGate = new Promise(function (resolve) {
    releaseDetail = resolve;
  });
  await routeSingleDaySchema2Fixture(page, fixture, async function (route) {
    detailRequests += 1;
    await detailGate;
    await route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(fixture.detail)
    });
  });

  await page.reload();
  const card = page.locator(".timeline-card");
  const timeline = page.getByTestId("timeline");
  const agentLifetime = page.locator('.agent-lifetime-group[data-agent-id="agent-a"]');
  await expect(card).toHaveAttribute("data-timeline-schema-mode", "schema2");
  await expect(timeline).toHaveAttribute("data-render-lod", "lifetime");

  await agentLifetime.dispatchEvent("dblclick", { detail: 2 });
  await expect(page.getByTestId("modal").locator(".loading-message")).toHaveText(
    "Loading agent work phases…"
  );
  await expect.poll(function () { return detailRequests; }).toBe(1);
  await page.locator("#modal-close").click();
  await expect(page.getByTestId("modal")).toBeHidden();

  for (let attempt = 0; attempt < 3; attempt += 1) {
    const revision = Number(await timeline.getAttribute("data-render-revision"));
    await timeline.press("=");
    await expect.poll(async function () {
      return Number(await timeline.getAttribute("data-render-revision"));
    }).toBeGreaterThan(revision);
  }
  await expect(timeline).toHaveAttribute("data-render-lod", "detail");
  expect(detailRequests).toBe(1);
  await expect(page.locator(".phase-group" + phaseSelector)).toHaveCount(0);

  releaseDetail();
  await expect(page.locator(".phase-group" + phaseSelector)).toBeVisible();
  await expect(card).toHaveAttribute("data-loaded-shard-count", "1");
  expect(detailRequests).toBe(1);
  await expect(page.getByTestId("modal")).toBeHidden();
});

test("the header identifies the project, execution host, and archive timezone", async function ({ page }) {
  const heading = page.locator("#site-title");
  await expect(heading).toHaveText("Agent Timeline: dev-widget, devbig014");
  await expect(heading.getByRole("link", { name: "dev-widget" })).toHaveAttribute(
    "href",
    "https://github.com/example-org/dev-widget"
  );
  await expect(heading.getByRole("link", { name: "dev-widget" })).toHaveAttribute(
    "rel",
    "noopener noreferrer"
  );
  await expect(page).toHaveTitle("Agent Timeline: dev-widget, devbig014");
  await expect(page.locator("#dataset-meta")).toContainText(
    "display America/New_York (explicit)"
  );

  const details = page.locator("#site-identity-details");
  await details.locator("summary").click();
  await expect(details).toHaveAttribute("open", "");
  await expect(page.locator("#site-identity-list")).toContainText(
    "devbig014.example.com"
  );
  await expect(page.locator("#site-identity-list")).toContainText(
    "from session metadata"
  );
  await expect(page.locator("#site-identity-list")).toContainText("agent-utils");
  await expect(page.locator("#site-identity-list")).toContainText("widget");
});

test("a horizontal trackpad gesture pans the zoomed timeline", async function ({ page }) {
  const timeline = await requireContract(
    page.locator('[data-testid="timeline"][data-view-start-ms][data-view-end-ms]'),
    "view-range data attributes are not implemented yet"
  );
  const axis = page.locator("#time-axis");
  const axisBox = await axis.boundingBox();
  expect(axisBox).not.toBeNull();
  await page.mouse.move(
    axisBox.x + axisBox.width * 0.7,
    axisBox.y + axisBox.height / 2
  );
  const full = await readView(timeline);
  await page.mouse.wheel(0, -480);
  const zoomed = await waitForViewChange(timeline, full.start, full.end);
  expect(zoomed.span).toBeLessThan(full.span);

  const tracks = page.locator("#timeline-svg");
  const tracksBox = await tracks.boundingBox();
  expect(tracksBox).not.toBeNull();
  await page.mouse.move(
    tracksBox.x + tracksBox.width * 0.75,
    tracksBox.y + Math.min(100, tracksBox.height / 2)
  );
  await page.mouse.wheel(260, 0);
  const panned = await waitForViewChange(timeline, zoomed.start, zoomed.end);
  expect(Math.abs(panned.span - zoomed.span)).toBeLessThan(zoomed.span * 0.02);
});

test("coarse wheel events and explicit buttons zoom in bounded continuous steps", async function ({ page }) {
  const timeline = page.getByTestId("timeline");
  const axis = page.locator("#time-axis");
  const box = await axis.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);

  const full = await readView(timeline);
  await page.mouse.wheel(0, -500);
  const wheelZoomed = await waitForViewChange(timeline, full.start, full.end);
  expect(wheelZoomed.span).toBeLessThan(full.span);
  expect(wheelZoomed.span).toBeGreaterThan(full.span * 0.84);

  await page.getByTestId("zoom-in").click();
  const buttonZoomed = await waitForViewChange(
    timeline,
    wheelZoomed.start,
    wheelZoomed.end
  );
  expect(buttonZoomed.span).toBeLessThan(wheelZoomed.span * 0.74);
  expect(buttonZoomed.span).toBeGreaterThan(wheelZoomed.span * 0.70);

  await page.getByTestId("zoom-out").click();
  const buttonExpanded = await waitForViewChange(
    timeline,
    buttonZoomed.start,
    buttonZoomed.end
  );
  expect(buttonExpanded.span).toBeGreaterThan(buttonZoomed.span);
});

test("two background clicks select and zoom a visible interval", async function ({ page }) {
  const timeline = page.getByTestId("timeline");
  const svg = page.locator("#timeline-svg");
  const box = await svg.boundingBox();
  expect(box).not.toBeNull();
  const full = await readView(timeline);
  const y = box.y + Math.min(27, box.height / 2);
  const startX = box.x + box.width * 0.42;
  const endX = box.x + box.width * 0.68;

  await page.mouse.click(startX, y);
  await expect(svg).toHaveAttribute("data-range-selection-state", "active");
  await page.mouse.move(endX, y);
  await expect(svg.locator(".range-selection-window")).toHaveCount(1);
  await page.mouse.click(endX, y);

  await expect(svg).not.toHaveAttribute("data-range-selection-state");
  const selected = await waitForViewChange(timeline, full.start, full.end);
  expect(selected.span).toBeLessThan(full.span * 0.35);
  expect(selected.span).toBeGreaterThan(full.span * 0.15);
});

test("semantic zoom drops subpixel detail within a deterministic DOM budget", async function ({ page }) {
  const timeline = page.getByTestId("timeline");
  const svg = page.locator("#timeline-svg");
  const globalDetailed = page.locator("#show-global-messages");

  await expect(timeline).toHaveAttribute("data-render-lod", "detail");
  await expect(timeline).toHaveAttribute("data-render-revision", /\d+/);
  await expect(timeline).toHaveAttribute("data-render-duration-ms", /\d+(?:\.\d+)?/);
  expect(await svg.locator(".state-strip").count()).toBeGreaterThan(0);
  await globalDetailed.check();
  await expect(svg.locator('[data-edge-id="message-a"]')).toHaveCount(1);

  await zoomOutToLod(page, timeline, "lifetime");
  await expect(svg).toHaveAttribute("data-render-lod", "lifetime");
  await expect(svg.locator(".state-strip")).toHaveCount(0);
  await expect(svg.locator(".phase-group")).toHaveCount(0);
  await expect(svg.locator('[data-edge-id="message-a"]')).toHaveCount(0);
  await expect(svg.locator(".edge-group")).toHaveCount(0);
  await svg.locator('.agent-lifetime-group[data-agent-id="agent-a"]').click();
  await expect(timeline).toHaveAttribute("data-selected-agent-id", "agent-a");
  await expect(svg.locator('[data-edge-id="spawn-a"]')).toHaveCount(1);
  await expect(svg.locator('[data-edge-id="result-a"]')).toHaveCount(1);
  await expect(svg.locator(".agent-lifetime-group")).toHaveCount(AGENT_COUNT);
  await expect(timeline).toHaveAttribute("data-selected-agent-id", "agent-a");

  await zoomOutToLod(page, timeline, "aggregate");
  await expect(svg).toHaveAttribute("data-render-lod", "aggregate");
  await expect(svg.locator(".state-strip")).toHaveCount(0);
  await expect(svg.locator(".phase-group")).toHaveCount(0);
  await expect(svg.locator(".edge-group")).toHaveCount(0);
  await expect(svg.locator(".agent-lifetime-group")).toHaveCount(0);
  await expect(svg).toHaveAttribute("data-track-mode", "aggregate");
  await expect(svg).toHaveAttribute("data-aggregate-resolution", "hourly");
  await expect(svg).toHaveAttribute("data-lane-count", "1");
  await expect(svg.locator(".activity-bin-group")).toHaveCount(3);
  await expect(svg.locator('.activity-bin-group[data-activity-role="combined"]')).toHaveCount(3);
  await expect(svg.locator('.activity-bin-group[data-summary-available="true"]')).toHaveCount(2);
  await expect(svg.locator('.activity-bin-group[data-summary-available="false"]')).toHaveCount(1);
  await expect(svg.locator(
    '.activity-bin-group[data-start-ms="' + AGGREGATE_GAP_START_MS + '"]'
  )).toHaveCount(0);
  await expect(svg.locator(
    '.activity-bin-group[data-start-ms="' + AGGREGATE_LATER_START_MS + '"]'
  )).toHaveCount(1);
  // Height is LINEAR in agents present, which is the only rule under which a reader can compare
  // two team blocks and get the ratio right. The fixture's first hour holds three agents (a
  // coordinator present throughout plus two workers on average) and its last holds one, so the
  // drawn heights must be in a 3:1 ratio -- not the 2:1 the superseded log scale gave.
  const threeAgentBin = svg.locator(
    '.activity-bin-group[data-start-ms="' + AGGREGATE_HOUR_START_MS + '"]'
  );
  const oneAgentBin = svg.locator(
    '.activity-bin-group[data-start-ms="' + AGGREGATE_LATER_START_MS + '"]'
  );
  await expect(threeAgentBin).toHaveAttribute("data-agents-present", "3.00");
  await expect(oneAgentBin).toHaveAttribute("data-agents-present", "1.00");
  const busyHeight = Number(
    await threeAgentBin.locator("rect").getAttribute("height")
  );
  const quieterHeight = Number(
    await oneAgentBin.locator("rect").getAttribute("height")
  );
  expect(busyHeight).toBe(quieterHeight * 3);

  // What linear costs, and how it is paid. A burst far above typical concurrency cannot be drawn
  // at true scale in one team row, so it is clamped -- and a clamped block carries a saw-tooth
  // top edge, because a twelve-agent hour silently drawn exactly as tall as a nine-agent one is
  // the misreading this whole change exists to remove. The count it could not draw is still in
  // the DOM and in the accessible name.
  const burstBin = svg.locator(
    '.activity-bin-group[data-start-ms="' + AGGREGATE_BURST_START_MS + '"]'
  );
  await expect(burstBin).toHaveAttribute("data-height-saturated", "true");
  await expect(burstBin).toHaveAttribute("data-agents-present", "12.50");
  await expect(burstBin).toHaveAttribute("aria-label", /drawn clamped at 9/);
  await expect(burstBin.locator(".activity-bin-overflow")).toHaveCount(1);
  const burstHeight = Number(await burstBin.locator("rect").getAttribute("height"));
  expect(burstHeight).toBeGreaterThan(busyHeight);
  // `quieterHeight` is one agent's worth of pixels, so this is "the clamp really did cost it
  // height" -- twelve and a half agents at true scale would not fit in the row.
  expect(burstHeight).toBeLessThan(12.5 * quieterHeight);
  await expect(threeAgentBin).toHaveAttribute("data-height-saturated", "false");
  await expect(threeAgentBin.locator(".activity-bin-overflow")).toHaveCount(0);
  await expect(oneAgentBin).toHaveAttribute("data-height-saturated", "false");
  await expect(oneAgentBin.locator(".activity-bin-overflow")).toHaveCount(0);

  const busyOpacity = Number(
    await threeAgentBin.locator("rect").getAttribute("fill-opacity")
  );
  const quieterOpacity = Number(await svg.locator(
    '.activity-bin-group[data-start-ms="' + AGGREGATE_LATER_START_MS + '"] rect'
  ).getAttribute("fill-opacity"));
  expect(busyOpacity).toBeGreaterThan(quieterOpacity);
  const svgNodeCount = await svg.locator("*").count();
  expect(svgNodeCount).toBeLessThanOrEqual(160);
  expect(Number(await timeline.getAttribute("data-render-duration-ms"))).toBeLessThan(100);
});

test("single clicks select and a double click opens phase detail", async function ({ page }) {
  const timeline = await requireContract(
    page.locator('[data-testid="timeline"][data-selection-scope]'),
    "selection state data attributes are not implemented yet"
  );
  const phase = page.locator(phaseSelector);
  const modal = page.getByTestId("modal");

  await phase.click();
  await expect(modal).toBeHidden();
  await expect(timeline).toHaveAttribute("data-selection-scope", "agent");
  await expect(timeline).toHaveAttribute("data-selected-agent-id", "agent-a");

  await page.waitForTimeout(600);
  await phase.click();
  await expect(modal).toBeHidden();
  await expect(timeline).toHaveAttribute("data-selection-scope", "phase");
  await expect(timeline).toHaveAttribute("data-selected-phase-id", "phase-a-1");

  await page.waitForTimeout(600);
  await phase.dblclick();
  await expect(modal).toBeVisible();
  await expect(page.locator("#modal-title")).toContainText("Audit parser invariants");
});

test("different work phases select directly while the same phase toggles scope", async function ({ page }) {
  const timeline = page.getByTestId("timeline");
  const firstPhase = page.locator(phaseSelector);
  const secondPhase = page.locator(secondPhaseSelector);

  await firstPhase.click();
  await page.waitForTimeout(600);
  await firstPhase.click();
  await expect(timeline).toHaveAttribute("data-selected-phase-id", "phase-a-1");

  await page.waitForTimeout(600);
  await secondPhase.click();
  await expect(timeline).toHaveAttribute("data-selection-scope", "phase");
  await expect(timeline).toHaveAttribute("data-selected-phase-id", "phase-a-2");

  await page.waitForTimeout(600);
  await secondPhase.click();
  await expect(timeline).toHaveAttribute("data-selection-scope", "agent");
  await expect(timeline).not.toHaveAttribute("data-selected-phase-id");
});

test("packed lane labels list their agents and empty background clears selection", async function ({ page }) {
  const timeline = page.getByTestId("timeline");
  const lane = page.locator('[data-lane-index="1"]');
  await expect(lane).toHaveAttribute("aria-label", /2 named agents/);
  await lane.click();

  const menu = page.getByTestId("lane-agent-menu");
  await expect(menu).toBeVisible();
  await expect(menu.getByRole("menuitem", { name: /Parser audit/ })).toBeVisible();
  await expect(menu.getByRole("menuitem", { name: /Documentation audit/ })).toBeVisible();
  await menu.getByRole("menuitem", { name: /Documentation audit/ }).click();
  await expect(menu).toBeHidden();
  await expect(timeline).toHaveAttribute("data-selected-agent-id", "agent-c");

  const svg = page.locator("#timeline-svg");
  const box = await svg.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.click(box.x + box.width - 12, box.y + 54 + 27);
  await expect(timeline).toHaveAttribute("data-selection-scope", "none");
  await expect(svg).toHaveAttribute("data-range-selection-state", "active");
  await page.keyboard.press("Escape");
  await expect(svg).not.toHaveAttribute("data-range-selection-state");

  await lane.click();
  await expect(menu).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(menu).toBeHidden();
  await expect(lane).toHaveAttribute("aria-expanded", "false");
});

test("agent lifetime hover shows the hindsight summary and tail-truncated official path", async function ({ page }) {
  const lifetime = page.locator('.agent-lifetime-group[data-agent-id="agent-a"]');
  await expect(lifetime).toHaveAttribute(
    "aria-label",
    /\/root\/transcript_auditor\/owner_turn_miner\/plugin_layout_audit\/parser_boundary_regression_audit/
  );
  await lifetime.hover({ position: { x: 4, y: 4 } });
  const tooltip = page.getByTestId("tooltip");
  await expect(tooltip).toBeVisible();
  await expect(page.locator("#tooltip-title")).toHaveText("Parser audit");
  await expect(page.locator("#tooltip-body")).not.toContainText("Short name:");
  await expect(page.locator("#tooltip-body")).toContainText("Official: …");
  await expect(page.locator("#tooltip-body")).toContainText("parser_boundary_regression_audit");
  await expect(page.locator("#tooltip-body")).toContainText("Found the malformed-input parser boundary");
});

test("the phase context menu can zoom exactly to the work phase", async function ({ page }) {
  const timeline = await requireContract(
    page.locator('[data-testid="timeline"][data-view-start-ms][data-view-end-ms]'),
    "view-range data attributes are not implemented yet"
  );
  const menu = await requireContract(
    page.locator('[data-testid="timeline-context-menu"]'),
    "the timeline context-menu test id is not implemented yet"
  );

  await page.locator(phaseSelector).click({ button: "right" });
  await expect(menu).toBeVisible();
  const zoomPhase = await requireContract(
    menu.getByRole("menuitem", { name: /zoom to (work )?phase/i }),
    "the Zoom to phase menu action is not implemented yet"
  );
  await zoomPhase.click();
  await expect(menu).toBeHidden();

  await expect.poll(async function () {
    const view = await readView(timeline);
    return Math.abs(view.start - PHASE_A_START_MS) <= 1 &&
      Math.abs(view.end - PHASE_A_END_MS) <= 1;
  }).toBeTruthy();
});

test("phase and agent zoom actions trim scoped idle margins and unrelated work", async function ({ page }) {
  const timeline = page.getByTestId("timeline");
  const menu = page.getByTestId("timeline-context-menu");
  const fit = page.getByTestId("fit");
  const phase = page.locator(secondPhaseSelector);
  const lifetime = page.locator('.agent-lifetime-group[data-agent-id="agent-a"]');

  await phase.click({ button: "right" });
  await menu.getByRole("menuitem", { name: "Zoom to work phase", exact: true }).click();
  await expect.poll(async function () {
    const view = await readView(timeline);
    return [view.start, view.end];
  }).toEqual([PHASE_A2_ACTIVITY_START_MS, PHASE_A2_ACTIVITY_END_MS]);

  await fit.click();
  await phase.click({ button: "right" });
  await menu.getByRole("menuitem", { name: "Zoom to agent lifetime", exact: true }).click();
  await expect.poll(async function () {
    const view = await readView(timeline);
    return [view.start, view.end];
  }).toEqual([AGENT_A_ACTIVITY_START_MS, AGENT_A_ACTIVITY_END_MS]);

  await fit.click();
  await lifetime.dispatchEvent("contextmenu", {
    button: 2,
    clientX: 200,
    clientY: 300
  });
  await expect(page.locator("#context-menu-title")).toHaveText("Parser audit");
  await menu.getByRole("menuitem", { name: "Zoom to agent lifetime", exact: true }).click();
  await expect.poll(async function () {
    const view = await readView(timeline);
    return [view.start, view.end];
  }).toEqual([AGENT_A_ACTIVITY_START_MS, AGENT_A_ACTIVITY_END_MS]);

  const selectedPhase = TIMELINE.phases.find(function (item) {
    return item.id === "phase-a-2";
  });
  const selectedAgent = TIMELINE.agents.find(function (item) {
    return item.id === "agent-a";
  });
  expect(selectedPhase.start_ms).toBeLessThan(PHASE_A2_ACTIVITY_START_MS);
  expect(selectedPhase.end_ms).toBeGreaterThan(PHASE_A2_ACTIVITY_END_MS);
  expect(selectedAgent.start_ms).toBeLessThan(AGENT_A_ACTIVITY_START_MS);
  expect(selectedAgent.end_ms).toBeGreaterThan(AGENT_A_ACTIVITY_END_MS);
});

test("rollup context menus trim empty calendar time without crossing the selected range", async function ({ page }) {
  const timeline = page.getByTestId("timeline");
  const menu = page.getByTestId("timeline-context-menu");
  const actionNames = {
    daily: "Zoom to day",
    weekly: "Zoom to week",
    monthly: "Zoom to month",
    quarterly: "Zoom to quarter"
  };
  expect(ROLLUP_RANGES[0].end_ms - ROLLUP_RANGES[0].start_ms).toBe(
    23 * 60 * 60 * 1000
  );

  for (const [index, rollup] of ROLLUP_RANGES.entries()) {
    await page.getByTestId("fit").click();
    await expect.poll(async function () {
      const view = await readView(timeline);
      return [view.start, view.end];
    }).toEqual([DATA_START_MS, DATA_END_MS]);

    const marker = page.locator(
      '.rollup-marker.rollup-' + rollup.kind +
      '[data-start-ms="' + rollup.start_ms + '"]'
    );
    await expect(marker).toBeVisible();
    await marker.click({ button: "right" });
    await expect(menu).toBeVisible();
    await expect(page.locator("#context-menu-title")).toHaveText(rollup.label);
    await expect(timeline).toHaveAttribute("data-selection-scope", "none");
    await menu.getByRole("menuitem", { name: actionNames[rollup.kind], exact: true }).click();

    await expect.poll(async function () {
      const view = await readView(timeline);
      return [view.start, view.end];
    }).toEqual([
      ROLLUP_EXPECTED_RANGES[index].start_ms,
      ROLLUP_EXPECTED_RANGES[index].end_ms
    ]);

    const view = await readView(timeline);
    expect(view.start).toBeGreaterThanOrEqual(rollup.start_ms);
    expect(view.end).toBeLessThanOrEqual(rollup.end_ms);
  }
});

test("packed tracks are the default and per-agent tracks remain available", async function ({ page }) {
  const svg = await requireContract(
    page.locator("#timeline-svg[data-track-mode][data-lane-count]"),
    "track-mode and lane-count data attributes are not implemented yet"
  );
  const toggle = page.locator("#per-agent-tracks");

  await expect(toggle).not.toBeChecked();
  await expect(svg).toHaveAttribute("data-track-mode", "packed");
  const packedCount = Number(await svg.getAttribute("data-lane-count"));
  expect(packedCount).toBeGreaterThan(0);
  expect(packedCount).toBeLessThan(AGENT_COUNT);

  await toggle.check();
  await expect(svg).toHaveAttribute("data-track-mode", "per-agent");
  await expect(svg).toHaveAttribute("data-lane-count", String(AGENT_COUNT));
});

test("fork and join edges stay structural while intermediate messages are detailed", async function ({ page }) {
  const spawn = page.locator('.edge-group[data-edge-id="spawn-a"]');
  const result = page.locator('.edge-group[data-edge-id="result-a"]');
  const message = page.locator('.edge-group[data-edge-id="message-a"]');
  const globalDetailed = page.locator("#show-global-messages");

  await expect(globalDetailed).not.toBeChecked();
  await expect(spawn).toHaveCount(1);
  await expect(spawn).toHaveAttribute("data-edge-state", "normal");
  await expect(result).toHaveCount(1);
  await expect(result).toHaveAttribute("data-edge-state", "normal");
  await expect(message).toHaveCount(0);

  const spawnWidth = await spawn.locator(".edge-visible").evaluate(function (element) {
    return Number.parseFloat(window.getComputedStyle(element).strokeWidth);
  });
  const resultWidth = await result.locator(".edge-visible").evaluate(function (element) {
    return Number.parseFloat(window.getComputedStyle(element).strokeWidth);
  });
  expect(resultWidth).toBe(spawnWidth);

  const structuralBends = await Promise.all([spawn, result].map(async function (edge) {
    return edge.locator(".edge-visible").evaluate(function (element) {
      const coordinates = (element.getAttribute("d") || "").match(/-?\d+(?:\.\d+)?/g) || [];
      return Number(coordinates[2]) - Number(coordinates[0]);
    });
  }));
  expect(structuralBends[0]).toBeLessThan(0);
  expect(structuralBends[1]).toBeGreaterThan(0);

  await globalDetailed.check();
  await expect(message).toHaveCount(1);
  await expect(message).toHaveAttribute("data-edge-state", "normal");
  const messageWidth = await message.locator(".edge-visible").evaluate(function (element) {
    return Number.parseFloat(window.getComputedStyle(element).strokeWidth);
  });
  expect(messageWidth).toBeLessThan(resultWidth);
});

test("full-transcript role filters support user-only, none, and all", async function ({ page }) {
  await page.locator(phaseSelector).dblclick();
  await expect(page.getByTestId("modal")).toBeVisible();
  await page.getByRole("tab", { name: "Full Transcript" }).click();
  const filters = await requireContract(
    page.locator('[data-testid="transcript-role-filters"]'),
    "transcript role filters are not implemented yet"
  );
  const entries = page.locator(".transcript-entry[data-role]");
  await requireContract(
    entries,
    "transcript entries do not expose normalized data-role values yet"
  );
  expect(await entries.count()).toBe(5);
  const pullRequest = page.locator('.pr-reference[href="https://github.com/example-org/dev-widget/pull/38"]');
  await expect(pullRequest).toHaveAttribute("title", "Repair malformed-input handling");

  await filters.getByRole("button", { name: /user only/i }).click();
  await expect(page.locator('.transcript-entry[data-role="user"]:visible')).toHaveCount(1);
  await expect(page.locator('.transcript-entry:not([data-role="user"]):visible')).toHaveCount(0);

  await filters.getByRole("button", { name: /select none/i }).click();
  await expect(page.locator(".transcript-entry[data-role]:visible")).toHaveCount(0);

  await filters.getByRole("button", { name: /select all/i }).click();
  await expect(page.locator(".transcript-entry[data-role]:visible")).toHaveCount(5);
});

test("rollups switch audiences and verified glossary terms open stable entries", async function ({ page }) {
  const attackerRequests = [];
  page.on("request", function (request) {
    if (request.url().startsWith("https://attacker.invalid")) {
      attackerRequests.push(request.url());
    }
  });
  const daily = page.locator(".rollup-marker.rollup-daily").first();
  await daily.dblclick();
  await expect(page.getByTestId("modal")).toBeVisible();
  await expect(page.getByRole("tab", { name: "Technical" })).toBeVisible();
  await expect(page.getByRole("tab", { name: "Plain Language" })).toBeVisible();
  await expect(page.locator("#modal-content")).toContainText("technical summary");

  await page.getByRole("tab", { name: "Plain Language" }).click();
  await expect(page.locator("#modal-content")).toContainText("plain-language summary");
  const term = page.locator(
    '.glossary-term-link[data-glossary-id="term-malformed-input-123456789abc"]'
  ).first();
  await expect(term).toHaveAttribute(
    "href",
    "#glossary/term-malformed-input-123456789abc"
  );
  await term.click();
  await expect(page.locator("#modal-title")).toHaveText("malformed-input");
  await expect(page).toHaveURL(/#glossary\/term-malformed-input-123456789abc$/);
  await expect(page.locator("#modal-content")).toContainText("inline docs");
  await expect(page.locator("#modal-content")).toContainText("https://attacker.invalid/bare");
  await expect(page.locator("#modal-content a[href^='https://attacker.invalid']")).toHaveCount(0);
  await expect(page.locator("#modal-content a[href^='mailto:']")).toHaveCount(0);
  await expect(page.locator("#modal-content img")).toHaveCount(0);
  await expect(
    page.locator(
      '#modal-content .glossary-term-link[href="#glossary/term-malformed-input-123456789abc"]'
    )
  ).toHaveCount(1);
  expect(attackerRequests).toEqual([]);

  await page.locator("#modal-close").click();
  await page.getByTestId("glossary-open").click();
  await expect(page.locator("#modal-title")).toHaveText("Project glossary");
  await expect(page.locator("#modal-content")).toContainText("Data that does not satisfy");
  await expect(page.locator("#modal-content a[href^='https://attacker.invalid']")).toHaveCount(0);
  expect(attackerRequests).toEqual([]);
});

test("sparse archives expose raw detail without inventing or fetching summaries", async function ({ page }) {
  const sparse = JSON.parse(JSON.stringify(TIMELINE));
  const agent = sparse.agents.find(function (item) { return item.id === "agent-a"; });
  const phase = sparse.phases.find(function (item) { return item.id === "phase-a-1"; });
  const rollup = sparse.rollups[0];
  agent.summary_available = false;
  agent.lifetime_summary = "A stale lifetime placeholder that must stay hidden.";
  phase.summary_available = false;
  phase.phrase = "A stale phase phrase that must stay hidden";
  phase.paragraph = "A stale phase paragraph that must stay hidden.";
  rollup.summary_available = false;
  rollup.technical_summary_available = false;
  rollup.plain_language_summary_available = false;
  rollup.technical_path = "";
  rollup.plain_language_path = "";
  rollup.stats = {
    user_prompts: 7,
    agent_responses: 11,
    inter_agent_messages: 3,
    tool_calls: 19
  };
  sparse.glossary = [];
  sparse.glossary_path = "";
  sparse.summary_files = [];

  await page.route("**/data/timeline.json", function (route) {
    return route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(sparse)
    });
  });
  await page.route("**/details/phase-a-1.json", function (route) {
    return route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify({
        summary_available: false,
        phrase: "A stale detail phrase that must stay hidden",
        paragraph: "A stale detail paragraph that must stay hidden.",
        stats: phase.stats,
        work_summary: [{ at_ms: phase.start_ms, text: "Synthetic summary text" }],
        transcript: [{
          role: "user",
          at_ms: phase.start_ms,
          text: "Preserve this raw owner-intent prompt."
        }],
        raw_summary_path: "summaries/phases/must-not-be-fetched.md"
      })
    });
  });
  const markdownRequests = [];
  page.on("request", function (request) {
    if (new URL(request.url()).pathname.endsWith(".md")) {
      markdownRequests.push(request.url());
    }
  });

  await page.reload();
  await expect(page.locator("#dataset-meta")).not.toContainText("Loading timeline");
  await expect(page.locator("#load-error")).toBeHidden();
  await expect(page.getByTestId("glossary-open")).toBeHidden();

  const sparsePhase = page.locator(phaseSelector);
  const sparseAgent = page.locator('.agent-lifetime-group[data-agent-id="agent-a"]');
  const sparseRollup = page.locator(
    '.rollup-marker[data-start-ms="' + rollup.start_ms + '"]'
  );
  await expect(sparsePhase).toHaveClass(/summary-not-generated/);
  await expect(sparseAgent).toHaveClass(/summary-not-generated/);
  await expect(sparseRollup).toHaveClass(/summary-not-generated/);
  await expect(sparsePhase).toHaveAttribute("aria-label", /Summary not generated/);

  await sparsePhase.dblclick();
  await expect(page.getByTestId("modal")).toBeVisible();
  await expect(page.locator("#modal-title")).toHaveText("Activity window");
  await expect(page.locator("#modal-summary")).toContainText("Summary not generated");
  await expect(page.getByRole("tab", { name: "Full Transcript" })).toHaveAttribute(
    "aria-selected",
    "true"
  );
  await expect(page.getByRole("tab", { name: "Agent Work Summary" })).toHaveCount(0);
  await expect(page.getByRole("tab", { name: "Markdown Summary" })).toHaveCount(0);
  await expect(page.locator("#modal-content")).toContainText(
    "Preserve this raw owner-intent prompt."
  );
  await expect(page.locator("#modal-content")).not.toContainText("Synthetic summary text");
  await page.locator("#modal-close").click();

  await sparseAgent.dispatchEvent("dblclick", { detail: 2 });
  await expect(page.locator("#modal-summary")).toContainText(
    "Summary not generated for this agent lifetime."
  );
  await expect(page.locator("#modal-summary")).not.toContainText("stale lifetime placeholder");
  await page.locator("#modal-close").click();

  await sparseRollup.dblclick();
  await expect(page.getByRole("tab", { name: "Summary" })).toHaveAttribute(
    "aria-selected",
    "true"
  );
  await expect(page.locator("#modal-summary")).toContainText("Summary not generated");
  await expect(page.locator("#modal-summary")).toContainText("7 prompts");
  await expect(page.getByRole("tab", { name: "Technical" })).toHaveCount(0);
  await expect(page.getByRole("tab", { name: "Plain Language" })).toHaveCount(0);
  await expect(page.locator("#modal-content")).toContainText("Summary not generated");
  expect(markdownRequests).toEqual([]);
});

test("the artifact catalog stays lazy until an artifact tab is opened", async function ({ page }) {
  let artifactRequests = 0;
  page.on("request", function (request) {
    if (new URL(request.url()).pathname === "/data/artifacts.json") {
      artifactRequests += 1;
    }
  });
  await page.reload();
  await expect(page.locator("#dataset-meta")).not.toContainText("Loading timeline");
  await expect(page.locator(phaseSelector)).toBeVisible();
  expect(artifactRequests).toBe(0);

  await page.locator(phaseSelector).dblclick();
  await page.getByRole("tab", { name: "Artifacts (3)" }).click();
  await expect.poll(function () { return artifactRequests; }).toBe(1);
  await expect(page.locator('[data-artifact-section="outputs"] .artifact-card')).toHaveCount(1);
});

test("artifact outputs and references stay distinct across phase, rollup, and agent views", async function ({ page }) {
  await page.locator(phaseSelector).dblclick();
  await expect(page.getByTestId("modal")).toBeVisible();
  await page.getByRole("tab", { name: "Artifacts (3)" }).click();

  const outputs = page.locator('[data-artifact-section="outputs"]');
  const references = page.locator('[data-artifact-section="references"]');
  await expect(outputs.locator(".artifact-card")).toHaveCount(1);
  await expect(references.locator(".artifact-card")).toHaveCount(2);
  await expect(outputs).toContainText("Produced");
  await expect(references).toContainText("Referenced");

  const pullRequest = outputs.locator('[data-artifact-id="artifact-pr38"] .artifact-primary-link');
  await expect(pullRequest).toHaveAttribute(
    "href",
    "https://github.com/example-org/dev-widget/pull/38"
  );
  await expect(pullRequest).toHaveAttribute("target", "_blank");
  await expect(pullRequest).toHaveAttribute("rel", "noopener noreferrer");
  await expect(outputs.locator(".artifact-project-link")).toHaveText("example-org/dev-widget");

  const unsafeCard = references.locator('[data-artifact-id="artifact-unsafe-link"]');
  await expect(unsafeCard.locator("a.artifact-primary-link")).toHaveCount(0);
  await expect(unsafeCard.locator(".artifact-link-disabled")).toHaveText("Unsafe transcript link");
  await expect(page.locator('.artifact-phase-link[data-phase-id="phase-a-1"]')).toHaveCount(3);

  await page.locator("#modal-close").click();
  await page.locator(".rollup-marker.rollup-daily").first().dblclick();
  await page.getByRole("tab", { name: "Artifacts (3)" }).click();
  await expect(page.locator('[data-artifact-section="outputs"] .artifact-card')).toHaveCount(1);
  await page.locator('.artifact-phase-link[data-phase-id="phase-a-1"]').first().click();
  await expect(page.locator("#modal-title")).toHaveText("Audit parser invariants");

  await page.locator("#modal-close").click();
  await page.locator('.agent-lifetime-group[data-agent-id="agent-a"]').dispatchEvent(
    "dblclick",
    { detail: 2 }
  );
  await expect(page.locator("#modal-title")).toHaveText("Parser audit");
  await expect(page.getByRole("tab", { name: "Work Phases" })).toBeVisible();
  await expect(page.getByRole("tab", { name: "Artifacts (3)" })).toBeVisible();
  await page.getByRole("tab", { name: "Artifacts (3)" }).click();
  await expect(page.locator('[data-artifact-section="references"] .artifact-card')).toHaveCount(2);
});

test("legacy archives do not request or require an artifact catalog", async function ({ page }) {
  const legacy = JSON.parse(JSON.stringify(TIMELINE));
  delete legacy.artifact_catalog_path;
  [legacy.agents, legacy.phases, legacy.rollups].forEach(function (items) {
    items.forEach(function (item) {
      delete item.artifact_ids;
      delete item.output_artifact_ids;
    });
  });
  let artifactRequests = 0;
  page.on("request", function (request) {
    if (new URL(request.url()).pathname === "/data/artifacts.json") {
      artifactRequests += 1;
    }
  });
  await page.route("**/data/timeline.json", function (route) {
    return route.fulfill({
      contentType: "application/json; charset=utf-8",
      body: JSON.stringify(legacy)
    });
  });
  await page.reload();
  await expect(page.locator("#dataset-meta")).not.toContainText("Loading timeline");
  await expect(page.locator("#load-error")).toBeHidden();
  await expect(page.locator(phaseSelector)).toBeVisible();
  expect(artifactRequests).toBe(0);

  await page.locator('.agent-lifetime-group[data-agent-id="agent-a"]').dispatchEvent(
    "dblclick",
    { detail: 2 }
  );
  await expect(page.locator("#modal-title")).toHaveText("Parser audit");
  await expect(page.getByRole("tab", { name: /Artifacts/ })).toHaveCount(0);
});
