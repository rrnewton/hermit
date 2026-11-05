"use strict";

// `zoomToActivityRange` chooses between two ways of framing a zoom: the bounds recorded on the
// record, and a scan of whatever timeline data is in memory. It used to make that choice by
// asking which *generation* the page had loaded -- `app.schemaMode === "schema2"` -- which was
// true of the only generation that published recorded bounds and became wrong the moment a
// second one did. Schema 3 publishes the same two numbers, derived by the same function, as a
// spine record keyed by stable reference.
//
// So the test is not "does it read the bounds", it is "does it read the bounds *whatever the
// schema says*", which is the bug. Each case below pins one branch and names the failure it
// would be: framing an agent's seven idle hours instead of the one hour it worked, or fetching
// a day of detail shards to recompute a number the record was already carrying.

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const source = fs.readFileSync(
  path.resolve(__dirname, "../../wrkviz/static/app.js"),
  "utf8"
);

function functionSource(name) {
  const marker = "  function " + name + "(";
  const start = source.indexOf(marker);
  assert.notStrictEqual(start, -1, "missing function " + name);
  const next = source.indexOf("\n  function ", start + marker.length);
  assert.notStrictEqual(next, -1, "missing function boundary after " + name);
  return source.slice(start, next);
}

function harness(schemaMode, scanResult, published, teams) {
  const calls = { zoomed: [], scanned: 0, fetched: 0, boundsFetched: [] };
  const context = {
    MIN_VIEW_MS: 1000,
    app: {
      schemaMode: schemaMode,
      viewStart: 0,
      viewEnd: 1000,
      data: {},
      activityBoundsByRef: new Map(Object.entries(published || {})),
      // The teams the schema-3 spine published. `boundsTeam` reads it, and reads it for one
      // reason: a single-team render stamps `team` on its agents and on nothing else, so a phase
      // or a rollup arrives here with no team at all and the sole published one is the answer.
      spineByTeam: new Map(
        (teams || ["alpha", "beta"]).map(function (slug) { return [slug, { team: slug }]; })
      )
    },
    timelineCore: {
      activityRangeWithin: function () {
        calls.scanned += 1;
        return scanResult;
      }
    },
    zoomToRange: function (start, end) {
      calls.zoomed.push([start, end]);
    },
    detailShardsForRange: function () {
      return ["a-shard"];
    },
    requestDetailShards: function () {
      calls.fetched += 1;
      return { promise: Promise.resolve() };
    },
    // Schema 3 keeps the zoom bounds out of the record and out of the first paint; this is the
    // fetch that brings one team's in, and counting it is how the test tells "read the published
    // number" apart from "scan the timeline for it again".
    ensureActivityBounds: function (team) {
      calls.boundsFetched.push(team);
      return Promise.resolve(null);
    },
    showDetailLoadError: function (error) {
      throw error;
    },
    Map: Map,
    Object: Object,
    Promise: Promise,
    Number: Number,
    Boolean: Boolean,
    Error: Error
  };
  vm.createContext(context);
  vm.runInContext(
    [
      functionSource("number"),
      functionSource("text"),
      functionSource("hasField"),
      functionSource("schema3Enabled"),
      functionSource("schema3LocalIdentifier"),
      functionSource("boundsTeam"),
      functionSource("activityBoundsRef"),
      functionSource("zoomToActivityRange"),
      functionSource("zoomWithActivityBounds")
    ].join("\n"),
    context
  );
  return { context: context, calls: calls };
}

const RECORDED = { start_ms: 0, end_ms: 700000, activity_start_ms: 600000, activity_end_ms: 660000 };
const BARE = { start_ms: 0, end_ms: 700000 };

// Recorded bounds are honoured under every generation, including the one that did not exist
// when the guard was written. Under schema 3 the old code fell through to `finishZoom`, which
// scans `app.data` -- and under schema 3 `app.data` holds no events at all, so the zoom framed
// the record's whole interval and the feature silently did nothing.
["schema1", "schema2", "schema3"].forEach(function (mode) {
  const run = harness(mode, null);
  run.context.zoomToActivityRange(RECORDED, null);
  assert.deepStrictEqual(run.calls.zoomed, [[600000, 660000]], mode);
  assert.strictEqual(run.calls.scanned, 0, mode + " must not scan when the record says");
  assert.strictEqual(run.calls.fetched, 0, mode + " must not fetch when the record says");
  assert.deepStrictEqual(
    run.calls.boundsFetched,
    [],
    mode + " must not fetch bounds the record already carries"
  );
});

// Bounds that are absent, unparseable or empty are not bounds. An inverted or zero-width pair
// would zoom to nothing, so it falls through to the derivation rather than being obeyed.
[
  BARE,
  { start_ms: 0, end_ms: 700000, activity_start_ms: 5, activity_end_ms: 5 },
  { start_ms: 0, end_ms: 700000, activity_start_ms: 9, activity_end_ms: 4 },
  { start_ms: 0, end_ms: 700000, activity_start_ms: "soon", activity_end_ms: 4 }
].forEach(function (bounds, index) {
  const run = harness("schema1", { start_ms: 10, end_ms: 20 });
  run.context.zoomToActivityRange(bounds, null);
  assert.deepStrictEqual(run.calls.zoomed, [[10, 20]], "case " + index);
  assert.strictEqual(run.calls.scanned, 1, "case " + index);
});

// Schema 1 has the whole timeline in memory, so the derivation is immediate and must not go
// looking for shards that a monolithic export does not have.
const monolith = harness("schema1", { start_ms: 10, end_ms: 20 });
monolith.context.zoomToActivityRange(BARE, null);
assert.strictEqual(monolith.calls.fetched, 0);

// A sharded generation without recorded bounds has to fetch the interval's detail before the
// derivation can see anything, and only then zoom.
const sharded = harness("schema2", { start_ms: 30, end_ms: 40 });
sharded.context.zoomToActivityRange(BARE, null);
assert.strictEqual(sharded.calls.fetched, 1);
assert.deepStrictEqual(sharded.calls.zoomed, []);
// Schema 3 is the third case, and the one the split above exists for. Its zoom bounds are not
// fields on the agent, phase card or rollup: they are a spine kind of their own, laid down last so
// that a first paint does not inflate them. So a zoom under schema 3 first fetches that team's
// bounds, then looks the subject up by the archive's stable reference, and only falls through to
// the derivation when the archive genuinely published nothing for it.
const agentBounds = harness("schema3", { start_ms: 30, end_ms: 40 }, {
  "agent:alpha::root": { activity_start_ms: 600000, activity_end_ms: 660000 }
});
agentBounds.context.zoomToActivityRange(
  { team: "alpha", id: "alpha::root", start_ms: 0, end_ms: 700000 },
  { agent_id: "alpha::root" }
);
assert.deepStrictEqual(agentBounds.calls.boundsFetched, ["alpha"]);
assert.deepStrictEqual(agentBounds.calls.zoomed, [], "the lookup is asynchronous");

const phaseBounds = harness("schema3", { start_ms: 30, end_ms: 40 }, {
  "phase:alpha::p1": { activity_start_ms: 100, activity_end_ms: 200 }
});
phaseBounds.context.zoomToActivityRange(
  { team: "alpha", id: "p1", start_ms: 0, end_ms: 700000 },
  { agent_id: "alpha::root", phase_id: "p1" }
);

const rollupBounds = harness("schema3", { start_ms: 30, end_ms: 40 }, {
  "rollup:alpha::daily::86400000": { activity_start_ms: 7, activity_end_ms: 8 }
});
rollupBounds.context.zoomToActivityRange(
  { team: "alpha", kind: "daily", start_ms: 86400000, end_ms: 172800000 },
  null
);

// Nothing published for this subject: the reader must not invent a frame, it must fall through to
// the same derivation schema 2 uses, which means fetching the interval's detail first.
const unpublished = harness("schema3", { start_ms: 30, end_ms: 40 }, {});
unpublished.context.zoomToActivityRange(
  { team: "alpha", id: "ghost", start_ms: 0, end_ms: 700000 },
  { agent_id: "ghost" }
);

// The single-team archive, which is the shape the whole feature quietly lost.
//
// `render.py` stamps `team` on agents and on nothing else -- there is one team, and schema 1 never
// carried the field -- so a phase and a rollup arrive here with no team, and the reference they
// are keyed by cannot be built without one. The writer's `_team_of` and the CLI reader's `_unwrap`
// both fall back to the sole team; without the same fallback in the bundle, every phase and rollup
// zoom on every single-team archive resolves to "" and degrades to fetching the day shards its
// subject overlaps -- for a monthly rollup, most of the archive -- to recompute two numbers the
// spine already published. It is not an error and there is nothing on screen to see, which is why
// it is asserted here rather than noticed later.
const solePhase = harness("schema3", { start_ms: 30, end_ms: 40 }, {
  "phase:only::p1": { activity_start_ms: 100, activity_end_ms: 200 }
}, ["only"]);
solePhase.context.zoomToActivityRange({ id: "p1", start_ms: 0, end_ms: 700000 }, { phase_id: "p1" });
assert.deepStrictEqual(solePhase.calls.boundsFetched, ["only"]);

const soleRollup = harness("schema3", { start_ms: 30, end_ms: 40 }, {
  "rollup:only::daily::86400000": { activity_start_ms: 7, activity_end_ms: 8 }
}, ["only"]);
soleRollup.context.zoomToActivityRange(
  { kind: "daily", start_ms: 86400000, end_ms: 172800000 },
  null
);

Promise.resolve().then(function () {
  assert.deepStrictEqual(sharded.calls.zoomed, [[30, 40]]);
  assert.deepStrictEqual(agentBounds.calls.zoomed, [[600000, 660000]]);
  assert.strictEqual(agentBounds.calls.scanned, 0);
  assert.strictEqual(agentBounds.calls.fetched, 0, "the published record replaces the scan");
  assert.deepStrictEqual(phaseBounds.calls.zoomed, [[100, 200]]);
  assert.deepStrictEqual(rollupBounds.calls.zoomed, [[7, 8]]);
  assert.deepStrictEqual(unpublished.calls.zoomed, []);
  assert.strictEqual(unpublished.calls.fetched, 1);
  assert.deepStrictEqual(
    solePhase.calls.zoomed,
    [[100, 200]],
    "a phase on a single-team archive must find its published bounds"
  );
  assert.strictEqual(solePhase.calls.fetched, 0, "and must not fetch day shards to recompute them");
  assert.deepStrictEqual(soleRollup.calls.zoomed, [[7, 8]]);
  assert.strictEqual(soleRollup.calls.fetched, 0);
  return Promise.resolve();
}).then(function () {
  assert.deepStrictEqual(unpublished.calls.zoomed, [[30, 40]]);
  console.log("timeline zoom UI tests passed");
}).catch(function (error) {
  process.stderr.write(String((error && error.stack) || error) + "\n");
  process.exitCode = 1;
});
