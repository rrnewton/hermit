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
  const next = source.indexOf("\n  function ", start + marker.length);
  assert.notStrictEqual(next, -1, "missing function boundary after " + name);
  return source.slice(start, next);
}

const names = [
  "text",
  "number",
  "hasField",
  "validateSchema2ObjectSourceDigest",
  "asciiLowerUtf8SearchBytes",
  "asciiLowerSearchText",
  "searchTextTrigrams",
  "queryTermBloomEligible",
  "fnv1a32",
  "bloomBitPositions",
  "decodeBase64Bytes",
  "decodeTrigramBloom",
  "trigramBloomMightContain",
  "trigramBloomMightMatchQuery",
  "transcriptSearchRecordCount",
  "validateTranscriptSearchShard",
  "loadSearchItemsBounded",
  "transcriptRecordMatchesScope",
  "searchRoleLabel",
  "compactSearchText",
  "searchQueryParts",
  "allSearchRanges",
  "searchWordCharacter",
  "searchCharacterBefore",
  "searchCharacterAt",
  "wholeSearchRanges",
  "smartSearchMatch",
  "searchExcerpt"
];
const context = {
  TextEncoder: global.TextEncoder,
  atob: global.atob,
  TRIGRAM_BLOOM_ALGORITHM: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
  TRIGRAM_BLOOM_HASH_COUNT: 7,
  SEARCH_EXCERPT_CHARACTERS: 480,
  SEARCH_LOAD_CONCURRENCY: 6,
  FNV_OFFSET: 2166136261,
  FNV_PRIME: 16777619,
  SECOND_HASH_SEED: 0x9e3779b9,
  app: {
    searchScope: "agent-responses",
    data: { source_digest: "digest-alpha" },
    agentsById: new Map([["root", { id: "root", team: "alpha" }]]),
    searchLoadQueue: [],
    searchLoadActive: 0,
    searchRequestGeneration: 1
  }
};
vm.createContext(context);
vm.runInContext(names.map(functionSource).join("\n"), context);

assert.strictEqual(
  context.compactSearchText("\u001c  Alpha\u0085BETA\u3000"),
  "Alpha BETA"
);
assert.strictEqual(context.compactSearchText("foo\ufeffbar"), "foo\ufeffbar");
assert.strictEqual(
  JSON.stringify(context.searchQueryParts("foo\ufeffbar")),
  JSON.stringify([{ value: "foo\ufeffbar", quoted: false }])
);
assert.deepStrictEqual(
  Array.from(context.asciiLowerUtf8SearchBytes("AZ az Ä")),
  [97, 122, 32, 97, 122, 32, 195, 132]
);
assert.deepStrictEqual(
  context.searchTextTrigrams("  Alpha\n\tBETA  ").map(function (value) {
    return Array.from(value);
  }),
  context.searchTextTrigrams("alpha beta").map(function (value) {
    return Array.from(value);
  })
);
assert.strictEqual(context.queryTermBloomEligible("backend"), true);
assert.strictEqual(context.queryTermBloomEligible("B3"), false);
assert.strictEqual(context.queryTermBloomEligible("Réverie"), false);

const abcBloom = context.decodeTrigramBloom({
  algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
  bit_count: 64,
  hash_count: 7,
  bits_base64: "AQoQgAAEIAA=",
  trigram_count: 1
}, "abc bloom");
assert.strictEqual(context.trigramBloomMightContain(abcBloom, "ABC"), true);
assert.strictEqual(context.trigramBloomMightContain(abcBloom, "zzzzzz"), false);

const backendBloom = context.decodeTrigramBloom({
  algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
  bit_count: 128,
  hash_count: 7,
  bits_base64: "VIrlptEOBb6e2UEwG9lmrQ==",
  trigram_count: 11
}, "backend bloom");
const ptraceBloom = context.decodeTrigramBloom({
  algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
  bit_count: 128,
  hash_count: 7,
  bits_base64: "CpJJrKcNEMHROrAlwPydPQ==",
  trigram_count: 10
}, "ptrace bloom");
assert.strictEqual(context.trigramBloomMightMatchQuery(backendBloom, "backend"), true);
assert.strictEqual(context.trigramBloomMightMatchQuery(backendBloom, "zzzzzz"), false);
assert.strictEqual(
  context.trigramBloomMightMatchQuery(backendBloom, "backend zzzzzz"),
  false,
  "each eligible smart-search term must be Bloom-positive"
);
assert.strictEqual(
  context.trigramBloomMightMatchQuery(backendBloom, "backend B3"),
  true,
  "an ineligible term cannot reject a shard that matches another eligible term"
);
assert.strictEqual(
  context.trigramBloomMightMatchQuery(ptraceBloom, "backend B3"),
  false,
  "the eligible term still rejects a shard when another term is too short"
);
assert.strictEqual(
  context.trigramBloomMightMatchQuery(ptraceBloom, "backend Réverie"),
  false,
  "the eligible term still rejects a shard when another term is non-ASCII"
);
assert.strictEqual(
  context.trigramBloomMightMatchQuery(ptraceBloom, "B3 Réverie"),
  true,
  "a query with no eligible terms scans every shard"
);
assert.throws(function () {
  context.decodeTrigramBloom({
    algorithm: "unknown",
    bit_count: 64,
    hash_count: 7,
    bits_base64: "AQoQgAAEIAA=",
    trigram_count: 1
  }, "bad bloom");
}, /unsupported trigram Bloom filter/);
assert.throws(function () {
  context.decodeTrigramBloom({
    algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
    bit_count: 65,
    hash_count: 7,
    bits_base64: "AQoQgAAEIAA=",
    trigram_count: 1
  }, "bad bloom");
}, /power of two/);
assert.throws(function () {
  context.decodeTrigramBloom({
    algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
    bit_count: 64,
    hash_count: 8,
    bits_base64: "AQoQgAAEIAA=",
    trigram_count: 1
  }, "bad bloom");
}, /expected 7/);
assert.throws(function () {
  context.decodeTrigramBloom({
    algorithm: "ascii-lower-utf8-trigram-fnv1a32-double-v1",
    bit_count: 64,
    hash_count: 7,
    bits_base64: "not base64!",
    trigram_count: 1
  }, "bad bloom");
}, /invalid base64/);

assert.ok(context.smartSearchMatch("DBI is a solid B3 at 130/152.", "B3"));
assert.strictEqual(
  context.smartSearchMatch("KVM backend", "KVM"),
  null,
  "ASCII-insensitive exact matching must stay aligned with the Bloom normalization"
);
assert.strictEqual(
  context.smartSearchMatch("RÉVERIE", "réverie"),
  null,
  "non-ASCII code points remain exact while ASCII letters are folded"
);
assert.strictEqual(
  context.smartSearchMatch("artifact hash 7ab3cdef was recorded", "B3"),
  null,
  "whole-term matching must not confuse a hash fragment with maturity level B3"
);
assert.strictEqual(
  context.smartSearchMatch("backend level is B30", "B3"),
  null,
  "whole-term matching must reject longer alphanumeric tokens"
);
assert.strictEqual(context.smartSearchMatch("éB3é", "B3"), null);
assert.strictEqual(context.smartSearchMatch("αbackendβ", "backend"), null);
assert.strictEqual(context.smartSearchMatch("𐐀B3𐐀", "B3"), null);

assert.ok(
  context.smartSearchMatch("KVM backend maturity is now B3.", "KVM B3"),
  "unquoted query terms use AND semantics"
);
assert.strictEqual(
  context.smartSearchMatch("KVM backend maturity remains B2.", "KVM B3"),
  null
);
assert.ok(
  context.smartSearchMatch(
    "The DBI backend passed more than fifty percent of ptrace tests.",
    "\"fifty percent\" DBI"
  ),
  "quoted phrases stay contiguous while combining with other terms"
);

const longText = "prefix ".repeat(40) + "KVM reached maturity B3" + " suffix".repeat(50);
const match = context.smartSearchMatch(longText, "KVM B3");
assert.ok(match);
const excerpt = context.searchExcerpt(match);
assert.strictEqual(excerpt.truncated, true);
assert.ok(excerpt.leadingOmitted > 0);
assert.ok(excerpt.trailingOmitted > 0);
assert.ok(excerpt.ranges.length >= 2);
assert.ok(excerpt.text.includes("KVM reached maturity B3"));
assert.ok(excerpt.text.length <= 480);

const distantMatch = context.smartSearchMatch(
  "B3" + "x".repeat(10000) + " B3",
  "B3"
);
assert.ok(distantMatch);
assert.ok(context.searchExcerpt(distantMatch).text.length <= 480);

assert.strictEqual(context.transcriptRecordMatchesScope({ record_type: "response" }), true);
assert.strictEqual(
  context.transcriptRecordMatchesScope({ record_type: "inter_agent_response" }),
  true
);
assert.strictEqual(
  context.transcriptRecordMatchesScope({ record_type: "inter_agent" }),
  false,
  "ambiguous inter-agent chatter is not presented as a completed response"
);
assert.strictEqual(
  context.transcriptRecordMatchesScope({ record_type: "inter_agent_prompt" }),
  false
);
assert.strictEqual(context.searchRoleLabel({ role: "system" }), "system");
assert.strictEqual(context.searchRoleLabel({ role: "event" }), "event");

const catalogEntry = {
  team: "alpha",
  start_ms: 100,
  end_ms: 200,
  counts: { records: 1 }
};
const validRecord = {
  schema_version: 1,
  ref: "message:alpha::final",
  record_type: "inter_agent_response",
  role: "agent",
  team: "alpha",
  agent_id: "root",
  agent_ref: "agent:alpha::root",
  event_id: "final",
  at_ms: 150,
  text: "Final response"
};
const validShard = {
  schema_version: 1,
  kind: "timeline-search-day",
  team: "alpha",
  range: { start_ms: 100, end_ms: 200 },
  records: [validRecord]
};
assert.strictEqual(
  context.validateTranscriptSearchShard(validShard, catalogEntry, "valid.json"),
  validShard.records
);

const boundShard = JSON.parse(JSON.stringify(validShard));
boundShard.source_digest = "digest-alpha";
assert.strictEqual(
  context.validateTranscriptSearchShard(boundShard, catalogEntry, "bound.json"),
  boundShard.records
);
const staleShard = JSON.parse(JSON.stringify(validShard));
staleShard.source_digest = "digest-beta";
assert.throws(function () {
  context.validateTranscriptSearchShard(staleShard, catalogEntry, "stale.json");
}, /source digest does not match the timeline generation/);

function copy(value) {
  return JSON.parse(JSON.stringify(value));
}

const wrongShardTeam = copy(validShard);
wrongShardTeam.team = "beta";
assert.throws(function () {
  context.validateTranscriptSearchShard(wrongShardTeam, catalogEntry, "team.json");
}, /does not match its catalog entry/);

const wrongRange = copy(validShard);
wrongRange.range.end_ms = 201;
assert.throws(function () {
  context.validateTranscriptSearchShard(wrongRange, catalogEntry, "range.json");
}, /does not match its catalog entry/);

assert.throws(function () {
  context.validateTranscriptSearchShard(
    validShard,
    Object.assign({}, catalogEntry, { counts: { records: 2 } }),
    "count.json"
  );
}, /does not match its catalog entry/);

[
  ["team", "beta"],
  ["ref", "message:beta::final"],
  ["agent_id", "missing-agent"],
  ["agent_ref", "agent:alpha::missing-agent"],
  ["event_id", "different"],
  ["role", "mystery"],
  ["prompt_ref", "message:beta::prompt"]
].forEach(function (mutation) {
  const invalid = copy(validShard);
  invalid.records[0][mutation[0]] = mutation[1];
  assert.throws(function () {
    context.validateTranscriptSearchShard(invalid, catalogEntry, mutation[0] + ".json");
  }, /invalid record/);
});

(async function () {
  let active = 0;
  let maximum = 0;
  function delayedLoader(value) {
    return new Promise(function (resolve) {
      active += 1;
      maximum = Math.max(maximum, active);
      setTimeout(function () {
        active -= 1;
        resolve(value);
      }, 5);
    });
  }
  await Promise.all([
    context.loadSearchItemsBounded(
      Array.from({ length: 12 }, function (_value, index) { return "a" + index; }),
      delayedLoader,
      1
    ),
    context.loadSearchItemsBounded(
      Array.from({ length: 12 }, function (_value, index) { return "b" + index; }),
      delayedLoader,
      1
    )
  ]);
  assert.ok(maximum <= 6, "search loading must share one global concurrency bound");
  console.log("timeline transcript search UI tests passed");
}()).catch(function (error) {
  console.error(error);
  process.exitCode = 1;
});
