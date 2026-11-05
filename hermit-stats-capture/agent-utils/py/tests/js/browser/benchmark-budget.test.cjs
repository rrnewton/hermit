"use strict";

// Zoom and pan redraw latency, as a PASS/FAIL check that also reports what it measured.
//
// `benchmark-site.test.cjs` beside this file asserts that the harness *records* metrics -- that
// every field is present and non-negative. That is a check on the instrument. This is the check
// on the thing being measured: it drives the same twelve-step wheel sequence (four zoom-in, two
// pan-right, two pan-left, four zoom-out) and fails when a redraw takes longer than the budget.
//
// Both halves are needed and they are not redundant. A harness that silently stopped sampling
// would keep passing here -- every latency under budget, because there are none -- which is why
// the sample count is asserted too, and why the instrument test exists separately.
//
// WHAT IS GATED, AND WHAT IS ONLY REPORTED. These are deliberately different, because wall-clock
// timing on a shared CI runner is not a property worth gating on. A hosted runner is variously
// throttled, co-tenanted and cold; the same commit can measure five times apart between two runs,
// so a timing bound tight enough to catch a real regression will fire on a slow neighbour
// instead. That produces the worst outcome available: a red build nobody believes, on a signal
// nobody can act on.
//
// So the gate is split.
//
//   STRICT, and machine-independent. Did the page load without a runtime error; did the wheel
//   sequence produce the number of samples it should; did those events actually change the
//   rendering. "Zoom does nothing" is a real defect, it is what a broken interaction looks like,
//   and detecting it does not require knowing how fast the machine is. This is the half that
//   earns its place as a gate.
//
//   GENEROUS, and wall-clock. The timing bounds below are COMPLETELY-BROKEN detectors, not
//   regression detectors. Measured p95 on a developer machine is about 32ms and the bound is
//   5000ms -- more than two orders of magnitude of headroom -- because the question they answer
//   is "did something catastrophic happen to rendering", not "is this slower than last week".
//   A change that doubles redraw cost will not trip them, and that is the intended trade: this
//   file is not the instrument for spotting a 2x regression.
//
// THE NUMBERS ARE STILL RECORDED, on every run, whatever the verdict. That is the part that is
// actually useful over time: a series you can look at, in CI as a build artifact and locally as
// `benchmark-report.txt`. Watching a trend is how a gradual regression gets caught -- a threshold
// only ever tells you that you have already lost, and on CI hardware it mostly tells you which
// runner you drew.
//
// ONE METRIC IS PRINTED BUT NOT ENFORCED AT ALL. `handler_to_raf_ms` -- the page's own handler
// time, excluding the browser's input plumbing -- measures as NEGATIVE on this fixture (about
// -0.1ms), because the harness captures the rAF timestamp before the handler is observed to
// finish. A budget over a quantity that reads below zero is a gate that cannot fail, which is
// worse than no gate: it looks like coverage. It stays in the table because the trend is still
// informative, and gets no assertion until the measurement is fixed.

const assert = require("node:assert/strict");
const childProcess = require("node:child_process");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");
const util = require("node:util");

const execFile = util.promisify(childProcess.execFile);
const benchmarkPath = path.join(__dirname, "benchmark-site.cjs");
const fixtureServerPath = path.join(__dirname, "fixture-server.cjs");

//: Wall-clock ceilings, in milliseconds. COMPLETELY-BROKEN detectors; see the header for why
//: they are this loose and why nothing tighter belongs in a CI gate.
const BUDGET = Object.freeze({
  // ~150x the measured developer-machine p95. A redraw taking five seconds is broken by any
  // standard, on any hardware, and nothing short of that is distinguishable from a slow runner.
  input_to_raf_p95_ms: 5_000,
  input_to_raf_max_ms: 10_000,
  // A minute to the first usable frame. Includes Chromium start on a cold, throttled runner.
  usable_ms: 60_000
});

//: The wheel sequence is twelve steps; a run that sampled fewer measured something else.
const EXPECTED_SAMPLES = 12;

async function unusedPort() {
  return new Promise(function (resolve, reject) {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", function () {
      const address = server.address();
      const port = typeof address === "object" && address ? address.port : 0;
      server.close(function (error) {
        if (error) {
          reject(error);
        } else {
          resolve(port);
        }
      });
    });
  });
}

async function waitForFixture(server) {
  return new Promise(function (resolve, reject) {
    let output = "";
    const timeout = setTimeout(function () {
      reject(new Error("fixture server did not become ready; output: " + output));
    }, 10_000);
    function finish(error) {
      clearTimeout(timeout);
      server.stdout.removeListener("data", onData);
      server.removeListener("exit", onExit);
      if (error) {
        reject(error);
      } else {
        resolve();
      }
    }
    function onData(chunk) {
      output += chunk.toString("utf8");
      if (output.includes("timeline browser fixture:")) {
        finish(null);
      }
    }
    function onExit(code) {
      finish(new Error("fixture server exited before readiness with status " + code));
    }
    server.stdout.on("data", onData);
    server.once("exit", onExit);
  });
}

function reportMeasurements(report) {
  const input = report.interaction.input_to_raf_ms;
  const handler = report.interaction.handler_to_raf_ms;
  const rows = [
    ["first usable frame", report.timings.usable_ms, BUDGET.usable_ms],
    ["zoom/pan input->frame p50", input.p50, null],
    ["zoom/pan input->frame p95", input.p95, BUDGET.input_to_raf_p95_ms],
    ["zoom/pan input->frame max", input.max, BUDGET.input_to_raf_max_ms],
    ["page handler->frame p95", handler.p95, null]
  ];
  const lines = rows.map(function (row) {
    const budget = row[2] === null ? "" : "  (budget " + row[2] + "ms)";
    return "    " + row[0].padEnd(28) + String(row[1]).padStart(8) + "ms" + budget;
  });
  // Written to a file as well as printed, and the file is the load-bearing half. Under
  // `dagrun box` -- which is how this runs in `make validate` -- a passing step's stdout is
  // summarised to one line, so a table that only went to the console would be visible exactly
  // when nobody needed it and swallowed the rest of the time. The file also gives the numbers
  // somewhere to accumulate: a budget tells you when you have already lost, a series tells you
  // that you are heading there.
  const table = [
    "timeline redraw benchmark",
    "sequence: " + report.interaction.sequence,
    ...rows.map(function (row) {
      const budget = row[2] === null ? "" : "  (budget " + row[2] + "ms)";
      return "  " + row[0].padEnd(28) + String(row[1]).padStart(8) + "ms" + budget;
    })
  ].join("\n") + "\n";
  fs.writeFileSync(path.join(__dirname, "benchmark-report.txt"), table, "utf8");
  // The full report too, not just the table. The text is for a person reading the terminal; the
  // JSON is what a later comparison actually needs -- every sample, the node counts, the heap,
  // the resource totals. Both land in the checkout, both are gitignored.
  fs.writeFileSync(
    path.join(__dirname, "benchmark-report.json"),
    JSON.stringify(report, null, 2) + "\n",
    "utf8"
  );

  // Printed on every run, not only on failure: a number you can only see when it is already too
  // late is not a measurement, it is an alarm.
  console.log(
    [
      "",
      "  timeline redraw benchmark (" + report.interaction.sequence + ")",
      ...lines,
      "    " + "frames that changed".padEnd(28) +
        String(report.interaction.render_change_count).padStart(8) + "/" +
        report.interaction.sample_count,
      "    " + "DOM / SVG nodes".padEnd(28) +
        String(report.timeline.final.counts.dom_nodes) + " / " +
        String(report.timeline.final.counts.svg_nodes),
      "    " + "JS heap".padEnd(28) +
        String(Math.round(report.js_heap.cdp.used_js_heap_bytes / 1048576)).padStart(8) + "MiB",
      ""
    ].join("\n")
  );
}

test("zoom and pan redraw within budget, and the measurements are reported", {
  timeout: 120_000
}, async function (context) {
  const port = await unusedPort();
  const server = childProcess.spawn(
    process.execPath,
    [fixtureServerPath, "--port", String(port)],
    { cwd: __dirname, stdio: ["ignore", "pipe", "pipe"] }
  );
  context.after(function () {
    if (!server.killed) {
      server.kill("SIGTERM");
    }
  });
  await waitForFixture(server);

  const temporaryDirectory = fs.mkdtempSync(
    path.join(os.tmpdir(), "agent-timeline-budget-")
  );
  context.after(function () {
    fs.rmSync(temporaryDirectory, { recursive: true, force: true });
  });
  const jsonPath = path.join(temporaryDirectory, "benchmark.json");
  const result = await execFile(
    process.execPath,
    [
      benchmarkPath,
      "--url", "http://127.0.0.1:" + port + "/",
      "--json", jsonPath,
      "--timeout-ms", "30000"
    ],
    { cwd: __dirname, maxBuffer: 5 * 1024 * 1024 }
  );
  const report = JSON.parse(result.stdout);

  reportMeasurements(report);

  assert.equal(report.success, true, "the page raised a runtime error while being measured");
  assert.deepEqual(report.diagnostics.page_errors, []);

  // --- strict, and independent of how fast the machine is ------------------------------------
  //
  // A harness that stopped sampling would otherwise satisfy every timing bound below by
  // measuring nothing at all.
  assert.equal(
    report.interaction.sample_count,
    EXPECTED_SAMPLES,
    "the wheel sequence did not produce the expected number of samples"
  );
  assert.equal(report.interaction.input_to_raf_ms.count, EXPECTED_SAMPLES);
  // The real interaction gate: every wheel event is supposed to change what is on screen. Zero
  // changed frames means zoom and pan do nothing -- a defect no timing bound would catch, and
  // one that needs no knowledge of the hardware to detect.
  assert.ok(
    report.interaction.render_change_count > 0,
    "no wheel event changed the rendering: zoom and pan are not redrawing at all"
  );

  // --- generous, and only for the catastrophic case --------------------------------------------
  const input = report.interaction.input_to_raf_ms;
  assert.ok(
    input.p95 <= BUDGET.input_to_raf_p95_ms,
    `zoom/pan p95 redraw was ${input.p95}ms against a ${BUDGET.input_to_raf_p95_ms}ms ` +
      "completely-broken ceiling; this is not a regression bound, so breaching it means " +
      "rendering is catastrophically slow rather than merely slower"
  );
  assert.ok(
    input.max <= BUDGET.input_to_raf_max_ms,
    `slowest zoom/pan redraw was ${input.max}ms against a ${BUDGET.input_to_raf_max_ms}ms ceiling`
  );
  assert.ok(
    report.timings.usable_ms <= BUDGET.usable_ms,
    `first usable frame took ${report.timings.usable_ms}ms against a ${BUDGET.usable_ms}ms ceiling`
  );
});
