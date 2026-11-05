"use strict";

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const appPath = path.resolve(
  __dirname,
  "../../wrkviz/static/app.js"
);
const source = fs.readFileSync(appPath, "utf8");

function functionSource(name) {
  const marker = "  function " + name + "(";
  const start = source.indexOf(marker);
  assert.notStrictEqual(start, -1, "missing function " + name);
  const bodyStart = source.indexOf("{", start);
  let depth = 0;
  let quote = "";
  let escaped = false;
  for (let index = bodyStart; index < source.length; index += 1) {
    const character = source[index];
    if (quote) {
      if (escaped) {
        escaped = false;
      } else if (character === "\\") {
        escaped = true;
      } else if (character === quote) {
        quote = "";
      }
      continue;
    }
    if (character === "\"" || character === "'" || character === "`") {
      quote = character;
    } else if (character === "{") {
      depth += 1;
    } else if (character === "}") {
      depth -= 1;
      if (depth === 0) {
        return source.slice(start, index + 1);
      }
    }
  }
  throw new Error("unterminated function " + name);
}

const context = {
  URL: URL,
  window: {
    location: {
      href: "http://127.0.0.1:8765/index.html",
      origin: "http://127.0.0.1:8765"
    }
  }
};
vm.createContext(context);
vm.runInContext([
  "array",
  "text",
  "uniqueArtifactIds",
  "artifactAssociation",
  "safeArtifactTarget"
].map(functionSource).join("\n"), context);

const grouped = context.artifactAssociation({
  artifact_ids: ["artifact-pr38", "artifact-issue41", "artifact-pr38", "bad id"],
  output_artifact_ids: ["artifact-pr38", "artifact-commit42", "artifact-pr38"]
}, null);
assert.deepStrictEqual(
  Array.from(grouped.all),
  ["artifact-pr38", "artifact-issue41", "artifact-commit42"]
);
assert.deepStrictEqual(
  Array.from(grouped.outputs),
  ["artifact-pr38", "artifact-commit42"]
);
assert.deepStrictEqual(Array.from(grouped.references), ["artifact-issue41"]);

const external = context.safeArtifactTarget("https://github.com/example-org/dev-widget/pull/38");
assert.strictEqual(external.href, "https://github.com/example-org/dev-widget/pull/38");
assert.strictEqual(external.external, true);

const internal = context.safeArtifactTarget("#glossary/term-parser");
assert.strictEqual(internal.href, "http://127.0.0.1:8765/index.html#glossary/term-parser");
assert.strictEqual(internal.external, false);

assert.strictEqual(context.safeArtifactTarget("javascript:alert(1)"), null);
assert.strictEqual(context.safeArtifactTarget("file:///tmp/secret"), null);
assert.strictEqual(context.safeArtifactTarget("//attacker.example/artifact"), null);
assert.strictEqual(context.safeArtifactTarget("http://attacker.example/artifact"), null);
assert.strictEqual(context.safeArtifactTarget("https://token@example.com/artifact"), null);

assert.match(source, /link\.rel = "noopener noreferrer"/);
assert.match(source, /dataset\.artifactSection = role/);
assert.match(source, /openAgentLifetimeModal\(agent\)/);

console.log("artifact UI tests passed");
