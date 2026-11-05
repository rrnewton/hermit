#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const { performance } = require("perf_hooks");

const DEFAULT_TIMEOUT_MS = 30_000;

//: Where to sit before measuring. The default -- `fit` -- is what the page opens at: the whole
//: archive in one screen. The other two exist because a timeline's cost is not one number: the
//: renderer chooses a level of detail from milliseconds-per-pixel, so the SAME archive draws a
//: handful of aggregate bins fitted and thousands of individual phases at a day's width. A
//: benchmark that only ever measures one of those measures one third of the tool.
//:
//: The spans are the ones a reader actually navigates to, not round numbers: a week and a day.
const ZOOM_TARGETS = Object.freeze({
  fit: null,
  week: 7 * 24 * 60 * 60 * 1000,
  day: 24 * 60 * 60 * 1000,
  // An hour, and it is here because a day is NOT enough to reach the expensive level. The
  // `detail` threshold is 60s per pixel, and a day across a normal chart width is about 61 --
  // so a day-wide view lands in `lifetime` by a hair, and a run that stopped at `day` would
  // report the tool's performance without ever having drawn an individual phase.
  hour: 60 * 60 * 1000
});
const WHEEL_SEQUENCE = Object.freeze([
  { name: "zoom-in-1", kind: "zoom", deltaX: 0, deltaY: -360 },
  { name: "zoom-in-2", kind: "zoom", deltaX: 0, deltaY: -360 },
  { name: "zoom-in-3", kind: "zoom", deltaX: 0, deltaY: -360 },
  { name: "zoom-in-4", kind: "zoom", deltaX: 0, deltaY: -360 },
  { name: "pan-right-1", kind: "pan", deltaX: 180, deltaY: 0 },
  { name: "pan-right-2", kind: "pan", deltaX: 180, deltaY: 0 },
  { name: "pan-left-1", kind: "pan", deltaX: -180, deltaY: 0 },
  { name: "pan-left-2", kind: "pan", deltaX: -180, deltaY: 0 },
  { name: "zoom-out-1", kind: "zoom", deltaX: 0, deltaY: 360 },
  { name: "zoom-out-2", kind: "zoom", deltaX: 0, deltaY: 360 },
  { name: "zoom-out-3", kind: "zoom", deltaX: 0, deltaY: 360 },
  { name: "zoom-out-4", kind: "zoom", deltaX: 0, deltaY: 360 }
]);

const HELP = `Usage: node benchmark-site.cjs [options] <url>

Benchmark a served wrkviz site with a deterministic Chromium run.
The complete report is printed as JSON; --json also writes it to disk.

Options:
  --url <url>          Timeline URL (alternative to the positional URL)
  --json <path>        Also write the JSON report to this path
  --timeout-ms <ms>    Load/interaction timeout (default: ${DEFAULT_TIMEOUT_MS})
  --zoom <level>       Where to sit before measuring: fit (whole archive, the
                       default), week, or day. The renderer picks its level of
                       detail from milliseconds-per-pixel, so these measure
                       genuinely different rendering work on the same archive.
  --headed             Show Chromium while measuring
  -h, --help           Show this help

No metric budget is enforced. Exit status is nonzero only for invalid usage or
when the page cannot load, become usable, or execute without a runtime failure.
`;

function parseArguments(argv) {
  const options = {
    url: "",
    jsonPath: "",
    timeoutMs: DEFAULT_TIMEOUT_MS,
    headed: false,
    zoom: "fit",
    help: false
  };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "-h" || argument === "--help") {
      options.help = true;
    } else if (argument === "--headed") {
      options.headed = true;
    } else if (argument === "--zoom") {
      const value = argv[index + 1];
      if (!value) {
        throw new Error("--zoom requires a value");
      }
      index += 1;
      if (!Object.prototype.hasOwnProperty.call(ZOOM_TARGETS, value)) {
        throw new Error(
          "--zoom must be one of " + Object.keys(ZOOM_TARGETS).join(", ") + "; got " + value
        );
      }
      options.zoom = value;
    } else if (argument === "--url" || argument === "--json" || argument === "--timeout-ms") {
      const value = argv[index + 1];
      if (!value) {
        throw new Error(argument + " requires a value");
      }
      index += 1;
      if (argument === "--url") {
        if (options.url) {
          throw new Error("timeline URL was provided more than once");
        }
        options.url = value;
      } else if (argument === "--json") {
        options.jsonPath = value;
      } else {
        const timeout = Number(value);
        if (!Number.isInteger(timeout) || timeout < 1_000) {
          throw new Error("--timeout-ms must be an integer of at least 1000");
        }
        options.timeoutMs = timeout;
      }
    } else if (argument.startsWith("-")) {
      throw new Error("unknown option: " + argument);
    } else if (options.url) {
      throw new Error("timeline URL was provided more than once");
    } else {
      options.url = argument;
    }
  }
  if (!options.help) {
    if (!options.url) {
      throw new Error("a timeline URL is required");
    }
    let parsed;
    try {
      parsed = new URL(options.url);
    } catch (_error) {
      throw new Error("timeline URL is invalid: " + options.url);
    }
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      throw new Error("timeline URL must use http or https");
    }
    options.url = parsed.href;
  }
  return options;
}

function rounded(value) {
  return Number.isFinite(value) ? Number(value.toFixed(3)) : null;
}

function percentile(values, proportion) {
  if (!values.length) {
    return null;
  }
  const sorted = values.slice().sort(function (left, right) { return left - right; });
  const index = Math.max(0, Math.ceil(proportion * sorted.length) - 1);
  return sorted[index];
}

function distribution(values) {
  const finite = values.filter(Number.isFinite);
  return {
    count: finite.length,
    p50: rounded(percentile(finite, 0.50)),
    p95: rounded(percentile(finite, 0.95)),
    max: rounded(finite.length ? Math.max.apply(null, finite) : NaN)
  };
}

async function waitForUsableTimeline(page, timeoutMs) {
  const handle = await page.waitForFunction(function () {
    const loadError = document.querySelector("#load-error");
    if (loadError && !loadError.hidden && loadError.textContent.trim()) {
      return { status: "error", message: loadError.textContent.trim() };
    }
    const timeline = document.querySelector('[data-testid="timeline"]');
    const svg = document.querySelector("#timeline-svg");
    const metadata = document.querySelector("#dataset-meta");
    if (!timeline || !svg || !metadata || /Loading timeline/i.test(metadata.textContent || "")) {
      return false;
    }
    const start = Number(timeline.getAttribute("data-view-start-ms"));
    const end = Number(timeline.getAttribute("data-view-end-ms"));
    const revision = Number(timeline.getAttribute("data-render-revision"));
    if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start ||
        !Number.isFinite(revision) || revision < 1) {
      return false;
    }
    return { status: "ready" };
  }, null, { timeout: timeoutMs });
  const result = await handle.jsonValue();
  if (result.status !== "ready") {
    throw new Error("timeline reported a load failure: " + result.message);
  }
}

async function timelineSnapshot(page) {
  return page.evaluate(function () {
    function numericAttribute(element, name) {
      const value = Number(element && element.getAttribute(name));
      return Number.isFinite(value) ? value : null;
    }
    const timeline = document.querySelector('[data-testid="timeline"]');
    const svg = document.querySelector("#timeline-svg");
    if (!timeline || !svg) {
      throw new Error("usable timeline elements disappeared");
    }
    return {
      lod: timeline.getAttribute("data-render-lod") ||
        svg.getAttribute("data-render-lod") || "unknown",
      render_duration_ms: numericAttribute(timeline, "data-render-duration-ms"),
      render_revision: numericAttribute(timeline, "data-render-revision"),
      view_start_ms: numericAttribute(timeline, "data-view-start-ms"),
      view_end_ms: numericAttribute(timeline, "data-view-end-ms"),
      track_mode: svg.getAttribute("data-track-mode") || "unknown",
      aggregate_resolution: svg.getAttribute("data-aggregate-resolution") || null,
      counts: {
        dom_nodes: document.querySelectorAll("*").length,
        svg_nodes: svg.querySelectorAll("*").length,
        state_strips: svg.querySelectorAll(".state-strip").length,
        edges: svg.querySelectorAll(".edge-group").length,
        phases: svg.querySelectorAll(".phase-group").length,
        agent_lifetimes: svg.querySelectorAll(".agent-lifetime-group").length,
        aggregate_bins: svg.querySelectorAll(".activity-bin-group").length
      }
    };
  });
}

async function currentSpanMs(page) {
  return page.evaluate(function () {
    const timeline = document.querySelector('[data-testid="timeline"]');
    const start = Number(timeline && timeline.getAttribute("data-view-start-ms"));
    const end = Number(timeline && timeline.getAttribute("data-view-end-ms"));
    return Number.isFinite(start) && Number.isFinite(end) ? end - start : NaN;
  });
}


async function zoomToSpan(page, targetSpanMs, timeoutMs) {
  // Zoom in until the visible span is at or below `targetSpanMs`.
  //
  // Driven by the same wheel events a reader uses, and verified against the view range the page
  // publishes, rather than by reaching into the app for a setter. That keeps this honest in the way
  // that matters for a benchmark: the cost being measured includes whatever the real zoom path
  // costs, and the harness cannot accidentally put the page into a state a user could not reach.
  //
  // The seek steps are NOT part of the measurement -- they run before sampling begins -- so their
  // cost does not contaminate the numbers for the level being measured.
  const MAX_STEPS = 60;
  const before = await currentSpanMs(page);
  if (!Number.isFinite(before)) {
    throw new Error("the timeline published no view range to zoom from");
  }
  let span = before;
  let steps = 0;
  while (span > targetSpanMs && steps < MAX_STEPS) {
    await wheelSample(page, { name: "seek", kind: "zoom", deltaX: 0, deltaY: -360 }, timeoutMs);
    const next = await currentSpanMs(page);
    if (!Number.isFinite(next) || next >= span) {
      // The view stopped narrowing: either a floor was reached or the wheel stopped being
      // honoured. Either way, continuing would spin for MAX_STEPS learning nothing, and
      // reporting the span actually reached is more useful than failing.
      break;
    }
    span = next;
    steps += 1;
  }
  return { requested_span_ms: targetSpanMs, reached_span_ms: span, initial_span_ms: before, steps: steps };
}


//: Every wheel sample gets its own key, and the page's sample array is never cleared, so the key
//: has to be unique for the LIFE of the run rather than for the phase it is in. Numbering each
//: phase from zero was a silent corruption: `zoomToSpan` seeks with keys 0..n before measuring
//: begins, so the measured samples 0..11 found the SEEK samples already in the array and returned
//: instantly with their latencies. Every level that seeks at all -- which is every level but the
//: fitted one -- reported up to twelve stale numbers taken while the view was somewhere else.
let wheelSampleSequence = 0;

async function wheelSample(page, action, timeoutMs) {
  const key = "wheel-" + (wheelSampleSequence += 1);
  const selector = "#time-axis";
  const target = page.locator(selector);
  const box = await target.boundingBox();
  if (!box) {
    throw new Error("wheel benchmark target is not visible: " + selector);
  }
  await page.mouse.move(box.x + box.width * 0.62, box.y + box.height / 2);
  const beforeRevision = await page.evaluate(function () {
    const timeline = document.querySelector('[data-testid="timeline"]');
    return Number(timeline && timeline.getAttribute("data-render-revision"));
  });
  await page.evaluate(function (settings) {
    const targetElement = document.querySelector(settings.selector);
    if (!targetElement) {
      throw new Error("missing wheel benchmark target");
    }
    window.__agentTimelineBenchmarkSamples =
      window.__agentTimelineBenchmarkSamples || [];
    targetElement.addEventListener("wheel", function (event) {
      const handlerAt = performance.now();
      requestAnimationFrame(function (frameAt) {
        let inputToRaf = frameAt - event.timeStamp;
        if (!Number.isFinite(inputToRaf) || inputToRaf < 0 || inputToRaf > 60_000) {
          inputToRaf = null;
        }
        window.__agentTimelineBenchmarkSamples.push({
          key: settings.key,
          input_to_raf_ms: inputToRaf,
          handler_to_raf_ms: frameAt - handlerAt
        });
      });
    }, { once: true, passive: true });
  }, { selector: selector, key: key });

  await page.mouse.wheel(action.deltaX, action.deltaY);
  const sampleHandle = await page.waitForFunction(function (sampleKey) {
    const samples = window.__agentTimelineBenchmarkSamples || [];
    return samples.find(function (sample) { return sample.key === sampleKey; }) || false;
  }, key, { timeout: Math.min(timeoutMs, 5_000) });
  const sample = await sampleHandle.jsonValue();

  let renderChanged = false;
  try {
    await page.waitForFunction(function (revision) {
      const timeline = document.querySelector('[data-testid="timeline"]');
      return Number(timeline && timeline.getAttribute("data-render-revision")) > revision;
    }, beforeRevision, { timeout: Math.min(timeoutMs, 1_000) });
    renderChanged = true;
  } catch (_error) {
    // A clamped zoom or pan can legitimately leave the view unchanged.
  }
  const after = await timelineSnapshot(page);
  return {
    name: action.name,
    kind: action.kind,
    delta_x: action.deltaX,
    delta_y: action.deltaY,
    input_to_raf_ms: rounded(sample.input_to_raf_ms),
    handler_to_raf_ms: rounded(sample.handler_to_raf_ms),
    render_changed: renderChanged,
    render_revision: after.render_revision,
    render_duration_ms: after.render_duration_ms,
    lod: after.lod
  };
}

function summarizeNetwork(records) {
  const byType = {};
  let encodedBytes = 0;
  let failedCount = 0;
  records.forEach(function (record) {
    const bytes = Number.isFinite(record.encodedDataLength) ? record.encodedDataLength : 0;
    encodedBytes += bytes;
    const type = String(record.type || "Other").toLowerCase();
    byType[type] = (byType[type] || 0) + bytes;
    if (record.failed) {
      failedCount += 1;
    }
  });
  return {
    resource_count: records.size,
    encoded_bytes: Math.round(encodedBytes),
    encoded_bytes_by_type: byType,
    failed_resource_count: failedCount
  };
}

async function browserPerformance(page) {
  return page.evaluate(function () {
    function finite(value) {
      return Number.isFinite(value) ? value : null;
    }
    const navigation = performance.getEntriesByType("navigation")[0];
    const resources = performance.getEntriesByType("resource");
    let transferSize = 0;
    let encodedBodySize = 0;
    let decodedBodySize = 0;
    resources.forEach(function (entry) {
      transferSize += entry.transferSize || 0;
      encodedBodySize += entry.encodedBodySize || 0;
      decodedBodySize += entry.decodedBodySize || 0;
    });
    return {
      navigation: navigation ? {
        response_end_ms: finite(navigation.responseEnd),
        dom_content_loaded_ms: finite(navigation.domContentLoadedEventEnd),
        load_event_ms: finite(navigation.loadEventEnd)
      } : null,
      resource_entries: {
        count: resources.length,
        transfer_bytes: transferSize,
        encoded_body_bytes: encodedBodySize,
        decoded_body_bytes: decodedBodySize
      },
      performance_memory: performance.memory ? {
        used_js_heap_bytes: performance.memory.usedJSHeapSize,
        total_js_heap_bytes: performance.memory.totalJSHeapSize,
        js_heap_limit_bytes: performance.memory.jsHeapSizeLimit
      } : null
    };
  });
}

async function cdpHeapUsage(session) {
  try {
    const usage = await session.send("Runtime.getHeapUsage");
    return {
      used_js_heap_bytes: Math.round(usage.usedSize),
      total_js_heap_bytes: Math.round(usage.totalSize),
      embedded_bytes: Math.round(usage.embedderHeapUsedSize || 0),
      backing_storage_bytes: Math.round(usage.backingStorageSize || 0)
    };
  } catch (_error) {
    return null;
  }
}

async function benchmarkSite(options) {
  const { chromium } = require("@playwright/test");
  const pageErrors = [];
  const consoleErrors = [];
  const failedRequests = [];
  const networkRecords = new Map();
  let browser = null;
  const base = {
    schema_version: 1,
    benchmark: "wrkviz-real-site",
    generated_at: new Date().toISOString(),
    url: options.url,
    viewport: { width: 1440, height: 900 },
    success: false
  };
  try {
    browser = await chromium.launch({ headless: !options.headed });
    const context = await browser.newContext({
      viewport: base.viewport,
      deviceScaleFactor: 1
    });
    const page = await context.newPage();
    page.on("pageerror", function (error) {
      pageErrors.push(error.stack || error.message || String(error));
    });
    page.on("console", function (message) {
      if (message.type() === "error") {
        consoleErrors.push(message.text());
      }
    });
    page.on("requestfailed", function (request) {
      failedRequests.push({
        url: request.url(),
        error: request.failure() ? request.failure().errorText : "unknown"
      });
    });

    const session = await context.newCDPSession(page);
    await session.send("Network.enable");
    session.on("Network.responseReceived", function (event) {
      networkRecords.set(event.requestId, {
        url: event.response.url,
        type: event.type,
        status: event.response.status,
        encodedDataLength: 0,
        failed: false
      });
    });
    session.on("Network.loadingFinished", function (event) {
      const record = networkRecords.get(event.requestId);
      if (record) {
        record.encodedDataLength = event.encodedDataLength;
      }
    });
    session.on("Network.loadingFailed", function (event) {
      const record = networkRecords.get(event.requestId);
      if (record) {
        record.failed = true;
      }
    });

    const startedAt = performance.now();
    const response = await page.goto(options.url, {
      waitUntil: "domcontentloaded",
      timeout: options.timeoutMs
    });
    const navigationMs = performance.now() - startedAt;
    if (!response) {
      throw new Error("navigation did not produce an HTTP response");
    }
    if (response.status() >= 400) {
      throw new Error("navigation returned HTTP " + response.status());
    }
    await waitForUsableTimeline(page, options.timeoutMs);
    const usableMs = performance.now() - startedAt;
    try {
      await page.waitForLoadState("networkidle", {
        timeout: Math.min(options.timeoutMs, 5_000)
      });
    } catch (_error) {
      // A usable timeline is sufficient; background traffic is diagnostic only.
    }

    // Seek to the requested level BEFORE the initial snapshot, so `timeline.initial` describes
    // the state actually being measured rather than the fitted view the page opened at.
    const zoomTargetMs = ZOOM_TARGETS[options.zoom];
    const fittedSpanMs = await currentSpanMs(page);
    const zoomSeek = zoomTargetMs === null
      ? {
          requested_span_ms: null,
          reached_span_ms: fittedSpanMs,
          initial_span_ms: fittedSpanMs,
          steps: 0
        }
      : await zoomToSpan(page, zoomTargetMs, options.timeoutMs);

    const initial = await timelineSnapshot(page);
    const initialWebPerformance = await browserPerformance(page);
    const initialNetwork = Object.assign(
      summarizeNetwork(new Map(networkRecords)),
      initialWebPerformance.resource_entries
    );
    const initialHeap = await cdpHeapUsage(session);
    const samples = [];
    for (let index = 0; index < WHEEL_SEQUENCE.length; index += 1) {
      samples.push(await wheelSample(
        page,
        WHEEL_SEQUENCE[index],
        options.timeoutMs
      ));
    }
    await page.waitForTimeout(100);
    const final = await timelineSnapshot(page);
    const webPerformance = await browserPerformance(page);
    const heap = await cdpHeapUsage(session);
    const inputLatencies = samples.map(function (sample) { return sample.input_to_raf_ms; });
    const handlerLatencies = samples.map(function (sample) { return sample.handler_to_raf_ms; });
    const report = Object.assign({}, base, {
      success: pageErrors.length === 0,
      zoom: Object.assign({ level: options.zoom }, zoomSeek),
      timings: {
        navigation_domcontentloaded_ms: rounded(navigationMs),
        usable_ms: rounded(usableMs),
        browser_navigation: webPerformance.navigation
      },
      timeline: {
        initial: initial,
        final: final
      },
      payload_resources: Object.assign(
        summarizeNetwork(networkRecords),
        webPerformance.resource_entries,
        { initial: initialNetwork }
      ),
      js_heap: {
        cdp: heap,
        performance_memory: webPerformance.performance_memory,
        initial_cdp: initialHeap,
        initial_performance_memory: initialWebPerformance.performance_memory
      },
      interaction: {
        sequence: "four zoom-in, two pan-right, two pan-left, four zoom-out wheel events",
        sample_count: samples.length,
        input_to_raf_ms: distribution(inputLatencies),
        handler_to_raf_ms: distribution(handlerLatencies),
        render_change_count: samples.filter(function (sample) {
          return sample.render_changed;
        }).length,
        samples: samples
      },
      diagnostics: {
        page_errors: pageErrors,
        console_errors: consoleErrors,
        failed_requests: failedRequests
      }
    });
    if (pageErrors.length) {
      report.failure = {
        kind: "runtime",
        message: "page raised " + pageErrors.length + " uncaught runtime error(s)"
      };
    }
    return report;
  } catch (error) {
    return Object.assign({}, base, {
      failure: {
        kind: "load-or-runtime",
        message: error instanceof Error ? error.message : String(error)
      },
      diagnostics: {
        page_errors: pageErrors,
        console_errors: consoleErrors,
        failed_requests: failedRequests
      }
    });
  } finally {
    if (browser) {
      await browser.close();
    }
  }
}

function writeReport(report, jsonPath) {
  const content = JSON.stringify(report, null, 2) + "\n";
  process.stdout.write(content);
  if (jsonPath) {
    const destination = path.resolve(jsonPath);
    fs.mkdirSync(path.dirname(destination), { recursive: true });
    fs.writeFileSync(destination, content, "utf8");
  }
}

async function main(argv) {
  let options;
  try {
    options = parseArguments(argv);
  } catch (error) {
    process.stderr.write("benchmark-site: " + error.message + "\n\n" + HELP);
    process.exitCode = 2;
    return;
  }
  if (options.help) {
    process.stdout.write(HELP);
    return;
  }
  const report = await benchmarkSite(options);
  try {
    writeReport(report, options.jsonPath);
  } catch (error) {
    process.stderr.write("benchmark-site: could not write report: " + error.message + "\n");
    process.exitCode = 2;
    return;
  }
  if (!report.success) {
    process.exitCode = 1;
  }
}

module.exports = {
  benchmarkSite: benchmarkSite,
  distribution: distribution,
  parseArguments: parseArguments
};

if (require.main === module) {
  main(process.argv.slice(2)).catch(function (error) {
    process.stderr.write("benchmark-site: " + (error.stack || error.message) + "\n");
    process.exitCode = 1;
  });
}
