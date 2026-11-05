"use strict";

// The schema-3 reader out of `static/app.js`, lifted into a Node realm so it can be tested.
//
// `app.js` is one IIFE that starts by resolving DOM nodes, so it cannot be `require`d: the very
// first thing it does on load is fail. The suites in this directory have always answered that by
// slicing named functions out of the source text and running them against a stub `app`, and this
// file is that trick factored out, because two suites now need the same twenty-odd functions --
// `test_timeline_v3_ui.js` against synthetic fixtures, and `schema3_http_probe.js` against a real
// archive served by the real `standalone_server.py`.
//
// The extraction is textual and therefore fragile in one specific way: it depends on this
// repository's formatting, where a top-level function inside the IIFE opens at indent two and
// closes with a line that is exactly two spaces and a brace. That is checked rather than assumed
// -- `assertExtractionIsSound` below re-parses every slice and fails loudly if one of them did not
// come out as a complete function -- so a reformat breaks the suite with a message that says so
// instead of silently testing a truncated body.

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const zlib = require("zlib");

// The shipped page runs in a browser, while these tests deliberately run its reader functions
// under whatever Node.js the developer already has. Node.js 16 has the required standard-library
// pieces but does not expose all three browser interfaces as globals. Supply only what is absent,
// so newer Node.js versions continue to exercise their native implementations and older versions
// still execute app.js's own inflateGzipMember body instead of skipping it.
if (typeof global.Blob !== "function") {
  global.Blob = require("buffer").Blob;
}
if (typeof global.Response !== "function") {
  global.Response = class Response {
    constructor(body) {
      this.body = body;
    }

    async arrayBuffer() {
      const reader = this.body.getReader();
      const chunks = [];
      for (;;) {
        const item = await reader.read();
        if (item.done) {
          break;
        }
        chunks.push(Buffer.from(item.value));
      }
      const buffer = Buffer.concat(chunks);
      return buffer.buffer.slice(buffer.byteOffset, buffer.byteOffset + buffer.byteLength);
    }
  };
}
if (typeof global.DecompressionStream !== "function") {
  const { TransformStream } = require("stream/web");
  global.DecompressionStream = class DecompressionStream {
    constructor(format) {
      assert.strictEqual(format, "gzip");
      const chunks = [];
      const stream = new TransformStream({
        transform(chunk) {
          chunks.push(Buffer.from(chunk));
        },
        flush(controller) {
          controller.enqueue(zlib.gunzipSync(Buffer.concat(chunks)));
        }
      });
      this.readable = stream.readable;
      this.writable = stream.writable;
    }
  };
}

const APP_PATH = path.resolve(
  __dirname,
  "..",
  "..",
  "wrkviz",
  "static",
  "app.js"
);

const source = fs.readFileSync(APP_PATH, "utf8");

function functionSource(name) {
  const opener = new RegExp("\\n  (?:async )?function " + name + "\\(");
  const match = opener.exec(source);
  assert.notStrictEqual(match, null, "app.js has no function " + name);
  const start = match.index + 1;
  const end = source.indexOf("\n  }\n", start);
  assert.notStrictEqual(end, -1, "app.js function " + name + " has no closing brace at indent 2");
  return source.slice(start, end + 5);
}

function constantSource(name) {
  const opener = new RegExp("\\n  var " + name + " = [^\\n]*\\n");
  const match = opener.exec(source);
  assert.notStrictEqual(match, null, "app.js has no constant " + name);
  return match[0].slice(1);
}

//: Constants the reader compares against rather than discovers. Taken from the bundle so that a
//: change to the index format shows up here as a failure and not as a passing test of a stale rule.
const CONSTANTS = [
  "SCHEMA_2_URL",
  "SCHEMA_3_URL",
  "SCHEMA_3_INDEX_FORMAT",
  "SCHEMA_3_INDEX_VERSION",
  "SCHEMA_3_INDEX_SUFFIX",
  "SCHEMA_3_ROOT",
  "SCHEMA_3_MEMBER_CONCURRENCY",
  "SCHEMA_3_DAY_MS",
  "SCHEMA_2_CORPUS_ABSENT",
  "SEARCH_RESULT_LIMIT",
  "SEARCH_EXCERPT_CHARACTERS",
  "SEARCH_LOAD_CONCURRENCY",
  "schema3MemberSlots",
  "TRIGRAM_BLOOM_ALGORITHM",
  "TRIGRAM_BLOOM_HASH_COUNT",
  "FNV_OFFSET",
  "FNV_PRIME",
  "SECOND_HASH_SEED"
];

//: Shared narrowing helpers, then the schema-3 reader itself.
const FUNCTIONS = [
  "array",
  "number",
  "text",
  "hasField",
  "immutableTimelineObjectUrl",
  "compactSearchText",
  "searchQueryParts",
  "asciiLowerUtf8SearchBytes",
  "searchTextTrigrams",
  "queryTermBloomEligible",
  "fnv1a32",
  "bloomBitPositions",
  "decodeBase64Bytes",
  "decodeTrigramBloom",
  "trigramBloomMightContain",
  "trigramBloomMightMatchQuery",
  "schema3Enabled",
  "resetSchema3State",
  "shardedMode",
  "shardKey",
  "schema3SafeRelativePath",
  "schema3Integer",
  "schema3ShardEntry",
  "schema3Stream",
  "parseChunkIndex",
  "loadChunkIndex",
  "membersForTimeRange",
  "membersForLineRange",
  "inflateGzipMember",
  "parseContentRangeTotal",
  "fetchShardMember",
  "boundedAll",
  "readShardRecords",
  "readShardKind",
  "schema3RecordsOfKind",
  "schema3Payload",
  "fetchJsonCached",
  "loadSchema3",
  "spineFirstPaintRanges",
  "ensurePhaseCards",
  "installPhaseCard",
  "sortPhaseIndex",
  "schema3InstallSpine",
  "schema3LocalIdentifier",
  "activityBoundsRef",
  "schema3DayRange",
  "schema3SearchCatalog",
  "queryCanUseBloom",
  "errorMessage",
  //: The page-wide member gate. Extracted with the rest because `fetchShardMember` calls it on
  //: every request, so leaving it out does not weaken a test -- it stops the reader loading.
  "schema3MemberSlot",
  //: The four schema-3 loaders, by their own entry points. Before this list grew, every suite
  //: reached past them to `readShardKind`/`readShardRecords` and hand-rolled what they do, so a
  //: regression inside one of them -- a dropped half of the links shard, a swapped line range,
  //: a prefilter installed under the wrong key -- was invisible to the whole tree while the
  //: assertions about refs and excerpts went on passing against the hand-rolled copy.
  "ensureActivityBounds",
  "boundsTeam",
  "loadPhaseIndex",
  "ensureSearchBlooms",
  "ensureSearchLinks",
  "loadSchema3SearchShard",
  //: The schema-2 half of the search corpus, which a schema-3 archive built before the search
  //: streams still reads. Both the parser and the fallback that decides to use it.
  "transcriptSearchRecordCount",
  "validateSchema2ObjectSourceDigest",
  "validateTranscriptSearchShard",
  "mergeTranscriptSearchShard",
  "schema2SearchCatalog",
  "schema3Schema2SearchFallback",
  //: The search itself, as the page runs it. `updateTranscriptSearch` is the real matcher, and
  //: the reason it is here is that a probe which substitutes `indexOf` for it is not testing the
  //: browser's answers, it is testing its own. See `schema3_http_probe.js`.
  "searchShardMightMatch",
  "searchShardAt",
  "fetchContentAddressedJson",
  "loadTranscriptSearchShard",
  "mergeTranscriptSearchLinkage",
  "validateTranscriptSearchLinkage",
  "loadTranscriptSearchLinkage",
  "loadSearchItemsBounded",
  "requestTranscriptSearchCorpus",
  "requestTranscriptSearchShards",
  "reportTranscriptSearchFailure",
  "transcriptSearchNeedsLoad",
  "transcriptSearchShards",
  "schema3SearchTeams",
  "prepareTranscriptSearchPrefilter",
  "transcriptSearchActive",
  "transcriptRecordMatchesScope",
  "selectedTeamAllows",
  "asciiLowerSearchText",
  "allSearchRanges",
  "searchCharacterAt",
  "searchCharacterBefore",
  "searchWordCharacter",
  "wholeSearchRanges",
  "smartSearchMatch",
  "searchExcerpt",
  "updateTranscriptSearch"
];

//: The DOM boundary, stubbed rather than avoided.
//:
//: `updateTranscriptSearch` is the page's real matcher and it ends by painting; `loadPhaseIndex`
//: and `mergeTranscriptSearchShard` end by updating the diagnostics dataset. Extracting them
//: without these three would fail on `dom`, which is why earlier suites stopped short of them and
//: re-implemented what they do. Stubbing the three named here is the smaller lie: the functions
//: under test run their own bodies, and only the paint is replaced -- the same trade
//: `initializeData` already made.
const DOM_BOUNDARY = ["updateShardDiagnostics", "renderTranscriptSearchResults", "scheduleRender"];

function assertExtractionIsSound(name, slice) {
  assert.ok(
    slice.startsWith("  function " + name + "(") ||
      slice.startsWith("  async function " + name + "("),
    "extraction of " + name + " did not start at its declaration"
  );
  assert.ok(slice.endsWith("\n  }\n"), "extraction of " + name + " did not end at indent 2");
}

//: A fresh reader bound to a fresh `app` state.
//:
//: `new Function` rather than `vm.runInContext`, so the extracted code runs in this realm and
//: reaches the same `fetch`, `Blob`, `Response`, `DecompressionStream` and `TextDecoder` a browser
//: would. The point of these suites is that the bundle's own bytes do the reading; giving them a
//: hand-built context would be testing the context.
function loadReader(overrides) {
  const parts = CONSTANTS.map(constantSource);
  FUNCTIONS.forEach(function (name) {
    const slice = functionSource(name);
    assertExtractionIsSound(name, slice);
    parts.push(slice);
  });
  const app = Object.assign(
    {
      schemaMode: "schema3",
      query: "",
      selectedTeam: "",
      shardIndexPromises: new Map(),
      memberPromises: new Map(),
      shardEtags: new Map(),
      activityBoundsByRef: new Map(),
      activityBoundsPromises: new Map(),
      spineByTeam: new Map(),
      searchBloomByTeam: new Map(),
      searchBloomByUrl: new Map(),
      searchLinksByTeam: new Map(),
      searchBloomPromises: new Map(),
      searchLinkPromises: new Map(),
      resourcePromises: new Map(),
      detailPromises: new Map(),
      loadedShardUrls: new Set(),
      loadedSearchShardUrls: new Set(),
      loadedSearchLinkUrls: new Set(),
      searchRecords: [],
      searchRecordsByRef: new Map(),
      searchPromptExcerpts: new Map(),
      searchResponsesByPrompt: new Map(),
      searchLinkPromptRefs: new Set(),
      searchLinkResponseRefs: new Set(),
      shardCatalog: [],
      searchCatalog: [],
      phaseCardPromises: new Map(),
      phaseIndexByAgent: new Map(),
      agentsById: new Map(),
      teamBySlug: new Map(),
      phaseIndexReady: false,
      phaseIndexPromise: null,
      phaseIndexReference: null,
      //: The search UI's own state, as `initializeState` sets it in the page. Present here
      //: because `updateTranscriptSearch` is the page's real matcher and reads all of it; a
      //: probe that supplied only what it happened to notice would be choosing the answers.
      searchCorpusMode: "schema3",
      searchCorpusNote: "",
      searchScope: "all-transcript",
      searchSort: "relevance",
      searchShardState: "unloaded",
      transcriptSearchState: "unloaded",
      transcriptSearchError: "",
      transcriptSearchResults: [],
      transcriptSearchTotal: 0,
      transcriptMatchedAgentIds: new Set(),
      activeSearchRef: "",
      searchLoadQueue: [],
      searchLoadActive: 0,
      searchRequestGeneration: 0,
      data: null
    },
    overrides || {}
  );
  // `loadSchema3` is the one function here that cannot be exercised by calling its pieces: it is
  //  the assembly, and an assembly is exactly where a name typed wrong survives every unit test and
  //  `node --check` alike. So it is extracted with the rest and given the two things it reaches for
  //  outside this set -- `initializeData`, which is the DOM boundary, and the state object. The
  //  caller passes an `installed` collector and gets back the schema-1-shaped object the page would
  //  have rendered.
  const installed = [];
  const painted = [];
  const exported = FUNCTIONS.concat(CONSTANTS).join(", ");
  const factory = new Function(
    "app",
    "initializeData",
    ...DOM_BOUNDARY,
    '"use strict";\n' + parts.join("\n") + "\nreturn { " + exported + " };"
  );
  const reader = factory(
    app,
    //: `initializeData` normalises and then assigns `app.data`; the normalisation is DOM-adjacent
    //: and the assignment is not, so the stub keeps the assignment. `selectedTeamAllows` -- which
    //: the real matcher calls on every candidate -- reads `app.data.teams`, and a stub that
    //: dropped it would make every team-scoped search on a single-team archive answer from a
    //: `null` and quietly return nothing.
    function (merged) {
      installed.push(merged);
      app.data = merged;
      // The two indices `initializeData` builds that the schema-2 corpus reader consults:
      // `validateTranscriptSearchShard` resolves every record's agent through `agentsById` and
      // refuses a record whose agent it cannot find. Rebuilt here the way the page rebuilds them
      // -- from the installed object, keyed by `id` and `slug` -- rather than left empty, which
      // would make every schema-2 shard fail validation for a reason the archive has nothing to
      // do with.
      app.agentsById = new Map(
        (merged.agents || [])
          .filter(function (agent) { return agent && typeof agent.id === "string"; })
          .map(function (agent) { return [agent.id, agent]; })
      );
      app.teamBySlug = new Map(
        (merged.teams || [])
          .filter(function (team) { return team && typeof team.slug === "string"; })
          .map(function (team) { return [team.slug, team]; })
      );
    },
    ...DOM_BOUNDARY.map(function (name) {
      return function () { painted.push(name); };
    })
  );
  reader.app = app;
  reader.installed = installed;
  reader.painted = painted;
  return reader;
}

module.exports = { loadReader: loadReader, appSource: source, appPath: APP_PATH };
