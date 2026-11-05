"use strict";

// Build, cache and serve a synthetic archive that is large enough to have levels of detail.
//
// The fixture beside this one is four agents in a single afternoon, and everything about it is
// deliberate: it is committed, it is readable, and it runs in seconds. What it cannot do is
// exercise the level-of-detail machinery, because at four agents over one afternoon every level
// draws everything. The three thresholds in `timeline-core.semanticZoomLevel` are therefore
// unreachable from that fixture, and were until now covered only by a manual run against a real
// archive that cannot be committed.
//
// So this module generates one instead. `wrkviz.synthetic` writes deterministic
// Claude-shaped transcripts -- coordinators forking subagents in overlapping waves, subagents
// forking their own, tool bursts, messages in both directions, work stopping overnight -- and the
// ordinary ingest/summarize/build path turns them into a real archive, served by the `serve.py`
// that ships inside it. Nothing here is mocked: the bytes the browser reads are the bytes a build
// produces.
//
// WHY IT IS GENERATED RATHER THAN COMMITTED. The archive is about 30 MB, most of it
// content-addressed shards and their gzip sidecars, and it changes whenever the builder changes.
// Committing that would put a generated binary tree in the repository and make every builder
// change a large diff of files nobody reads. Generating it costs about ten seconds, once,
// because the result is cached under a fingerprint of BOTH the requested size and the package
// that produced it -- so an edit to the builder invalidates the cache and an edit to an unrelated
// file does not.

const childProcess = require("node:child_process");
const fs = require("node:fs");
const net = require("node:net");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..", "..");
const PY_ROOT = path.join(REPO_ROOT, "py");
const CACHE_ROOT = path.join(__dirname, ".synthetic-archive");

//: The named size in `wrkviz.synthetic`. It is chosen there, not here, so the Python
//: checks and these browser checks cannot drift onto two different fixtures.
const PRESET = "ci";

function ensureSyntheticArchive(options) {
  const settings = options || {};
  const timeoutMs = settings.timeoutMs || 600_000;
  const result = childProcess.spawnSync(
    "python3",
    [
      "-m", "wrkviz.synthetic",
      "--out", CACHE_ROOT,
      "--preset", PRESET,
      "--build",
      "--reuse"
    ],
    {
      cwd: PY_ROOT,
      encoding: "utf8",
      timeout: timeoutMs,
      maxBuffer: 32 * 1024 * 1024,
      env: Object.assign({}, process.env, { PYTHONPATH: PY_ROOT })
    }
  );
  if (result.error) {
    throw new Error("could not generate the synthetic archive: " + result.error.message);
  }
  if (result.status !== 0) {
    throw new Error(
      "generating the synthetic archive failed (status=" + String(result.status) + ")\n" +
      "  stderr: " + (result.stderr || "<empty>").slice(-2000)
    );
  }
  const report = JSON.parse(result.stdout);
  const archive = path.join(CACHE_ROOT, "archive");
  if (!fs.existsSync(path.join(archive, "serve.py"))) {
    throw new Error("the synthetic archive has no serve.py: " + archive);
  }
  return { archive: archive, report: report };
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

async function waitForPort(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const reachable = await new Promise(function (resolve) {
      const socket = net.connect(port, "127.0.0.1");
      socket.once("connect", function () {
        socket.destroy();
        resolve(true);
      });
      socket.once("error", function () {
        resolve(false);
      });
    });
    if (reachable) {
      return;
    }
    await new Promise(function (resolve) { setTimeout(resolve, 100); });
  }
  throw new Error("the archive's serve.py did not start within " + timeoutMs + "ms");
}

async function startArchiveServer(archive) {
  const port = await unusedPort();
  // `detached`, so the server is killable as a process GROUP. A bare signal to the pid was
  // observed leaving servers behind in the manual benchmark that does the same thing.
  const child = childProcess.spawn(
    "python3",
    [path.join(archive, "serve.py"), "--port", String(port)],
    { cwd: archive, stdio: ["ignore", "pipe", "pipe"], detached: true }
  );
  try {
    await waitForPort(port, 30_000);
  } catch (error) {
    await stopArchiveServer(child);
    throw error;
  }
  return { child: child, baseUrl: "http://127.0.0.1:" + port + "/" };
}

async function stopArchiveServer(child) {
  if (!child || child.exitCode !== null) {
    return;
  }
  const exited = new Promise(function (resolve) { child.once("exit", resolve); });
  try {
    process.kill(-child.pid, "SIGTERM");
  } catch (_error) {
    // Already gone.
  }
  const done = await Promise.race([
    exited.then(function () { return true; }),
    new Promise(function (resolve) { setTimeout(function () { resolve(false); }, 5_000); })
  ]);
  if (!done) {
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch (_error) {
      // Already gone.
    }
  }
}

module.exports = {
  CACHE_ROOT: CACHE_ROOT,
  PRESET: PRESET,
  ensureSyntheticArchive: ensureSyntheticArchive,
  startArchiveServer: startArchiveServer,
  stopArchiveServer: stopArchiveServer
};
