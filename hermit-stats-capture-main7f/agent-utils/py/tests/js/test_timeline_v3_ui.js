"use strict";

// The schema-3 half of `static/app.js`, tested against synthetic shards.
//
// This suite owns the *arithmetic and the refusals*: catalogue narrowing, sidecar parsing and its
// contiguity invariant, member selection by time and by line, the Range/If-Range/206/416 protocol,
// and the checks that stop a mismatched generation before a record is believed. The companion
// Python test `test_timeline_v3_website.py` owns the other half -- the same functions reading
// shards this repository's own writer produced, over HTTP, from the archive's own `serve.py` --
// so nothing here has to pretend to be either the writer or the server.

const assert = require("assert");
const crypto = require("crypto");
const zlib = require("zlib");

const { loadReader, appSource } = require("./schema3_probe.js");

const INDEX_SUFFIX = ".index.jsonl";

function sha256(buffer) {
  return crypto.createHash("sha256").update(buffer).digest("hex");
}

//: Build one multi-member gzip shard and its sidecar, the way `seekable_jsonl` writes them.
//:
//: `groups` is a list of lists of records: one gzip member per group, one line per record. The
//: sidecar's fields are computed here rather than copied from a fixture so that a test which
//: perturbs a record perturbs the digests with it.
function buildShard(relativePath, groups, timestampKey) {
  const members = [];
  const chunks = [];
  let cOff = 0;
  let uOff = 0;
  let line = 0;
  const uncompressed = [];
  groups.forEach(function (group) {
    const body = Buffer.from(
      group.map(function (record) { return JSON.stringify(record); }).join("\n") + "\n",
      "utf8"
    );
    const compressed = zlib.gzipSync(body, { level: 6, mtime: 0 });
    const stamps = group
      .map(function (record) { return record[timestampKey]; })
      .filter(function (value) { return Number.isFinite(value); });
    members.push({
      c_off: cOff,
      c_len: compressed.length,
      u_off: uOff,
      u_len: body.length,
      l0: line,
      n: group.length,
      t0: stamps.length ? Math.min.apply(null, stamps) : null,
      t1: stamps.length ? Math.max.apply(null, stamps) : null
    });
    chunks.push(compressed);
    uncompressed.push(body);
    cOff += compressed.length;
    uOff += body.length;
    line += group.length;
  });
  const data = Buffer.concat(chunks);
  const plain = Buffer.concat(uncompressed);
  const header = {
    format: "wrkviz/seekable-jsonl-index",
    version: 1,
    codec: "gzip",
    codec_level: 6,
    target_chunk_bytes: 1 << 20,
    timestamp_key: timestampKey,
    timestamps_sorted: true,
    member_count: members.length,
    record_count: line,
    c_size: data.length,
    u_size: plain.length,
    c_sha256: sha256(data),
    u_sha256: sha256(plain),
    data_file: relativePath.split("/").pop()
  };
  const index = [header]
    .concat(members)
    .map(function (value) { return JSON.stringify(value); })
    .join("\n") + "\n";
  return {
    path: relativePath,
    data: data,
    index: index,
    members: members,
    catalog: {
      stream: null,
      team: null,
      day: null,
      path: relativePath,
      index_path: relativePath + INDEX_SUFFIX,
      records: line,
      members: members.length,
      c_bytes: data.length,
      u_bytes: plain.length,
      c_sha256: header.c_sha256,
      u_sha256: header.u_sha256,
      timestamps_sorted: true,
      t0: members.length ? members[0].t0 : null,
      t1: members.length ? members[members.length - 1].t1 : null,
      t_end_exclusive: members.length ? members[members.length - 1].t1 + 1 : null
    }
  };
}

//: A `fetch` that behaves the way `standalone_server.TimelineRequestHandler` does for the two
//: things this reader depends on: a single byte range answered with 206 and a `Content-Range`, and
//: `If-Range` answered with the whole representation when it does not match. The real server is
//: exercised by the Python companion test; this stub exists so the refusal branches -- 416, a
//: mismatched `If-Range`, a truncated file -- can be produced on demand.
function makeFetch(files, options) {
  const settings = options || {};
  const log = [];
  async function stub(url, init) {
    const file = files.get(url);
    log.push({ url: url, headers: (init && init.headers) || {} });
    if (!file) {
      return {
        ok: false,
        status: 404,
        statusText: "Not Found",
        headers: new Map()
      };
    }
    const body = Buffer.isBuffer(file) ? file : Buffer.from(file, "utf8");
    const etag = settings.etagFor ? settings.etagFor(url) : '"sha256-' + sha256(body) + '"';
    const headers = { Range: "", "If-Range": "" };
    Object.keys((init && init.headers) || {}).forEach(function (key) {
      headers[key] = init.headers[key];
    });
    const range = /^bytes=(\d+)-(\d+)$/.exec(headers.Range || "");
    const ignoreRange = settings.ignoreRangeFor && settings.ignoreRangeFor(url);
    const conditional = headers["If-Range"] && headers["If-Range"] !== etag;
    function response(status, buffer, extra) {
      const map = new Map(Object.entries(Object.assign({ ETag: etag }, extra || {})));
      return {
        ok: status >= 200 && status < 300,
        status: status,
        statusText: String(status),
        headers: { get: function (name) { return map.has(name) ? map.get(name) : null; } },
        arrayBuffer: async function () {
          return buffer.buffer.slice(
            buffer.byteOffset,
            buffer.byteOffset + buffer.byteLength
          );
        },
        text: async function () { return buffer.toString("utf8"); },
        json: async function () { return JSON.parse(buffer.toString("utf8")); }
      };
    }
    if (!range || ignoreRange || conditional) {
      return response(200, body);
    }
    const first = Number(range[1]);
    const last = Number(range[2]);
    if (first >= body.length) {
      return response(416, Buffer.alloc(0), {
        "Content-Range": "bytes */" + body.length
      });
    }
    const end = Math.min(last, body.length - 1);
    return response(206, body.subarray(first, end + 1), {
      "Content-Range": "bytes " + first + "-" + end + "/" + body.length
    });
  }
  stub.log = log;
  return stub;
}

function withFetch(stub, run) {
  const previous = global.fetch;
  global.fetch = stub;
  return Promise.resolve()
    .then(run)
    .finally(function () { global.fetch = previous; });
}

// ---------------------------------------------------------------------------------------------
// The bundle names the schema-3 bootstrap, which is what `archive_gc._website_refusal` looks for
// ---------------------------------------------------------------------------------------------

assert.ok(
  appSource.indexOf("data/timeline-v3.json") >= 0,
  "app.js must name the schema-3 bootstrap; archive_gc holds 1.4 GB on exactly this string"
);
assert.ok(
  appSource.indexOf("data/timeline-v2.json") >= 0,
  "app.js must keep its schema-2 fallback: an older archive ships an older bundle and a newer " +
    "bundle meets older archives"
);

// ---------------------------------------------------------------------------------------------
// Catalogue narrowing
// ---------------------------------------------------------------------------------------------

const reader = loadReader();

assert.strictEqual(reader.shardedMode(), true);
assert.strictEqual(reader.schema3Enabled(), true);

const legacyReader = loadReader({ schemaMode: "schema1" });
assert.strictEqual(legacyReader.shardedMode(), false);
assert.strictEqual(legacyReader.schema3Enabled(), false);
const schema2Reader = loadReader({ schemaMode: "schema2" });
assert.strictEqual(schema2Reader.shardedMode(), true);
assert.strictEqual(schema2Reader.schema3Enabled(), false);

// A schema-2 catalogue entry is still keyed by its validated content-addressed URL, and a
// schema-3 one by its path. One accessor, two generations, and neither loses its own check.
const digest = "a".repeat(64);
assert.strictEqual(
  reader.shardKey({ url: "data/timeline-v2/objects/" + digest + ".json", sha256: digest }, "x"),
  "data/timeline-v2/objects/" + digest + ".json"
);
assert.strictEqual(
  reader.shardKey({ path: "data/timeline-v3/spine/alpha.jsonl.gz" }, "x"),
  "data/timeline-v3/spine/alpha.jsonl.gz"
);
assert.throws(function () {
  reader.shardKey({ url: "data/timeline-v2/objects/" + digest + ".json", sha256: "b" }, "x");
}, /complete SHA-256/);

// A path that escapes the schema-3 root is refused rather than fetched.
[
  "data/timeline-v2/objects/x.json",
  "data/timeline-v3/../secrets.json",
  "data/timeline-v3//spine/alpha.jsonl.gz",
  "data/timeline-v3/spine/alpha.jsonl.gz?token=1"
].forEach(function (candidate) {
  assert.throws(function () {
    reader.schema3SafeRelativePath(candidate, "shard.path");
  }, /must be a path under/, candidate + " must be refused");
});

const spineFixture = buildShard(
  "data/timeline-v3/spine/alpha.jsonl.gz",
  [
    [
      { record_kind: "team", slug: "alpha" },
      { record_kind: "agent", id: "root", team: "alpha", start_ms: 10, end_ms: 90 },
      { record_kind: "phase_card", id: "p1", agent_id: "root", team: "alpha", start_ms: 10, end_ms: 40 }
    ],
    [
      { record_kind: "phase_card", id: "p2", agent_id: "root", team: "alpha", start_ms: 40, end_ms: 90 },
      { record_kind: "activity_bounds", ref: "agent:alpha::root", activity_start_ms: 12, activity_end_ms: 71 },
      { record_kind: "activity_bounds", ref: "phase:alpha::p1", activity_start_ms: 12, activity_end_ms: 33 }
    ]
  ],
  "at_ms"
);

const spineEntry = reader.schema3ShardEntry(
  Object.assign({}, spineFixture.catalog, {
    stream: "spine",
    team: "alpha",
    line_ranges: { team: [0, 1], agent: [1, 1], phase_card: [2, 2], activity_bounds: [4, 2] }
  }),
  "spine",
  "spine shard"
);

assert.strictEqual(spineEntry.records, 6);
// Two ranges with a hole where the phase cards are: `[0, 2)` is the team and the agent, `[4, 4)`
// would be empty here because nothing follows the cards before the zoom bounds.
assert.deepStrictEqual(reader.spineFirstPaintRanges(spineEntry), [{ first: 0, count: 2 }]);

// A spine that publishes no zoom bounds is read whole -- there is no prefix to stop at.
const boundlessEntry = reader.schema3ShardEntry(
  Object.assign({}, spineFixture.catalog, {
    stream: "spine",
    team: "alpha",
    line_ranges: { team: [0, 1], agent: [1, 5] }
  }),
  "spine",
  "spine shard"
);
assert.deepStrictEqual(reader.spineFirstPaintRanges(boundlessEntry), [{ first: 0, count: 6 }]);

// A spine with kinds on both sides of the cards produces both ranges, and the hole between them is
// exactly the cards. This is the shape every real archive has.
const straddling = reader.schema3ShardEntry(
  Object.assign({}, spineFixture.catalog, {
    stream: "spine",
    team: "alpha",
    line_ranges: {
      team: [0, 1],
      agent: [1, 1],
      phase_card: [2, 2],
      structural_edge: [4, 1],
      activity_bounds: [5, 1]
    }
  }),
  "spine",
  "spine shard"
);
assert.deepStrictEqual(
  reader.spineFirstPaintRanges(straddling),
  [{ first: 0, count: 2 }, { first: 4, count: 1 }]
);

assert.throws(function () {
  reader.schema3ShardEntry(
    Object.assign({}, spineFixture.catalog, {
      stream: "spine",
      index_path: "data/timeline-v3/spine/alpha.jsonl.gz.idx"
    }),
    "spine",
    "spine shard"
  );
}, /index_path must be its shard/);

assert.throws(function () {
  reader.schema3Stream(
    {
      streams: {
        spine: {
          shards: [
            Object.assign({}, spineFixture.catalog, { stream: "spine" }),
            Object.assign({}, spineFixture.catalog, { stream: "spine" })
          ]
        }
      }
    },
    "spine",
    true
  );
}, /twice/);

assert.throws(function () {
  reader.schema3Stream({ streams: {} }, "spine", true);
}, /does not publish the spine stream/);
assert.deepStrictEqual(reader.schema3Stream({ streams: {} }, "search", false), []);

// ---------------------------------------------------------------------------------------------
// The sidecar, and every way it is allowed to say no
// ---------------------------------------------------------------------------------------------

const parsed = reader.parseChunkIndex(spineFixture.index, spineEntry);
assert.strictEqual(parsed.members.length, 2);
assert.strictEqual(parsed.members[0].l0, 0);
assert.strictEqual(parsed.members[1].l0, 3);

assert.throws(function () {
  reader.parseChunkIndex("", spineEntry);
}, /is empty/);

assert.throws(function () {
  reader.parseChunkIndex(
    spineFixture.index.replace('"version":1', '"version":2'),
    spineEntry
  );
}, /not a version-1/);

assert.throws(function () {
  reader.parseChunkIndex(
    spineFixture.index.replace('"codec":"gzip"', '"codec":"zstd"'),
    spineEntry
  );
}, /codec zstd/);

// The binding to the bootstrap: a sidecar from another build is refused before a data byte moves.
assert.throws(function () {
  reader.parseChunkIndex(
    spineFixture.index.replace(spineEntry.u_sha256, "f".repeat(64)),
    spineEntry
  );
}, /different generation/);

// Contiguity. `seekable_jsonl.ChunkIndex._parse` checks the same invariant on the writing side;
// a reader that trusted it would compute every later member's offset from a lie.
assert.throws(function () {
  const lines = spineFixture.index.trimEnd().split("\n");
  const member = JSON.parse(lines[2]);
  member.c_off += 1;
  lines[2] = JSON.stringify(member);
  reader.parseChunkIndex(lines.join("\n") + "\n", spineEntry);
}, /not contiguous/);

// ---------------------------------------------------------------------------------------------
// Member selection
// ---------------------------------------------------------------------------------------------

const timed = [
  { l0: 0, n: 2, t0: 100, t1: 200 },
  { l0: 2, n: 2, t0: 300, t1: 400 },
  { l0: 4, n: 1, t0: null, t1: null }
];
assert.deepStrictEqual(
  reader.membersForTimeRange(timed, 200, 300).map(function (m) { return m.l0; }),
  [0],
  "half-open: a member starting exactly at the window end is not selected, one ending at its " +
    "start is"
);
assert.deepStrictEqual(
  reader.membersForTimeRange(timed, 201, 301).map(function (m) { return m.l0; }),
  [2]
);
assert.deepStrictEqual(
  reader.membersForTimeRange(timed, NaN, NaN).map(function (m) { return m.l0; }),
  [0, 2],
  "an unbounded window still cannot select a member with no timestamped record"
);
assert.deepStrictEqual(
  reader.membersForLineRange(timed, 0, 0),
  [],
  "an empty line range reads nothing, rather than everything"
);
assert.deepStrictEqual(
  reader.membersForLineRange(timed, 2, 1).map(function (m) { return m.l0; }),
  [2]
);
assert.deepStrictEqual(
  reader.membersForLineRange(timed, 1, 3).map(function (m) { return m.l0; }),
  [0, 2]
);

// ---------------------------------------------------------------------------------------------
// Reading over Range
// ---------------------------------------------------------------------------------------------

function spineFiles() {
  return new Map([
    [spineFixture.path, spineFixture.data],
    [spineFixture.path + INDEX_SUFFIX, spineFixture.index]
  ]);
}

async function readsOnlyTheMembersItNeeds() {
  const local = loadReader();
  const stub = makeFetch(spineFiles());
  await withFetch(stub, async function () {
    // The first paint's prefix stops before the zoom bounds, but it spans both members here, so
    // both are fetched -- and the two overshoot records are dropped inside the second one.
    const records = await local.readShardRecords(spineEntry, { first: 0, count: 4 });
    assert.deepStrictEqual(
      records.map(function (record) { return record.record_kind; }),
      ["team", "agent", "phase_card", "phase_card"]
    );
    assert.strictEqual(stub.log.length, 3, "one sidecar, two members");
    assert.deepStrictEqual(
      stub.log.map(function (item) { return item.headers.Range || null; }),
      [null, "bytes=0-" + (spineFixture.members[0].c_len - 1),
        "bytes=" + spineFixture.members[1].c_off + "-" + (spineFixture.data.length - 1)]
    );
    // The zoom bounds live in the member the prefix already inflated, so asking for them is free.
    const bounds = await local.readShardKind(spineEntry, "activity_bounds");
    assert.deepStrictEqual(
      bounds.map(function (record) { return record.ref; }),
      ["agent:alpha::root", "phase:alpha::p1"]
    );
    assert.strictEqual(stub.log.length, 3, "the sidecar and both members were already cached");
  });
}

async function aLaterRangeIsConditionalOnTheFirst() {
  // The two members of one first-paint prefix are fetched in parallel, so neither can carry an
  // ETag the other has not learned yet. What `If-Range` protects is the *later* read -- a pan into
  // a day the reader has already opened part of -- and that is what this asserts, because a
  // rebuild between the two is the only way a reader could splice two generations together.
  const local = loadReader();
  const stub = makeFetch(spineFiles());
  await withFetch(stub, async function () {
    await local.readShardRecords(spineEntry, { first: 0, count: 3 });
    assert.strictEqual(stub.log.length, 2);
    assert.strictEqual(stub.log[1].headers["If-Range"], undefined);
    await local.readShardRecords(spineEntry, { first: 3, count: 3 });
    assert.strictEqual(stub.log.length, 3);
    assert.ok(
      String(stub.log[2].headers["If-Range"] || "").startsWith('"sha256-'),
      "a range into a shard already partly read must be conditional on that representation"
    );
  });
}

async function timeAddressedReadsFilterWithinTheMember() {
  const local = loadReader();
  const timeline = buildShard(
    "data/timeline-v3/timeline/alpha/2026-08-01.jsonl.gz",
    [
      [
        { record_kind: "event", at_ms: 1000, id: "e1" },
        { record_kind: "event", at_ms: 2000, id: "e2" }
      ],
      [
        { record_kind: "event", at_ms: 3000, id: "e3" },
        { record_kind: "phase", at_ms: 4000, id: "p1" }
      ]
    ],
    "at_ms"
  );
  const entry = local.schema3ShardEntry(
    Object.assign({}, timeline.catalog, {
      stream: "timeline",
      team: "alpha",
      day: "2026-08-01"
    }),
    "timeline",
    "timeline shard"
  );
  const stub = makeFetch(new Map([
    [timeline.path, timeline.data],
    [timeline.path + INDEX_SUFFIX, timeline.index]
  ]));
  await withFetch(stub, async function () {
    const records = await local.readShardRecords(entry, { start_ms: 2000, end_ms: 3001 });
    assert.deepStrictEqual(
      records.map(function (record) { return record.id; }),
      ["e2", "e3"],
      "the window is half-open and is applied per record, not per member"
    );
    // Both members overlap the window, so both were fetched -- but the record at 1000 and the one
    // at 4000 were dropped inside them. A member is the smallest thing the format can seek to.
    assert.strictEqual(stub.log.length, 3);
  });
}

async function aTruncatedShardIsRefused() {
  const local = loadReader();
  const files = spineFiles();
  files.set(spineFixture.path, spineFixture.data.subarray(0, 4));
  await withFetch(makeFetch(files), async function () {
    await assert.rejects(
      local.readShardRecords(spineEntry, { first: 0, count: 4 }),
      /shorter than|served bytes|is 4 bytes/
    );
  });
}

async function aServerThatIgnoresRangeIsStillCorrect() {
  const local = loadReader();
  const stub = makeFetch(spineFiles(), {
    ignoreRangeFor: function (url) { return url === spineFixture.path; }
  });
  await withFetch(stub, async function () {
    const records = await local.readShardRecords(spineEntry, { first: 4, count: 2 });
    assert.deepStrictEqual(
      records.map(function (record) { return record.ref; }),
      ["agent:alpha::root", "phase:alpha::p1"],
      "a 200 to a range request is legal; the member is sliced out of the whole representation"
    );
  });
}

async function aShardThatChangedUnderTheReaderIsRefused() {
  const local = loadReader();
  let generation = 0;
  const stub = makeFetch(spineFiles(), {
    ignoreRangeFor: function (url) { return url === spineFixture.path && generation > 0; },
    etagFor: function (url) {
      return url === spineFixture.path ? '"gen-' + generation + '"' : '"static"';
    }
  });
  await withFetch(stub, async function () {
    await local.readShardRecords(spineEntry, { first: 0, count: 1 });
    generation = 1;
    await assert.rejects(
      local.readShardRecords(spineEntry, { first: 4, count: 2 }),
      /changed while it was being read/
    );
  });
}

async function aRangePastTheEndIs416() {
  const local = loadReader();
  const files = spineFiles();
  // A catalogue that claims more bytes than the file has: the first member past the real end is
  // what the server answers 416 to, and 416 is a different diagnosis from "not found".
  const entry = Object.assign({}, spineEntry, { c_bytes: spineFixture.data.length });
  files.set(spineFixture.path, spineFixture.data.subarray(0, spineFixture.members[0].c_len));
  await withFetch(makeFetch(files), async function () {
    await assert.rejects(
      local.readShardRecords(entry, { first: 4, count: 2 }),
      /is shorter than/
    );
  });
}

// ---------------------------------------------------------------------------------------------
// Derived shapes the rest of the page consumes
// ---------------------------------------------------------------------------------------------

assert.deepStrictEqual(
  reader.schema3Payload({ record_kind: "event", at_ms: 5, id: "e" }),
  { at_ms: 5, id: "e" },
  "the envelope's one added key is dropped; at_ms is the record's own field and stays"
);

assert.strictEqual(reader.schema3LocalIdentifier("alpha", "alpha::root"), "root");
assert.strictEqual(reader.schema3LocalIdentifier("alpha", "root"), "root");

assert.strictEqual(
  reader.activityBoundsRef({ team: "alpha", id: "alpha::p1" }, { agent_id: "root", phase_id: "p1" }),
  "phase:alpha::p1"
);
assert.strictEqual(
  reader.activityBoundsRef({ team: "alpha", id: "root" }, { agent_id: "root" }),
  "agent:alpha::root"
);
assert.strictEqual(
  reader.activityBoundsRef({ team: "alpha", kind: "daily", start_ms: 172800000 }, null),
  "rollup:alpha::daily::172800000"
);
assert.strictEqual(reader.activityBoundsRef({ id: "root" }, { agent_id: "root" }), "");

assert.deepStrictEqual(
  reader.schema3DayRange({ day: "2026-08-01" }, "shard"),
  { start_ms: Date.UTC(2026, 7, 1), end_ms: Date.UTC(2026, 7, 2) }
);
assert.throws(function () {
  reader.schema3DayRange({ day: "2026-8-1" }, "shard");
}, /not addressed by a UTC day/);

const searchCatalog = reader.schema3SearchCatalog([
  { team: "beta", day: "2026-08-02", records: 3, path: "b" },
  { team: "alpha", day: "2026-08-03", records: 4, path: "a2" },
  { team: "alpha", day: "2026-08-01", records: 5, path: "a1" }
]);
assert.deepStrictEqual(
  searchCatalog.map(function (entry) { return entry.path; }),
  ["a1", "a2", "b"]
);
assert.deepStrictEqual(searchCatalog[0].counts, { records: 5 });
assert.strictEqual(searchCatalog[0].start_ms, Date.UTC(2026, 7, 1));
assert.strictEqual(searchCatalog[0].end_ms, Date.UTC(2026, 7, 2));

// The prefilter is skipped for a query no trigram can be built from -- `B3` is the case study's
// own acceptance query, and under schema 2 it paid for every Bloom filter in the bootstrap and
// then used none of them.
assert.strictEqual(reader.queryCanUseBloom("B3"), false);
assert.strictEqual(reader.queryCanUseBloom("backend maturity B3"), true);
assert.strictEqual(reader.queryCanUseBloom(""), false);

const sink = {
  agents: [], phaseCards: [], edges: [], rollups: [],
  projects: [], summary_files: [], glossary: [], project_overviews: []
};
reader.schema3InstallSpine("alpha", [
  { record_kind: "team", slug: "alpha" },
  { record_kind: "agent", id: "root" },
  { record_kind: "phase_card", id: "p1", agent_id: "root" },
  { record_kind: "activity_bounds", ref: "agent:alpha::root" }
], sink);
assert.deepStrictEqual(sink.agents, [{ id: "root" }]);
assert.deepStrictEqual(sink.phaseCards, [{ id: "p1", agent_id: "root" }]);
assert.throws(function () {
  reader.schema3InstallSpine("alpha", [{ record_kind: "surprise" }], sink);
}, /unknown record kind/);

// ---------------------------------------------------------------------------------------------
// The sole-team fallback: a single-team render stamps `team` on its agents and on nothing else
// ---------------------------------------------------------------------------------------------
//
// `render.py` adds `team` to agents (it is what disambiguates a combined export) and not to phases
// or rollups, because on a single-team archive there is nothing to disambiguate and schema 1 never
// carried the field. The schema-3 *writer* has a `sole_team` fallback for exactly that, and so
// does `query._SchemaThreeArchive._unwrap`. Without the same fallback here, every phase and rollup
// zoom on a single-team archive computes an empty reference, finds no published bound, and falls
// back to fetching every day shard the subject overlaps to recompute two numbers that are sitting
// in the spine. Schema 2 never showed it, because it carried the bounds inline on the record.

const soleTeamReader = loadReader({ spineByTeam: new Map([["only", { team: "only" }]]) });
assert.strictEqual(
  soleTeamReader.activityBoundsRef({ id: "p1" }, { phase_id: "p1" }),
  "phase:only::p1",
  "a phase with no team on a single-team archive resolves to the sole team"
);
assert.strictEqual(
  soleTeamReader.activityBoundsRef({ kind: "daily", start_ms: 172800000 }, null),
  "rollup:only::daily::172800000"
);
assert.strictEqual(
  soleTeamReader.activityBoundsRef({ team: "other", id: "p1" }, { phase_id: "p1" }),
  "phase:other::p1",
  "a record that names its team is never overridden by the fallback"
);

// Two teams: there is no sole team, so a record without one is unresolvable and says so by
// returning "" rather than guessing.
const twoTeamReader = loadReader({
  spineByTeam: new Map([["a", { team: "a" }], ["b", { team: "b" }]])
});
assert.strictEqual(twoTeamReader.activityBoundsRef({ id: "p1" }, { phase_id: "p1" }), "");

// ---------------------------------------------------------------------------------------------
// A whole schema-3 archive, small enough to assert about and complete enough to load
// ---------------------------------------------------------------------------------------------

function tinyArchive(options) {
  const settings = options || {};
  const spine = buildShard(
    "data/timeline-v3/spine/alpha.jsonl.gz",
    [[
      { record_kind: "team", slug: "alpha", at_ms: 10 },
      { record_kind: "agent", id: "root", team: "alpha", at_ms: 10, start_ms: 10, end_ms: 90 }
    ]],
    "at_ms"
  );
  const day = buildShard(
    "data/timeline-v3/timeline/alpha/2026-08-01.jsonl.gz",
    [[{ record_kind: "event", id: "e1", team: "alpha", at_ms: 20 }]],
    "at_ms"
  );
  const bins = buildShard(
    "data/timeline-v3/bins.jsonl.gz",
    [[{ record_kind: "activity_bin", at_ms: 10, team: "alpha" }]],
    "at_ms"
  );
  const files = new Map([
    [spine.path, spine.data],
    [spine.path + INDEX_SUFFIX, spine.index],
    [day.path, day.data],
    [day.path + INDEX_SUFFIX, day.index],
    [bins.path, bins.data],
    [bins.path + INDEX_SUFFIX, bins.index]
  ]);
  const streams = {
    timeline: {
      shards: [Object.assign({}, day.catalog, {
        stream: "timeline",
        team: "alpha",
        day: "2026-08-01",
        t0: 20,
        t_end_exclusive: 21
      })]
    },
    spine: {
      shards: [Object.assign({}, spine.catalog, {
        stream: "spine",
        team: "alpha",
        line_ranges: { team: [0, 1], agent: [1, 1] }
      })]
    },
    bins: { shards: [Object.assign({}, bins.catalog, { stream: "bins" })] }
  };
  Object.assign(streams, settings.searchStreams || {});
  const bootstrap = {
    schema_version: 3,
    kind: "timeline-v3-bootstrap",
    generated_at: "2026-08-01T00:00:00Z",
    source_digest: settings.sourceDigest || "tiny-digest",
    display_timezone: "UTC",
    display_timezone_source: "test",
    range: { start_ms: 10, end_ms: 90 },
    stats: {},
    teams: [{ slug: "alpha", label: "Alpha" }],
    codec: {
      container: "multi-member-gzip",
      timestamp_key: "at_ms",
      record_kind_key: "record_kind",
      index_suffix: INDEX_SUFFIX
    },
    streams: streams
  };
  files.set("data/timeline-v3.json", JSON.stringify(bootstrap));
  return { files: files, bootstrap: bootstrap, spine: spine, day: day, bins: bins };
}

//: One `search` shard for a team the corpus has no `search_links` shard for -- the shape a build
//: that died between two teams leaves. The three sections are all non-empty, so the *stream* rule
//: is satisfied and only the per-team rule can catch it.
async function aPerTeamGapInTheCorpusIsDeclined() {
  const searchShard = buildShard(
    "data/timeline-v3/search/beta/2026-08-01.jsonl.gz",
    [[{ record_kind: "search_record", ref: "message:beta::m1", team: "beta", at_ms: 20 }]],
    "at_ms"
  );
  const bloomShard = buildShard(
    "data/timeline-v3/search-bloom/beta.jsonl.gz",
    [[{ record_kind: "search_bloom", team: "beta", at_ms: 0, shard: searchShard.path }]],
    "at_ms"
  );
  const linkShard = buildShard(
    "data/timeline-v3/search-links/alpha.jsonl.gz",
    [[{ record_kind: "search_prompt", team: "alpha", at_ms: 0, ref: "message:alpha::m1" }]],
    "at_ms"
  );
  const archive = tinyArchive({
    searchStreams: {
      search: {
        shards: [Object.assign({}, searchShard.catalog, {
          stream: "search", team: "beta", day: "2026-08-01"
        })]
      },
      search_bloom: {
        shards: [Object.assign({}, bloomShard.catalog, { stream: "search_bloom", team: "beta" })]
      },
      search_links: {
        shards: [Object.assign({}, linkShard.catalog, { stream: "search_links", team: "alpha" })]
      }
    }
  });
  const local = loadReader();
  await withFetch(makeFetch(archive.files), async function () {
    await assert.rejects(
      local.loadSchema3(),
      /no search_links shard for beta/,
      "a corpus missing one team's relationships must be declined, not half-read"
    );
  });
}

//: A prefilter record that names another team's shard. The Bloom filter's only wrong answer is a
//: false miss, so installing one under the wrong path makes the page skip a whole day and drop
//: every record in it -- silently. `query._SchemaThreeArchive.search_blooms` refuses the same
//: record for the same reason.
async function aPrefilterForAnotherTeamsShardIsRefused() {
  const bloomShard = buildShard(
    "data/timeline-v3/search-bloom/alpha.jsonl.gz",
    [[{
      record_kind: "search_bloom",
      team: "alpha",
      at_ms: 0,
      shard: "data/timeline-v3/search/beta/2026-08-01.jsonl.gz",
      bloom: { algorithm: "x", bits: 8, hashes: 1, bits_base64: "AA==" }
    }]],
    "at_ms"
  );
  const local = loadReader({
    searchBloomByTeam: new Map([["alpha", reader.schema3ShardEntry(
      Object.assign({}, bloomShard.catalog, { stream: "search_bloom", team: "alpha" }),
      "search_bloom",
      "bloom shard"
    )]]),
    searchCatalog: [
      { team: "alpha", path: "data/timeline-v3/search/alpha/2026-08-01.jsonl.gz" },
      { team: "beta", path: "data/timeline-v3/search/beta/2026-08-01.jsonl.gz" }
    ]
  });
  const files = new Map([
    [bloomShard.path, bloomShard.data],
    [bloomShard.path + INDEX_SUFFIX, bloomShard.index]
  ]);
  await withFetch(makeFetch(files), async function () {
    await assert.rejects(
      local.ensureSearchBlooms("alpha"),
      /which is not a search shard of alpha/,
      "one team's prefilter must not be installable under another team's shard path"
    );
  });
}

// ---------------------------------------------------------------------------------------------
// The corpus an archive built before the search streams still has: schema 2's
// ---------------------------------------------------------------------------------------------

function schema2SearchBootstrap(sourceDigest) {
  const shardDigest = "c".repeat(64);
  const linkDigest = "d".repeat(64);
  return {
    schema_version: 2,
    kind: "timeline-bootstrap",
    source_digest: sourceDigest,
    range: { start_ms: 10, end_ms: 90 },
    teams: [{ slug: "alpha" }],
    detail_shards: [],
    search: {
      strategy: "transcript-message-shards",
      schema_version: 1,
      shards: [{
        team: "alpha",
        day: "2026-08-01",
        start_ms: 10,
        end_ms: 90,
        counts: { records: 1 },
        url: "data/timeline-v2/objects/" + shardDigest + ".json",
        sha256: shardDigest,
        linkage: {
          url: "data/timeline-v2/objects/" + linkDigest + ".json",
          sha256: linkDigest,
          counts: { prompts: 1, responses: 0 }
        }
      }]
    }
  };
}

//: The capability the CLI kept and the page had lost. An archive whose schema 3 predates the
//: search streams publishes a full corpus in `data/timeline-v2.json`, and a page that declared
//: search "unavailable" beside it was saying something false about the tree it was looking at.
async function aSchemaThreeWithoutTheStreamsSearchesSchemaTwo() {
  const archive = tinyArchive({});
  archive.files.set(
    "data/timeline-v2.json",
    JSON.stringify(schema2SearchBootstrap("tiny-digest"))
  );
  const local = loadReader();
  await withFetch(makeFetch(archive.files), async function () {
    assert.strictEqual(await local.loadSchema3(), true);
    assert.strictEqual(local.app.schemaMode, "schema3");
    assert.strictEqual(local.app.searchCorpusMode, "schema2");
    assert.strictEqual(local.app.searchCatalog.length, 1);
    assert.strictEqual(local.app.transcriptSearchState, "unloaded");
    assert.strictEqual(local.app.transcriptSearchError, "");
    assert.ok(
      local.app.searchCorpusNote.indexOf("from schema 2") >= 0,
      "the fallback is announced rather than silent: " + local.app.searchCorpusNote
    );
    // And the search *dispatch* follows the corpus rather than the timeline. Three call sites --
    // the shard loader, the prefilter and the linkage -- used to ask `schema3Enabled()`, which is
    // now the wrong question: the page is in schema-3 mode and the corpus is schema 2's. Asked
    // here through `transcriptSearchNeedsLoad`, because that is the one of the three with an
    // observable answer: the schema-3 branch looks in `searchLinksByTeam`, which is empty, and
    // would say "nothing to load" about a corpus with a linkage object waiting for it.
    assert.strictEqual(local.app.searchLinksByTeam.size, 0);
    local.app.query = "anything";
    // The text half is marked loaded first, so that what remains is *only* the linkage question --
    // otherwise the unloaded text shard answers "yes" before the branch under test is reached and
    // the assertion would pass whichever branch ran.
    local.app.searchCatalog.forEach(function (shard) {
      local.app.loadedSearchShardUrls.add(local.shardKey(shard, "transcript search shard"));
    });
    assert.strictEqual(
      local.transcriptSearchNeedsLoad(),
      true,
      "the schema-2 linkage object must still be seen as unloaded"
    );
    assert.deepStrictEqual(
      await local.prepareTranscriptSearchPrefilter(),
      [],
      "and the schema-3 prefilter stream must not be reached for"
    );
  });
}

//: Two generations of one archive that disagree about their source describe two different builds,
//: and answering phases out of one and messages out of the other puts messages inside phases they
//: never occurred in. `query.TimelineQuery._search_bootstrap` refuses that; so does this.
async function aSchemaTwoCorpusFromAnotherBuildIsRefused() {
  const archive = tinyArchive({});
  archive.files.set(
    "data/timeline-v2.json",
    JSON.stringify(schema2SearchBootstrap("some-other-build"))
  );
  const local = loadReader();
  await withFetch(makeFetch(archive.files), async function () {
    assert.strictEqual(await local.loadSchema3(), true);
    assert.strictEqual(local.app.searchCorpusMode, "none");
    assert.strictEqual(local.app.searchCatalog.length, 0);
    assert.ok(
      local.app.searchCorpusNote.indexOf("different source generation") >= 0,
      "the refusal is stated rather than swallowed: " + local.app.searchCorpusNote
    );
  });
}

//: An archive with no corpus anywhere says nothing extra. The note exists to report a surprise,
//: and "this export was built without transcript search" is not one.
async function anArchiveWithNoCorpusAnywhereSaysNothingExtra() {
  const archive = tinyArchive({});
  const local = loadReader();
  await withFetch(makeFetch(archive.files), async function () {
    assert.strictEqual(await local.loadSchema3(), true);
    assert.strictEqual(local.app.searchCorpusMode, "none");
    assert.strictEqual(local.app.searchCorpusNote, "");
    assert.strictEqual(local.app.transcriptSearchState, "unavailable");
  });
}

// ---------------------------------------------------------------------------------------------
// The member gate is page-wide, which is what its constant claims
// ---------------------------------------------------------------------------------------------
//
// `readShardRecords` bounds the members of *one* shard, and a first paint calls it once per spine
// shard inside a single `Promise.all`. Before `schema3MemberSlot` existed the bound was therefore
// per call -- twelve teams times six -- while the constant's comment said six. The browser's
// per-origin cap hid it; a constant that documents an invariant the code does not have is what the
// next change gets sized against.
async function memberFetchesAreBoundedAcrossTheWholePage() {
  const shards = [];
  const files = new Map();
  for (let index = 0; index < 5; index += 1) {
    const groups = [];
    for (let member = 0; member < 4; member += 1) {
      groups.push([{ record_kind: "event", id: "e" + index + "-" + member, at_ms: member }]);
    }
    const shard = buildShard(
      "data/timeline-v3/timeline/team" + index + "/2026-08-01.jsonl.gz",
      groups,
      "at_ms"
    );
    files.set(shard.path, shard.data);
    files.set(shard.path + INDEX_SUFFIX, shard.index);
    shards.push(reader.schema3ShardEntry(
      Object.assign({}, shard.catalog, {
        stream: "timeline",
        team: "team" + index,
        day: "2026-08-01"
      }),
      "timeline",
      "timeline shard"
    ));
  }
  const local = loadReader();
  let inFlight = 0;
  let peak = 0;
  const stub = makeFetch(files);
  const counting = async function (url, init) {
    const isMember = url.indexOf(INDEX_SUFFIX) < 0;
    if (isMember) {
      inFlight += 1;
      peak = Math.max(peak, inFlight);
    }
    try {
      return await stub(url, init);
    } finally {
      if (isMember) {
        inFlight -= 1;
      }
    }
  };
  await withFetch(counting, async function () {
    await Promise.all(shards.map(function (entry) {
      return local.readShardRecords(entry, { start_ms: NaN, end_ms: NaN });
    }));
  });
  assert.strictEqual(peak <= local.SCHEMA_3_MEMBER_CONCURRENCY, true,
    "20 members across 5 concurrent shard reads peaked at " + peak + " in flight, and the " +
      "constant says " + local.SCHEMA_3_MEMBER_CONCURRENCY);
  // And the gate drains: every record still arrives.
  assert.strictEqual(local.app.memberPromises.size, 20);
}

// ---------------------------------------------------------------------------------------------

Promise.resolve()
  .then(readsOnlyTheMembersItNeeds)
  .then(aLaterRangeIsConditionalOnTheFirst)
  .then(timeAddressedReadsFilterWithinTheMember)
  .then(aTruncatedShardIsRefused)
  .then(aServerThatIgnoresRangeIsStillCorrect)
  .then(aShardThatChangedUnderTheReaderIsRefused)
  .then(aRangePastTheEndIs416)
  .then(aPerTeamGapInTheCorpusIsDeclined)
  .then(aPrefilterForAnotherTeamsShardIsRefused)
  .then(aSchemaThreeWithoutTheStreamsSearchesSchemaTwo)
  .then(aSchemaTwoCorpusFromAnotherBuildIsRefused)
  .then(anArchiveWithNoCorpusAnywhereSaysNothingExtra)
  .then(memberFetchesAreBoundedAcrossTheWholePage)
  .then(function () {
    process.stdout.write("timeline schema-3 UI suite passed\n");
  })
  .catch(function (error) {
    process.stderr.write(String((error && error.stack) || error) + "\n");
    process.exitCode = 1;
  });
