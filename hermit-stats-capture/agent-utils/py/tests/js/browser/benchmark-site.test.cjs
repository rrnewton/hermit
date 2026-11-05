"use strict";

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

test("--help is dependency-light and documents the real-site interface", async function () {
  const result = await execFile(process.execPath, [benchmarkPath, "--help"]);
  assert.match(result.stdout, /Usage: node benchmark-site\.cjs/);
  assert.match(result.stdout, /--json <path>/);
  assert.match(result.stdout, /No metric budget is enforced/);
});

test("fixture smoke records load, resource, structure, heap, and wheel metrics", {
  timeout: 45_000
}, async function (context) {
  const port = await unusedPort();
  assert.ok(port > 0);
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
    path.join(os.tmpdir(), "agent-timeline-benchmark-")
  );
  context.after(function () {
    fs.rmSync(temporaryDirectory, { recursive: true, force: true });
  });
  const jsonPath = path.join(temporaryDirectory, "benchmark.json");
  const result = await execFile(
    process.execPath,
    [
      benchmarkPath,
      "--url",
      "http://127.0.0.1:" + port + "/",
      "--json",
      jsonPath,
      "--timeout-ms",
      "15000"
    ],
    { cwd: __dirname, maxBuffer: 5 * 1024 * 1024 }
  );
  const stdoutReport = JSON.parse(result.stdout);
  const fileReport = JSON.parse(fs.readFileSync(jsonPath, "utf8"));
  assert.deepEqual(fileReport, stdoutReport);
  assert.equal(stdoutReport.success, true);
  assert.equal(stdoutReport.benchmark, "wrkviz-real-site");
  assert.ok(stdoutReport.timings.navigation_domcontentloaded_ms >= 0);
  assert.ok(stdoutReport.timings.usable_ms >= 0);
  assert.ok(stdoutReport.payload_resources.resource_count > 0);
  assert.ok(stdoutReport.payload_resources.encoded_bytes > 0);
  assert.ok(stdoutReport.payload_resources.initial.resource_count > 0);
  assert.ok(stdoutReport.payload_resources.initial.encoded_bytes > 0);
  assert.ok(stdoutReport.timeline.initial.counts.dom_nodes > 0);
  assert.ok(stdoutReport.timeline.initial.counts.svg_nodes > 0);
  assert.ok(Object.hasOwn(stdoutReport.timeline.initial.counts, "state_strips"));
  assert.ok(Object.hasOwn(stdoutReport.timeline.initial.counts, "edges"));
  assert.ok(Object.hasOwn(stdoutReport.timeline.initial.counts, "aggregate_bins"));
  assert.ok(stdoutReport.js_heap.cdp.used_js_heap_bytes > 0);
  assert.ok(stdoutReport.js_heap.initial_cdp.used_js_heap_bytes > 0);
  assert.equal(stdoutReport.interaction.sample_count, 12);
  assert.equal(stdoutReport.interaction.input_to_raf_ms.count, 12);
  assert.ok(stdoutReport.interaction.input_to_raf_ms.p50 >= 0);
  assert.ok(stdoutReport.interaction.input_to_raf_ms.p95 >= 0);
  assert.ok(stdoutReport.interaction.input_to_raf_ms.max >= 0);
  assert.equal(stdoutReport.diagnostics.page_errors.length, 0);
});
