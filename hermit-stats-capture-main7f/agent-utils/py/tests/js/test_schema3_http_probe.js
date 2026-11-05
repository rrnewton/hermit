"use strict";

const assert = require("assert");
const childProcess = require("child_process");
const events = require("events");
const http = require("http");
const path = require("path");

const { fetchForNode, fetchWithHttp } = require("./schema3_http_probe.js");

function listen(server) {
  return new Promise(function (resolve, reject) {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", function () {
      server.removeListener("error", reject);
      resolve();
    });
  });
}

function close(server) {
  return new Promise(function (resolve, reject) {
    server.close(function (error) {
      if (error) {
        reject(error);
      } else {
        resolve();
      }
    });
  });
}

function nativeGlobalsArePreserved() {
  const probe = path.resolve(__dirname, "schema3_probe.js");
  const script = `
    const assert = require("assert");
    const blob = function Blob() {};
    const response = function Response() {};
    const decompression = function DecompressionStream() {};
    global.Blob = blob;
    global.Response = response;
    global.DecompressionStream = decompression;
    require(${JSON.stringify(probe)});
    assert.strictEqual(global.Blob, blob);
    assert.strictEqual(global.Response, response);
    assert.strictEqual(global.DecompressionStream, decompression);
  `;
  const result = childProcess.spawnSync(process.execPath, ["-e", script], {
    encoding: "utf8"
  });
  assert.strictEqual(result.status, 0, result.stdout + result.stderr);
}

async function httpFallbackReadsCompleteAndRejectsIncompleteResponses() {
  const server = http.createServer(function (request, response) {
    if (request.url === "/complete") {
      const body = Buffer.from("complete", "utf8");
      response.writeHead(200, {
        "Content-Length": String(body.byteLength),
        "X-Test": "present"
      });
      response.end(body);
      return;
    }
    response.writeHead(200, { "Content-Length": "20" });
    response.write("short", function () {
      response.destroy();
    });
  });
  await listen(server);
  const address = server.address();
  assert.notStrictEqual(address, null);
  assert.strictEqual(typeof address, "object");
  const base = "http://127.0.0.1:" + address.port;
  try {
    const complete = await fetchWithHttp(base + "/complete", {});
    assert.strictEqual(complete.status, 200);
    assert.strictEqual(complete.headers.get("x-test"), "present");
    assert.strictEqual(await complete.text(), "complete");

    const started = Date.now();
    let timer = null;
    const deadline = new Promise(function (_resolve, reject) {
      timer = setTimeout(function () {
        reject(new Error("incomplete HTTP response did not reject within two seconds"));
      }, 2000);
    });
    try {
      await assert.rejects(
        Promise.race([fetchWithHttp(base + "/incomplete", {}), deadline]),
        /before its declared body was complete/
      );
      assert.ok(Date.now() - started < 2000);
    } finally {
      clearTimeout(timer);
    }
  } finally {
    await close(server);
  }
}

async function httpFallbackRejectsResponseErrorsAndCleansUp() {
  const outgoing = new events.EventEmitter();
  outgoing.destroyed = false;
  outgoing.destroy = function () { outgoing.destroyed = true; };
  const response = new events.EventEmitter();
  response.complete = false;
  response.destroyed = false;
  response.headers = {};
  response.statusCode = 200;
  response.statusMessage = "OK";
  response.destroy = function () { response.destroyed = true; };
  outgoing.end = function () {
    process.nextTick(function () {
      outgoing.emit("response", response);
      process.nextTick(function () { response.emit("error", new Error("read failed")); });
    });
  };
  const requestFunction = function () { return outgoing; };

  await assert.rejects(
    fetchWithHttp("http://127.0.0.1/response-error", {}, requestFunction),
    /HTTP response failed before its declared body was complete: read failed/
  );
  assert.strictEqual(outgoing.destroyed, true);
  assert.strictEqual(response.destroyed, true);
  assert.strictEqual(outgoing.listenerCount("error"), 0);
  for (const event of ["data", "aborted", "error", "end", "close"]) {
    assert.strictEqual(response.listenerCount(event), 0, event);
  }
}

async function main() {
  nativeGlobalsArePreserved();

  let receiver = null;
  const native = function () {
    receiver = this;
    return Promise.resolve("native response");
  };
  assert.strictEqual(await fetchForNode(native)("unused"), "native response");
  assert.strictEqual(receiver, global);
  assert.strictEqual(fetchForNode(undefined), fetchWithHttp);

  await httpFallbackReadsCompleteAndRejectsIncompleteResponses();
  await httpFallbackRejectsResponseErrorsAndCleansUp();
  process.stdout.write("schema-3 HTTP probe suite passed\n");
}

main().catch(function (error) {
  process.stderr.write(String((error && error.stack) || error) + "\n");
  process.exitCode = 1;
});
