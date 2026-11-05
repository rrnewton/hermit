"use strict";

const assert = require("assert");
const path = require("path");

const core = require(path.resolve(
  __dirname,
  "..",
  "..",
  "wrkviz",
  "static",
  "timeline-core.js"
));

const tuesdayStart = Date.UTC(2026, 7, 4, 4);
const wednesdayStart = Date.UTC(2026, 7, 5, 4);
const thursdayStart = Date.UTC(2026, 7, 6, 4);
const calendarRollups = [
  { kind: "daily", start_ms: tuesdayStart, end_ms: wednesdayStart },
  { kind: "daily", start_ms: wednesdayStart, end_ms: thursdayStart },
  {
    kind: "weekly",
    start_ms: Date.UTC(2026, 7, 3, 4),
    end_ms: Date.UTC(2026, 7, 10, 4)
  },
  {
    kind: "monthly",
    start_ms: Date.UTC(2026, 7, 1, 4),
    end_ms: Date.UTC(2026, 8, 1, 4)
  },
  {
    kind: "quarterly",
    start_ms: Date.UTC(2026, 6, 1, 4),
    end_ms: Date.UTC(2026, 9, 1, 4)
  },
  { kind: "unknown", start_ms: 0, end_ms: Number.MAX_SAFE_INTEGER },
  { kind: "daily", start_ms: 20, end_ms: 10 }
];
const eventRange = {
  start_ms: Date.UTC(2026, 7, 5, 3, 10, 54),
  end_ms: Date.UTC(2026, 7, 5, 20, 28, 31)
};
const navigationRange = core.navigableRange(eventRange, calendarRollups);
assert.deepStrictEqual(navigationRange, {
  start_ms: calendarRollups[4].start_ms,
  end_ms: calendarRollups[4].end_ms
});
calendarRollups.slice(0, 5).forEach(function (rollup) {
  assert.deepStrictEqual(
    core.boundedViewRange(
      rollup.start_ms,
      rollup.end_ms,
      navigationRange,
      1000
    ),
    { start_ms: rollup.start_ms, end_ms: rollup.end_ms },
    rollup.kind + " bounds remain navigable outside the event range"
  );
});

assert.strictEqual(
  core.semanticZoomLevel(0, 60 * 1000 * 1000, 1000),
  "detail",
  "one minute per pixel retains work phases and state strips"
);
assert.strictEqual(
  core.semanticZoomLevel(0, 60 * 1000 * 1000 + 1, 1000),
  "lifetime",
  "medium density collapses phases into one lifetime block per agent"
);
assert.strictEqual(
  core.semanticZoomLevel(0, 5 * 60 * 1000 * 1000, 1000),
  "lifetime",
  "five minutes per pixel remains in lifetime mode"
);
assert.strictEqual(
  core.semanticZoomLevel(0, 5 * 60 * 1000 * 1000 + 1, 1000),
  "aggregate",
  "outer views suppress individual interaction detail"
);
assert.strictEqual(
  core.aggregateResolution(0, 125 * 60 * 60 * 1000, 1000),
  "hourly",
  "hour bins are retained while each remains at least eight pixels wide"
);
assert.strictEqual(
  core.aggregateResolution(0, 126 * 60 * 60 * 1000, 1000),
  "daily",
  "daily bins replace narrow hour bins"
);
assert.strictEqual(
  core.aggregateResolution(0, 126 * 24 * 60 * 60 * 1000, 1000),
  "weekly",
  "weekly bins replace narrow day bins"
);

const springForwardDay = {
  kind: "daily",
  start_ms: Date.UTC(2026, 2, 8, 5),
  end_ms: Date.UTC(2026, 2, 9, 4)
};
assert.strictEqual(
  springForwardDay.end_ms - springForwardDay.start_ms,
  23 * 60 * 60 * 1000
);
const springNavigation = core.navigableRange(
  {
    start_ms: Date.UTC(2026, 2, 8, 7),
    end_ms: Date.UTC(2026, 2, 8, 12)
  },
  [springForwardDay]
);
assert.deepStrictEqual(
  core.boundedViewRange(
    springForwardDay.start_ms,
    springForwardDay.end_ms,
    springNavigation,
    1000
  ),
  { start_ms: springForwardDay.start_ms, end_ms: springForwardDay.end_ms },
  "rollup zoom uses the supplied DST-aware bounds instead of assuming 24 hours"
);

const activityStart = springForwardDay.end_ms - 42 * 60 * 1000;
assert.deepStrictEqual(
  core.activityRangeWithin(
    springForwardDay,
    {
      range: {
        start_ms: springForwardDay.end_ms - 50 * 60 * 1000,
        end_ms: springForwardDay.end_ms + 12 * 60 * 60 * 1000
      },
      agents: [{
        start_ms: springForwardDay.start_ms,
        end_ms: springForwardDay.end_ms
      }],
      phases: [
        {
          start_ms: springForwardDay.start_ms,
          end_ms: springForwardDay.end_ms,
          states: [{
            kind: "idle",
            start_ms: springForwardDay.start_ms,
            end_ms: springForwardDay.end_ms
          }]
        },
        {
          start_ms: springForwardDay.end_ms - 50 * 60 * 1000,
          end_ms: springForwardDay.end_ms,
          states: [
            {
              kind: "idle",
              start_ms: springForwardDay.end_ms - 50 * 60 * 1000,
              end_ms: activityStart
            },
            {
              kind: "active",
              start_ms: activityStart,
              end_ms: springForwardDay.end_ms
            }
          ]
        }
      ],
      events: [],
      edges: []
    },
    1000
  ),
  { start_ms: activityStart, end_ms: springForwardDay.end_ms },
  "idle states and agent lifetimes do not fill the empty portion of a rollup"
);

const sourceRollup = { start_ms: 100_000, end_ms: 200_000 };
assert.deepStrictEqual(
  core.activityRangeWithin(
    sourceRollup,
    {
      range: { start_ms: 100_000, end_ms: 200_000 },
      phases: [
        {
          start_ms: 120_000,
          end_ms: 130_000,
          states: [{ kind: "tool", start_ms: 120_000, end_ms: 130_000 }]
        },
        { start_ms: 140_000, end_ms: 150_000 }
      ],
      events: [{ at_ms: 110_000 }],
      edges: [{ source_ms: 160_000, target_ms: 190_000 }]
    },
    1000
  ),
  { start_ms: 110_000, end_ms: 190_001 },
  "events, edge endpoints, non-idle states, and unsegmented phases define activity"
);
assert.deepStrictEqual(
  core.activityRangeWithin(
    sourceRollup,
    {
      range: { start_ms: 90_000, end_ms: 180_000 },
      phases: [],
      events: [{ at_ms: 199_900 }],
      edges: []
    },
    1000
  ),
  { start_ms: 199_000, end_ms: 200_000 },
  "minimum point padding stays inside the selected rollup"
);
assert.deepStrictEqual(
  core.activityRangeWithin(
    sourceRollup,
    {
      range: { start_ms: 120_000, end_ms: 180_000 },
      phases: [],
      events: [],
      edges: []
    },
    1000
  ),
  { start_ms: 120_000, end_ms: 180_000 },
  "a rollup without activity falls back to its intersection with the data range"
);

const focusedPhase = {
  id: "focus",
  agent_id: "selected",
  start_ms: 100_000,
  end_ms: 200_000,
  states: [
    { kind: "idle", start_ms: 100_000, end_ms: 120_000 },
    { kind: "active", start_ms: 120_000, end_ms: 160_000 },
    { kind: "idle", start_ms: 160_000, end_ms: 200_000 }
  ]
};
const scopedData = {
  range: { start_ms: 0, end_ms: 300_000 },
  agents: [{ id: "selected", start_ms: 50_000, end_ms: 250_000 }],
  phases: [
    focusedPhase,
    {
      id: "overlapping",
      agent_id: "selected",
      start_ms: 100_000,
      end_ms: 200_000,
      states: [{ kind: "tool", start_ms: 100_000, end_ms: 200_000 }]
    },
    {
      id: "followup",
      agent_id: "selected",
      start_ms: 210_000,
      end_ms: 230_000,
      states: [{ kind: "active", start_ms: 215_000, end_ms: 220_000 }]
    },
    {
      id: "unrelated",
      agent_id: "other",
      start_ms: 50_000,
      end_ms: 250_000,
      states: [{ kind: "active", start_ms: 50_000, end_ms: 250_000 }]
    }
  ],
  events: [
    { agent_id: "selected", at_ms: 80_000 },
    { agent_id: "selected", at_ms: 130_000 },
    { agent_id: "other", at_ms: 105_000 }
  ],
  edges: [
    {
      source_id: "other",
      target_id: "selected",
      source_ms: 60_000,
      target_ms: 90_000
    },
    {
      source_id: "selected",
      target_id: "other",
      source_ms: 225_000,
      target_ms: 245_000
    }
  ]
};
assert.deepStrictEqual(
  core.activityRangeWithin(
    focusedPhase,
    scopedData,
    1000,
    { agent_id: "selected", phase_id: "focus" }
  ),
  { start_ms: 120_000, end_ms: 160_000 },
  "phase activity excludes its idle margins and overlapping unrelated work"
);
assert.deepStrictEqual(
  core.activityRangeWithin(
    { start_ms: 50_000, end_ms: 250_000 },
    scopedData,
    1000,
    { agent_id: "selected" }
  ),
  { start_ms: 80_000, end_ms: 225_001 },
  "agent activity uses only its phases, events, and own side of message edges"
);

const packed = core.packLifetimes([
  { id: "root", start_ms: 0, end_ms: 100, dedicated: true, input_index: 0 },
  { id: "b", start_ms: 5, end_ms: 8, official_name: "/root/b", input_index: 2 },
  { id: "a", start_ms: 0, end_ms: 10, official_name: "/root/a", input_index: 1 },
  { id: "c", start_ms: 10, end_ms: 12, official_name: "/root/c", input_index: 3 }
]);
assert.strictEqual(packed.lane_count, 3);
assert.strictEqual(packed.lane_by_id.root, 0);
assert.strictEqual(packed.lane_by_id.a, 1);
assert.strictEqual(packed.lane_by_id.b, 2);
assert.strictEqual(packed.lane_by_id.c, 1, "half-open adjacent lifetimes reuse a lane");

const visibleLifetimes = core.lifetimesWithin([
  { id: "old-root", start_ms: 0, end_ms: 100, dedicated: true },
  { id: "active-root", start_ms: 100, end_ms: 300, dedicated: true },
  { id: "active-child", start_ms: 120, end_ms: 240, dedicated: false },
  { id: "future-child", start_ms: 260, end_ms: 290, dedicated: false }
], 150, 250);
assert.deepStrictEqual(
  visibleLifetimes.map(function (item) {
    return [item.id, item.start_ms, item.end_ms];
  }),
  [
    ["active-root", 150, 250],
    ["active-child", 150, 240]
  ],
  "visible packing drops inactive roots and clips conflicts to the current range"
);
const visiblePacked = core.packLifetimes(visibleLifetimes);
assert.strictEqual(visiblePacked.lane_count, 2);
assert.strictEqual(visiblePacked.lane_by_id["active-root"], 0);
assert.strictEqual(visiblePacked.lane_by_id["active-child"], 1);

let selection = core.nextPhaseSelection(null, "agent-a", "phase-1", 10, 20);
assert.deepStrictEqual(selection, { kind: "agent", agent_id: "agent-a" });
selection = core.nextPhaseSelection(selection, "agent-a", "phase-1", 10, 20);
assert.deepStrictEqual(selection, {
  kind: "phase",
  agent_id: "agent-a",
  phase_id: "phase-1",
  start_ms: 10,
  end_ms: 20
});
selection = core.nextPhaseSelection(selection, "agent-a", "phase-2", 21, 30);
assert.deepStrictEqual(selection, {
  kind: "phase",
  agent_id: "agent-a",
  phase_id: "phase-2",
  start_ms: 21,
  end_ms: 30
}, "a different phase on the selected agent becomes the phase selection");
selection = core.nextPhaseSelection(selection, "agent-a", "phase-2", 21, 30);
assert.deepStrictEqual(selection, { kind: "agent", agent_id: "agent-a" });
selection = core.nextPhaseSelection(selection, "agent-a", "phase-1", 10, 20);
assert.deepStrictEqual(selection, {
  kind: "phase",
  agent_id: "agent-a",
  phase_id: "phase-1",
  start_ms: 10,
  end_ms: 20
});
selection = core.nextPhaseSelection(selection, "agent-a", "phase-1", 10, 20);
assert.deepStrictEqual(selection, { kind: "agent", agent_id: "agent-a" });

const spawn = {
  id: "spawn",
  kind: "spawn",
  source_id: "parent",
  target_id: "agent-a",
  source_ms: 9,
  target_ms: 10
};
const inPhase = {
  id: "message-in",
  kind: "message",
  source_id: "agent-a",
  target_id: "parent",
  source_ms: 15,
  target_ms: 15
};
const result = {
  id: "result",
  kind: "result",
  source_id: "agent-a",
  target_id: "parent",
  source_ms: 20,
  target_ms: 20
};
const continuation = {
  id: "continuation",
  kind: "continuation",
  source_id: "parent",
  target_id: "agent-a",
  source_ms: 9,
  target_ms: 10
};
const outOfPhase = {
  id: "message-out",
  kind: "message",
  source_id: "agent-a",
  target_id: "parent",
  source_ms: 25,
  target_ms: 25
};
const phaseSelection = {
  kind: "phase",
  agent_id: "agent-a",
  phase_id: "phase-1",
  start_ms: 10,
  end_ms: 20
};
assert.strictEqual(core.edgeDisplayState(spawn, null, false, true), "normal");
assert.strictEqual(
  core.edgeDisplayState(continuation, null, false, false),
  "normal",
  "an explicit session continuation remains visible when detailed edges are off"
);
assert.strictEqual(
  core.edgeDisplayState(continuation, phaseSelection, false, false),
  "highlighted",
  "an explicit session continuation highlights as structural context"
);
assert.strictEqual(
  core.edgeDisplayState(result, null, false, false),
  "normal",
  "the terminal child-to-parent join remains visible when detailed edges are off"
);
assert.strictEqual(
  core.edgeDisplayState(result, phaseSelection, false, false),
  "highlighted",
  "the structural join highlights without the highlighted-detailed toggle"
);
assert.strictEqual(
  core.edgeDisplayState(result, { kind: "agent", agent_id: "unrelated" }, false, false),
  "dimmed",
  "an unrelated structural join dims instead of disappearing"
);
assert.strictEqual(core.edgeDisplayState(inPhase, null, false, true), "hidden");
assert.strictEqual(core.edgeDisplayState(inPhase, phaseSelection, false, true), "highlighted");
assert.strictEqual(core.edgeDisplayState(outOfPhase, phaseSelection, false, true), "hidden");
assert.strictEqual(core.edgeDisplayState(outOfPhase, phaseSelection, true, true), "dimmed");

console.log("timeline core tests passed");
