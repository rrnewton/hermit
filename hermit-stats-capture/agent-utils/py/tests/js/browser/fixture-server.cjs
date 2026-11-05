"use strict";

const fs = require("fs");
const http = require("http");
const path = require("path");
const { URL } = require("url");
const { virtualFiles } = require("./fixture-data.cjs");

const staticRoot = path.resolve(
  __dirname,
  "..",
  "..",
  "..",
  "wrkviz",
  "static"
);
const staticPrefix = staticRoot + path.sep;

function parsePort(argv) {
  const index = argv.indexOf("--port");
  const candidate = index >= 0 ? Number(argv[index + 1]) : 41739;
  if (!Number.isInteger(candidate) || candidate < 1 || candidate > 65535) {
    throw new Error("invalid --port value");
  }
  return candidate;
}

function contentType(filePath) {
  const extension = path.extname(filePath).toLowerCase();
  return ({
    ".css": "text/css; charset=utf-8",
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".json": "application/json; charset=utf-8",
    ".md": "text/markdown; charset=utf-8",
    ".txt": "text/plain; charset=utf-8"
  })[extension] || "application/octet-stream";
}

function send(response, status, type, body, headOnly) {
  const payload = Buffer.isBuffer(body) ? body : Buffer.from(body, "utf8");
  response.writeHead(status, {
    "Cache-Control": "no-store",
    "Content-Length": payload.length,
    "Content-Type": type
  });
  response.end(headOnly ? undefined : payload);
}

function serve(request, response) {
  if (request.method !== "GET" && request.method !== "HEAD") {
    send(response, 405, "text/plain; charset=utf-8", "method not allowed\n", false);
    return;
  }
  const headOnly = request.method === "HEAD";
  const requestUrl = new URL(request.url || "/", "http://127.0.0.1");
  let pathname;
  try {
    pathname = decodeURIComponent(requestUrl.pathname);
  } catch (_error) {
    send(response, 400, "text/plain; charset=utf-8", "bad path\n", headOnly);
    return;
  }

  if (pathname === "/__health") {
    send(response, 200, "text/plain; charset=utf-8", "ok\n", headOnly);
    return;
  }
  const virtual = virtualFiles.get(pathname);
  if (virtual) {
    send(response, 200, virtual.contentType, virtual.body, headOnly);
    return;
  }

  const relativePath = pathname === "/" ? "index.html" : pathname.slice(1);
  const candidate = path.resolve(staticRoot, relativePath);
  if (candidate !== staticRoot && !candidate.startsWith(staticPrefix)) {
    send(response, 404, "text/plain; charset=utf-8", "not found\n", headOnly);
    return;
  }
  let body;
  try {
    body = fs.readFileSync(candidate);
  } catch (_error) {
    send(response, 404, "text/plain; charset=utf-8", "not found\n", headOnly);
    return;
  }
  send(response, 200, contentType(candidate), body, headOnly);
}

const port = parsePort(process.argv.slice(2));
const server = http.createServer(serve);
server.listen(port, "127.0.0.1", function () {
  process.stdout.write("timeline browser fixture: http://127.0.0.1:" + port + "\n");
});

function stop() {
  server.close(function () {
    process.exit(0);
  });
}

process.on("SIGINT", stop);
process.on("SIGTERM", stop);

