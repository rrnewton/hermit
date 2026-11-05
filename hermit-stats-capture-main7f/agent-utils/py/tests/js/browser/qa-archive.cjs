#!/usr/bin/env node
"use strict";

// Benchmark a REAL generated archive at four zoom levels, using its own `serve.py`.
//
// The suite beside this runs against a four-agent fixture. That fixture is the right thing to
// gate on -- it is deterministic, it is committed, and it runs in twenty seconds -- but it cannot
// answer the question this file exists for: does the tool stay responsive on an archive with
// thousands of agents and hundreds of thousands of tool calls. Nothing about a four-agent page
// exercises the level-of-detail machinery, because at that size every level renders everything.
//
// So this is manual QA, deliberately not a gate: it needs an archive the repository does not
// contain and must never contain. Point it at one you have built.
//
//     node qa-archive.cjs --archive ~/logs/summary/myproject
//
// If you have no captured archive of that size, generate one: `make -C ../../../wrkviz
// synth-archive OUT=/tmp/synth PRESET=large` writes deterministic transcripts and builds them
// through the ordinary path. `synthetic-scale.spec.js` gates on a small version of the same
// thing; the large sizes exist for exactly this file.
//
// WHY THREE LEVELS. `timeline-core.semanticZoomLevel` picks a level of detail from
// milliseconds-per-pixel -- `detail` at or under a minute per pixel, `lifetime` at or under five,
// `aggregate` beyond -- so one archive draws three substantially different scenes depending only
// on how far out the reader is. Measuring the fitted view alone measures the cheapest of them and
// reports it as the tool's performance. The three levels here are chosen to land in the three
// regimes on a normal window: a fitted multi-week archive is `aggregate`, a day is `detail`, and
// a week is usually the boundary.
//
// The archive's OWN `serve.py` is what serves it, not a fixture server, because byte-range
// answers over the schema-3 shards are part of what is being measured and that is the server
// that ships inside the archive.

const childProcess = require("node:child_process");
const fs = require("node:fs");
const http = require("node:http");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");

//: Four, not three. `day` lands in `lifetime` rather than `detail` on a normal window -- a day
//: across ~1400px is about 61s per pixel against a 60s threshold -- so `hour` is what actually
//: exercises the level that draws individual phases, which is the expensive one.
//:
//: WHAT HAPPENED TO THE "FOURTH LEVEL" FAILURE, and why there is no retry here any more. This
//: file used to record a load that timed out at 120s with no page error, no console error and no
//: failed request, and to retry each level once to work around it. The retry is gone, because a
//: retry standing around a failure nobody has explained hides the next one.
//:
//: One mechanism that produces EXACTLY that signature was found and is fixed below: the server's
//: log went to a pipe this process could not read, and a full pipe stops `serve.py` answering
//: without killing it or refusing a connection. See `measure`, where it is described and where
//: the numbers behind it are recorded.
//:
//: What is NOT established is that this was the failure recorded here, and it should not be
//: claimed. Against a 13,760-phase, 2,656-agent, 264,719-tool-call archive the recorded failure
//: did not recur once in fourteen consecutive four-level runs -- fifty-six levels -- taken before
//: any change, so there was no live instance left to attribute. Two facts bound it. That archive
//: logs 39-72 requests per level and the pipe holds about 962, a thirteenfold margin, so the
//: deadlock cannot fire on an archive this quiet; and the site inside a rebuilt archive is not
//: the site the original run measured, so the request pattern that mattered may no longer exist.
//: If a load ever times out here again, the error now carries a probe of the server taken before
//: teardown and the tail of that server's log, which is what the original investigation lacked.
const LEVELS = ["fit", "week", "day", "hour"];

function parseArguments(argv) {
  const options = { archive: "", jsonPath: "", timeoutMs: 120_000, levels: LEVELS.slice() };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "-h" || argument === "--help") {
      options.help = true;
    } else if (argument === "--levels") {
      const value = argv[index + 1];
      if (!value) {
        throw new Error("--levels requires a comma-separated list");
      }
      index += 1;
      options.levels = value.split(",").map(function (item) { return item.trim(); });
      const unknown = options.levels.filter(function (item) { return LEVELS.indexOf(item) < 0; });
      if (unknown.length) {
        throw new Error("unknown level(s): " + unknown.join(", "));
      }
    } else if (argument === "--archive" || argument === "--json" || argument === "--timeout-ms") {
      const value = argv[index + 1];
      if (!value) {
        throw new Error(argument + " requires a value");
      }
      index += 1;
      if (argument === "--archive") {
        options.archive = value;
      } else if (argument === "--json") {
        options.jsonPath = value;
      } else {
        options.timeoutMs = Number(value);
      }
    } else if (!options.archive) {
      options.archive = argument;
    } else {
      throw new Error("unexpected argument: " + argument);
    }
  }
  return options;
}

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

function hasExited(child) {
  return child.exitCode !== null || child.signalCode !== null;
}

async function stopGroup(child) {
  // Signal the GROUP (negative pid), then wait, then SIGKILL the group. Signalling the pid alone
  // leaves anything the server spawned, and leaves the server itself if it declines SIGTERM.
  // `python3` here is a launcher that forks the real interpreter rather than exec'ing it, so the
  // pid Node holds is not the pid that is listening -- the group is what has to be addressed.
  //
  // A child that has ALREADY exited is left alone, for two reasons. It has been reaped, so its
  // pid is free for the kernel to reissue and `kill(-pid)` would be aimed at whoever holds it
  // next; and its `exit` event has already fired, so waiting for another one would spend the
  // full eight seconds below learning nothing.
  if (hasExited(child)) {
    return;
  }
  const done = new Promise(function (resolve) { child.once("exit", resolve); });
  try {
    process.kill(-child.pid, "SIGTERM");
  } catch (_error) {
    // Already gone.
  }
  const exited = await Promise.race([
    done.then(function () { return true; }),
    new Promise(function (resolve) { setTimeout(function () { resolve(false); }, 5_000); })
  ]);
  if (!exited) {
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch (_error) {
      // Already gone.
    }
    await Promise.race([
      done,
      new Promise(function (resolve) { setTimeout(resolve, 3_000); })
    ]);
  }
}


async function waitForServer(port, timeoutMs, child) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (child && hasExited(child)) {
      throw new Error(
        "the archive's serve.py exited before it began listening (status=" +
        String(child.exitCode) + " signal=" + String(child.signalCode) + ")"
      );
    }
    const reachable = await new Promise(function (resolve) {
      const socket = net.connect(port, "127.0.0.1");
      socket.once("connect", function () {
        socket.destroy();
        resolve(true);
      });
      socket.once("error", function () {
        socket.destroy();
        resolve(false);
      });
    });
    if (reachable) {
      return;
    }
    await new Promise(function (resolve) { setTimeout(resolve, 200); });
  }
  throw new Error("the archive's serve.py did not start within " + timeoutMs + "ms");
}

//: Ask the server for the entry point and say, in one line, what happened. Run when a level has
//: just failed and BEFORE the server is torn down, because "did the server answer at the moment
//: the page gave up" is the question that separates a stalled server from a stalled page, and it
//: is unanswerable afterwards.
async function probeServer(port, timeoutMs) {
  const startedAt = Date.now();
  return new Promise(function (resolve) {
    const request = http.get(
      { host: "127.0.0.1", port: port, path: "/", agent: false },
      function (response) {
        response.resume();
        response.once("end", function () {
          resolve(
            "answered HTTP " + response.statusCode + " in " + (Date.now() - startedAt) + "ms"
          );
        });
      }
    );
    request.setTimeout(timeoutMs, function () {
      request.destroy(new Error("timeout"));
      resolve(
        "DID NOT ANSWER within " + timeoutMs + "ms -- the server has stopped serving, so the " +
        "page was waiting on it rather than on itself"
      );
    });
    request.once("error", function (error) {
      resolve("connection failed: " + (error && error.message ? error.message : String(error)));
    });
  });
}

function tailOf(logPath, lines) {
  let text;
  try {
    text = fs.readFileSync(logPath, "utf8");
  } catch (error) {
    return "    <unreadable: " + (error && error.message ? error.message : error) + ">";
  }
  const all = text.split("\n").filter(function (line) { return line.length > 0; });
  const kept = all.slice(-lines).map(function (line) { return "    " + line; });
  return (
    "    (" + all.length + " logged line(s) in total)\n" +
    (kept.length ? kept.join("\n") : "    <the server logged nothing>")
  );
}

function runBenchmark(url, level, timeoutMs) {
  const result = childProcess.spawnSync(
    process.execPath,
    [
      path.join(__dirname, "benchmark-site.cjs"),
      "--url", url,
      "--zoom", level,
      "--timeout-ms", String(timeoutMs)
    ],
    { cwd: __dirname, encoding: "utf8", maxBuffer: 32 * 1024 * 1024 }
  );
  if (result.error) {
    throw new Error("benchmark could not run at zoom=" + level + ": " + result.error.message);
  }
  if (result.status !== 0 || result.signal) {
    // Status AND signal AND both streams. Reporting only stderr hid a killed child behind an
    // empty message: a process terminated by a signal has status null and says nothing, which
    // reads as "failed for no reason".
    throw new Error(
      "benchmark failed at zoom=" + level +
      " (status=" + String(result.status) + " signal=" + String(result.signal) + ")" +
      "\n  stderr: " + (result.stderr || "<empty>").slice(-1500) +
      "\n  stdout: " + (result.stdout || "<empty>").slice(-500)
    );
  }
  return JSON.parse(result.stdout);
}

function formatSpan(ms) {
  if (!Number.isFinite(ms)) {
    return "?";
  }
  const hours = ms / 3_600_000;
  if (hours >= 48) {
    return (hours / 24).toFixed(1) + "d";
  }
  return hours.toFixed(1) + "h";
}

function renderTable(reports) {
  const header =
    "  " + "level".padEnd(7) + "span".padStart(8) + "  " + "lod".padEnd(10) +
    "tracks".padEnd(11) + "svg".padStart(7) + "edges".padStart(7) + "bins".padStart(7) +
    "draw".padStart(9) + "p50".padStart(9) + "p95".padStart(9) + "max".padStart(9);
  const rows = reports.map(function (entry) {
    const report = entry.report;
    const shape = report.timeline.initial;
    const input = report.interaction.input_to_raf_ms;
    return "  " +
      entry.level.padEnd(7) +
      formatSpan(report.zoom.reached_span_ms).padStart(8) + "  " +
      String(shape.lod).padEnd(10) +
      String(shape.track_mode).padEnd(11) +
      String(shape.counts.svg_nodes).padStart(7) +
      String(shape.counts.edges).padStart(7) +
      String(shape.counts.aggregate_bins).padStart(7) +
      (String(shape.render_duration_ms) + "ms").padStart(9) +
      (String(input.p50) + "ms").padStart(9) +
      (String(input.p95) + "ms").padStart(9) +
      (String(input.max) + "ms").padStart(9);
  });
  return [
    "",
    "  archive redraw benchmark -- zoom/pan latency by level of detail",
    "  draw = the page's own last render; p50/p95/max = wheel event to the frame reflecting it",
    "",
    header,
    "  " + "-".repeat(header.length - 2),
    ...rows,
    ""
  ].join("\n");
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  if (options.help || !options.archive) {
    process.stdout.write(
      "Usage: node qa-archive.cjs --archive <built archive directory> [options]\n\n" +
      "  --levels a,b,c   subset of: " + LEVELS.join(", ") + " (default: all)\n" +
      "  --json <path>    also write the full per-level reports as JSON\n\n" +
      "Benchmarks zoom/pan redraw by level of detail against a real archive, served by\n" +
      "the archive's own serve.py. Manual QA: not a gate, and it needs an archive this\n" +
      "repository does not ship.\n"
    );
    process.exitCode = options.help ? 0 : 2;
    return;
  }
  const archive = path.resolve(options.archive);
  const server = path.join(archive, "serve.py");
  if (!fs.existsSync(server)) {
    throw new Error("not a built archive (no serve.py): " + archive);
  }

  // Somewhere for the servers to log. One file per level, removed when the whole run succeeds
  // and named in the error when it does not.
  const logDirectory = fs.mkdtempSync(path.join(os.tmpdir(), "wrkviz-qa-"));

  // A FRESH server per level, not one shared across the run. The reason that matters for the
  // numbers is the HTTP cache: a shared server let level N load from level N-1's warm cache, so
  // the later rows measured a state a real reader would not have and the rows were not
  // comparable with each other.
  async function measure(level) {
    const port = await unusedPort();
    const logPath = path.join(logDirectory, "serve-" + level + ".log");
    const logFd = fs.openSync(logPath, "w");
    let child;
    try {
      // THE SERVER'S OUTPUT GOES TO A FILE, NEVER TO A PIPE, and that is load-bearing rather
      // than tidy. `serve.py` logs every non-index request to stderr, and a pipe nobody reads
      // holds 64KiB. The parent cannot read it: it spends the whole of a level blocked inside
      // `spawnSync` waiting for the benchmark, so its event loop -- and with it any drain of the
      // pipe -- is stopped for exactly the window in which the requests happen. When the buffer
      // fills, `serve.py` blocks in `write`, and because `sys.stderr` is one lock-protected
      // stream every other request thread queues behind it: the server stops answering, without
      // dying and without ever refusing a connection.
      //
      // That failure is invisible from the browser's side. The page's pending fetches simply
      // never settle, so there is no page error, no console error and no failed request -- only
      // a load that times out. Measured against this archive's own `serve.py`: with a pipe the
      // 963rd request was never answered; with a file 3,000 requests and 251KB of log went
      // through untouched. Attaching a `data` listener is NOT the fix -- with the parent inside
      // `spawnSync` the listener never runs, and the same server wedged after 172 requests.
      //
      // A real archive today logs 39-72 lines per level, so there is roughly a thirteenfold
      // margin and this is a trap rather than a daily failure. It is still the cheapest possible
      // fix for the most confusing possible symptom, and a noisier archive walks straight into
      // it.
      //
      // `detached`, so the server can be killed as a process GROUP. A bare SIGTERM to the pid was
      // observed leaving servers alive -- four at once during a four-level run that should never
      // have had more than one.
      child = childProcess.spawn("python3", [server, "--port", String(port)], {
        cwd: archive,
        stdio: ["ignore", logFd, logFd],
        detached: true
      });
    } finally {
      // The child has its own duplicate; holding this one open only keeps the file alive.
      fs.closeSync(logFd);
    }
    try {
      await waitForServer(port, 30_000, child);
      return runBenchmark("http://127.0.0.1:" + port + "/", level, options.timeoutMs);
    } catch (error) {
      // Diagnose BEFORE the teardown, while the failing state still exists. Whether the server
      // answers right now is the one fact that decides where to look next, and killing it first
      // throws that fact away.
      const probe = await probeServer(port, 5_000);
      error.message +=
        "\n  server probe: " + probe +
        "\n  server log " + logPath + ":\n" + tailOf(logPath, 12);
      throw error;
    } finally {
      await stopGroup(child);
      // Let the port and the browser teardown settle before the next level starts.
      await new Promise(function (resolve) { setTimeout(resolve, 3_000); });
    }
  }

  const reports = [];
  for (const level of options.levels) {
    process.stderr.write("  measuring zoom=" + level + "...\n");
    reports.push({ level: level, report: await measure(level) });
  }
  fs.rmSync(logDirectory, { recursive: true, force: true });

  const table = renderTable(reports);
  process.stdout.write(table);
  if (options.jsonPath) {
    fs.writeFileSync(
      options.jsonPath,
      JSON.stringify({ archive: archive, levels: reports }, null, 2) + "\n",
      "utf8"
    );
  }
  fs.writeFileSync(path.join(__dirname, "qa-archive-report.txt"), table + "\n", "utf8");
}

main().catch(function (error) {
  process.stderr.write("qa-archive: " + (error && error.message ? error.message : error) + "\n");
  process.exitCode = 1;
});
