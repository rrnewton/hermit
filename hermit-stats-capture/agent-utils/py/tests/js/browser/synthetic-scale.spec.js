"use strict";

// The level-of-detail machinery, driven by a real browser against a real archive.
//
// `timeline-interactions.spec.js` runs against a four-agent fixture and covers interaction
// behaviour. It cannot cover this: `timeline-core.semanticZoomLevel` picks `detail`, `lifetime` or
// `aggregate` from milliseconds-per-pixel, so a fixture spanning one afternoon renders `detail` at
// every reachable zoom and the other two branches are never taken. Those branches are the ones
// that keep a large archive responsive -- they suppress phases, then agents and edges entirely --
// and until this file existed nothing automated ever entered them.
//
// The archive here spans eleven days and holds about two hundred agents and a thousand phases,
// which is enough for a fitted view to sit well past the five-minute-per-pixel aggregate
// threshold and for a zoomed-in view to be genuinely busy. It is generated, not committed; see
// `synthetic-archive.cjs` for why, and for what makes it reproducible.
//
// Zooming is done with the wheel, against the view range the page publishes, rather than by
// calling into the app. A test that set the range directly could put the page in a state a reader
// cannot reach, and would stop covering the path that actually renders.

const { test, expect } = require("@playwright/test");
const {
  ensureSyntheticArchive,
  startArchiveServer,
  stopArchiveServer
} = require("./synthetic-archive.cjs");

//: Floors, not equalities. They exist so that shrinking the generated size below what the
//: thresholds need fails here, loudly, instead of quietly turning this file into a second
//: small-fixture test.
const MINIMUM_AGENTS = 150;
const MINIMUM_PHASES = 600;

let server = null;
let baseUrl = "";
let report = null;

test.describe("synthetic archive at scale", function () {
  test.describe.configure({ mode: "serial", timeout: 120_000 });

  test.beforeAll(async function () {
    // Generating and building the archive is the slow part, and it happens at most once per
    // change to the package; the cached path is a few hundred milliseconds.
    test.setTimeout(900_000);
    const generated = ensureSyntheticArchive();
    report = generated.report;
    const started = await startArchiveServer(generated.archive);
    server = started.child;
    baseUrl = started.baseUrl;
  });

  test.afterAll(async function () {
    await stopArchiveServer(server);
    server = null;
  });

  async function openFitted(page) {
    const response = await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    expect(response, "the archive server answered the navigation").not.toBeNull();
    expect(response.status()).toBeLessThan(400);
    await page.waitForFunction(function () {
      const loadError = document.querySelector("#load-error");
      if (loadError && !loadError.hidden && (loadError.textContent || "").trim()) {
        return true;
      }
      const timeline = document.querySelector('[data-testid="timeline"]');
      const svg = document.querySelector("#timeline-svg");
      if (!timeline || !svg) {
        return false;
      }
      const start = Number(timeline.getAttribute("data-view-start-ms"));
      const end = Number(timeline.getAttribute("data-view-end-ms"));
      const revision = Number(timeline.getAttribute("data-render-revision"));
      return Number.isFinite(start) && Number.isFinite(end) && end > start && revision >= 1;
    }, null, { timeout: 90_000 });
    const failure = await page.evaluate(function () {
      const loadError = document.querySelector("#load-error");
      return loadError && !loadError.hidden ? (loadError.textContent || "").trim() : "";
    });
    expect(failure, "the page reported no load failure").toBe("");
  }

  //: Wait until the page stops redrawing before reading it.
  //:
  //: This archive is loaded in pieces: the day shards a view needs arrive after the view exists,
  //: and each arrival redraws. Reading the scene the instant a zoom is honoured therefore samples
  //: whatever had loaded by then, which on a slow or cold run is nothing -- and a check that reads
  //: an empty scene reports "the detail level drew no phases" about a page that drew them a moment
  //: later. Settling on the revision counter waits for the data rather than for a fixed delay, and
  //: still fails if the data never comes: the assertions afterwards are unchanged.
  async function settle(page) {
    const deadline = Date.now() + 30_000;
    let previous = -1;
    while (Date.now() < deadline) {
      const revision = await page.evaluate(function () {
        const timeline = document.querySelector('[data-testid="timeline"]');
        return Number(timeline.getAttribute("data-render-revision"));
      });
      if (revision === previous) {
        return;
      }
      previous = revision;
      await page.waitForTimeout(400);
    }
  }

  async function shape(page) {
    await settle(page);
    return page.evaluate(function () {
      const timeline = document.querySelector('[data-testid="timeline"]');
      const svg = document.querySelector("#timeline-svg");
      return {
        lod: svg.getAttribute("data-render-lod"),
        trackMode: svg.getAttribute("data-track-mode"),
        resolution: svg.getAttribute("data-aggregate-resolution"),
        spanMs:
          Number(timeline.getAttribute("data-view-end-ms")) -
          Number(timeline.getAttribute("data-view-start-ms")),
        bins: svg.querySelectorAll(".activity-bin-group").length,
        lifetimes: svg.querySelectorAll(".agent-lifetime-group").length,
        phases: svg.querySelectorAll(".phase-group").length,
        strips: svg.querySelectorAll(".state-strip").length
      };
    });
  }

  async function wheelZoom(page, deltaY) {
    const axis = page.locator("#time-axis");
    const box = await axis.boundingBox();
    expect(box, "the time axis is visible").not.toBeNull();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    const before = await page.evaluate(function () {
      const timeline = document.querySelector('[data-testid="timeline"]');
      return Number(timeline.getAttribute("data-render-revision"));
    });
    await page.mouse.wheel(0, deltaY);
    await page.waitForFunction(function (revision) {
      const timeline = document.querySelector('[data-testid="timeline"]');
      return Number(timeline.getAttribute("data-render-revision")) > revision;
    }, before, { timeout: 15_000 }).catch(function () {
      // A clamped zoom legitimately redraws nothing; the caller notices via the span.
    });
  }

  test("the generated archive is large enough to have levels of detail", function () {
    expect(report.build.agents).toBeGreaterThanOrEqual(MINIMUM_AGENTS);
    expect(report.build.phases).toBeGreaterThanOrEqual(MINIMUM_PHASES);
    // Five days at a normal window is roughly where a fitted view crosses into `aggregate`;
    // this corpus is twice that, so the crossing does not depend on the exact viewport.
    expect(report.span_ms).toBeGreaterThan(10 * 24 * 60 * 60 * 1000);
  });

  test("a fitted multi-day view is aggregate and suppresses the rest", async function ({ page }) {
    await openFitted(page);
    const fitted = await shape(page);

    expect(fitted.lod).toBe("aggregate");
    expect(fitted.trackMode).toBe("aggregate");
    expect(["hourly", "daily", "weekly"]).toContain(fitted.resolution);
    expect(fitted.bins).toBeGreaterThan(0);
    // The suppression is the point of the level: at this density a per-agent scene would be tens
    // of thousands of nodes, and drawing it is the failure this branch exists to avoid.
    expect(fitted.phases).toBe(0);
    expect(fitted.lifetimes).toBe(0);
    expect(fitted.strips).toBe(0);
  });

  test("zooming in walks aggregate to lifetime to detail", async function ({ page }) {
    await openFitted(page);
    const seen = [];
    let current = await shape(page);
    seen.push(current);
    for (let step = 0; step < 60 && current.lod !== "detail"; step += 1) {
      await wheelZoom(page, -360);
      const next = await shape(page);
      if (next.spanMs >= current.spanMs) {
        break;
      }
      current = next;
      seen.push(current);
    }

    const levels = seen.map(function (entry) { return entry.lod; });
    expect(levels[0], "a fitted view of eleven days starts at aggregate").toBe("aggregate");
    expect(levels, "the intermediate level is entered on the way in").toContain("lifetime");
    expect(levels[levels.length - 1], "zooming far enough reaches detail").toBe("detail");

    // Each level draws what it promises. Checked on the states actually visited rather than on a
    // re-derived expectation, so this fails if a level starts rendering the wrong scene.
    const lifetime = seen.find(function (entry) { return entry.lod === "lifetime"; });
    expect(lifetime.lifetimes, "the lifetime level keeps one block per agent").toBeGreaterThan(0);
    expect(lifetime.phases, "the lifetime level suppresses phases").toBe(0);
    expect(lifetime.strips, "the lifetime level suppresses state strips").toBe(0);
    expect(lifetime.bins, "the lifetime level draws no aggregate bins").toBe(0);

    expect(current.phases, "the detail level draws phases").toBeGreaterThan(0);
    expect(current.bins, "the detail level draws no aggregate bins").toBe(0);
  });
});
