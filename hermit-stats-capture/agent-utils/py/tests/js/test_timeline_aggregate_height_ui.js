"use strict";

// The aggregate view's team block encodes how big the team was during the bin, and the whole
// value of that encoding is that a reader can compare two blocks. It could not: the height was
// `log2(1 + average)`, under which three concurrent agents drew twice one agent and ten drew
// three and a half times it. A picture that says "busier" while refusing to say how much busier
// is not a measurement, and nobody reading it knows they are being misled -- that is what these
// tests pin. Height is now a straight multiple of a count.
//
// A log scale was buying something real, though, and these tests pin the replacement too. Log
// compressed outliers; linear does not, so a burst far above typical concurrency cannot be drawn
// at true scale in a 78px row. It is clamped, and a clamped block is MARKED, because the failure
// mode of a silent clamp is worse than the log's: a sixty-agent hour drawn exactly as tall as a
// nine-agent one, with nothing on screen saying so.
//
// The constants are read out of `app.js` rather than restated here. A test that copies the
// number it is checking passes forever after somebody changes the source.

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const appPath = path.resolve(__dirname, "../../wrkviz/static/app.js");
const source = fs.readFileSync(appPath, "utf8");

function functionSource(name) {
  const marker = "  function " + name + "(";
  const start = source.indexOf(marker);
  assert.notStrictEqual(start, -1, "missing function " + name);
  const bodyStart = source.indexOf("{", start);
  let depth = 0;
  for (let index = bodyStart; index < source.length; index += 1) {
    const character = source[index];
    if (character === "{") {
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

function constant(name) {
  const match = source.match(new RegExp("\\n  var " + name + " = ([0-9.]+);"));
  assert.notStrictEqual(match, null, "missing constant " + name);
  return Number(match[1]);
}

const PIXELS_PER_AGENT = constant("AGGREGATE_PIXELS_PER_AGENT");
const MAX_HEIGHT = constant("AGGREGATE_BIN_MAX_HEIGHT");
const MIN_HEIGHT = constant("AGGREGATE_BIN_MIN_HEIGHT");
const TEAM_HEIGHT = constant("AGGREGATE_TEAM_HEIGHT");

const context = {
  AGGREGATE_PIXELS_PER_AGENT: PIXELS_PER_AGENT,
  AGGREGATE_BIN_MAX_HEIGHT: MAX_HEIGHT,
  AGGREGATE_BIN_MIN_HEIGHT: MIN_HEIGHT
};
vm.createContext(context);
vm.runInContext(
  ["number", "clamp", "activityBinHeight", "saturationEdgePoints"]
    .map(functionSource).join("\n"),
  context
);

const height = context.activityBinHeight;

// -- linear, which is the entire point ------------------------------------------------------

// Three agents present, three times the height. Under the log this ratio was 2.00, and the
// difference is not academic: it is the difference between reading a team's size off the chart
// and guessing it.
const one = height(0, 1);
const three = height(0, 3);
assert.strictEqual(three.height / one.height, 3, "three agents draw three times one agent");
assert.strictEqual(one.height, PIXELS_PER_AGENT);
assert.strictEqual(three.height, 3 * PIXELS_PER_AGENT);
assert.ok(
  Math.abs(Math.log2(1 + 3) / Math.log2(1 + 1) - 2) < 1e-9,
  "the superseded log rule really did draw three agents at twice one agent"
);

// Linear all the way up to the cap, at every fractional count, not merely at the integers a
// hand-written table would have checked.
for (let agents = MIN_HEIGHT / PIXELS_PER_AGENT; agents <= MAX_HEIGHT / PIXELS_PER_AGENT;
     agents += 0.25) {
  assert.ok(
    Math.abs(height(0, agents).height - agents * PIXELS_PER_AGENT) < 1e-9,
    "height is exactly " + PIXELS_PER_AGENT + "px per agent at " + agents
  );
}

// The coordinator is an agent, counted as the fraction of the bin it has evidence for. That is
// what lets the rule be a plain multiple of a count: the old formula added a flat 10px pedestal
// whenever the coordinator showed up at all, and a pedestal destroys the ratio -- one worker plus
// coordinator drew 16px and three plus coordinator drew 28px, 1.75x for three times the team.
assert.strictEqual(height(1, 0).height, PIXELS_PER_AGENT, "a lone coordinator is one agent tall");
assert.strictEqual(height(1, 2).height, 3 * PIXELS_PER_AGENT);
assert.strictEqual(height(1, 2).height / height(1, 0).height, 3);
assert.strictEqual(height(0.5, 0.5).height, PIXELS_PER_AGENT, "presence adds, fractionally");
assert.strictEqual(height(1, 0).agents, 1);
assert.strictEqual(height(0.25, 2).agents, 2.25);

// Coverage fractions arrive from the archive and are trusted only as far as their range.
assert.strictEqual(height(4, 0).agents, 1, "coordinator presence saturates at one whole agent");
assert.strictEqual(height(-1, 2).agents, 2);
assert.strictEqual(height(0, -5).agents, 0);
assert.strictEqual(height(NaN, NaN).agents, 0);
assert.strictEqual(height(undefined, undefined).height, MIN_HEIGHT);

// -- the floor is a hit target, not a measurement -------------------------------------------

const trace = height(0.01, 0);
assert.strictEqual(trace.height, MIN_HEIGHT, "a trace of activity is still clickable");
assert.strictEqual(trace.saturated, false);
assert.ok(MIN_HEIGHT >= 4, "a 4px target is the smallest a pointer can reasonably hit");

// -- what a log used to buy: outliers, now clamped AND marked -------------------------------

const capacity = MAX_HEIGHT / PIXELS_PER_AGENT;
assert.strictEqual(height(0, 1).capacityAgents, capacity);
assert.strictEqual(
  capacity,
  Math.round(capacity),
  "the cap is a whole number of agents, so the tooltip can state it exactly"
);

const atCapacity = height(0, capacity);
assert.strictEqual(atCapacity.height, MAX_HEIGHT);
assert.strictEqual(atCapacity.saturated, false, "exactly at the cap is not clamped");

const burst = height(1, 59);
assert.strictEqual(burst.height, MAX_HEIGHT, "a sixty-agent burst is clamped to the row");
assert.strictEqual(burst.saturated, true);
assert.strictEqual(burst.agents, 60, "and the true count survives, for the tooltip to state");

// The mark is spent only where it means something. A block a third of a pixel over the cap is
// not misleading anyone, and marking it would make the torn edge ordinary.
assert.strictEqual(height(0, capacity + 0.05).saturated, false, "sub-pixel overflow is not a lie");
assert.strictEqual(height(0, capacity + 0.5).saturated, true);

// The clamp cannot be quietly relaxed into overlapping the next team's row. The block is drawn
// 8px below the row top, and the row is `AGGREGATE_TEAM_HEIGHT` tall.
assert.ok(8 + MAX_HEIGHT < TEAM_HEIGHT, "a saturated block stays inside its own team row");

// -- the constant was chosen against a real archive, not for roundness ----------------------

// Measured over the activity bins of a 2,656-agent, 13,760-phase archive: per-bin agents present
// (coordinator evidence fraction plus average worker concurrency) has a median of 1.85, a p90 of
// 5.89 and a p99 of 9.49. These assertions are the reasoning, kept executable: whoever changes
// `AGGREGATE_PIXELS_PER_AGENT` finds out here which property of the distribution they broke.
assert.ok(
  height(0, 1.85).height >= 10,
  "the median bin draws as a bar, not a hairline -- a scale of three would give it 6px"
);
assert.ok(
  height(0, 5.89).saturated === false,
  "nine bins in ten are drawn at true scale -- a scale of ten would saturate at five agents"
);
assert.ok(
  height(0, 9.49).saturated === true,
  "and the torn edge stays rare: it is the top ~1% of bins that get it"
);

// -- the torn edge itself ---------------------------------------------------------------------

const wide = context.saturationEdgePoints(10, 20, 40).split(" ").map(function (pair) {
  return pair.split(",").map(Number);
});
assert.ok(wide.length >= 3, "a saw-tooth needs teeth");
assert.strictEqual(wide[0][0], 10, "the edge starts at the block's left");
assert.strictEqual(wide[wide.length - 1][0], 50, "and ends at its right");
wide.forEach(function (point, index) {
  assert.strictEqual(point[1] === 20, index % 2 === 0, "teeth alternate");
  assert.ok(point[1] >= 20, "teeth point down, into the block, never up into the row above");
});

// A one-pixel bin is where a saw-tooth is least useful and most likely to divide by zero.
const narrow = context.saturationEdgePoints(0, 0, 1).split(" ");
assert.strictEqual(narrow.length, 3);
narrow.forEach(function (pair) {
  pair.split(",").map(Number).forEach(function (value) {
    assert.ok(Number.isFinite(value), "no NaN in a degenerate width");
  });
});

// -- and it is actually wired into the render ------------------------------------------------

assert.match(source, /block\.saturated \? " is-saturated" : ""/);
assert.match(source, /"data-height-saturated": block\.saturated \? "true" : "false"/);
assert.match(source, /class: "activity-bin-overflow"/);
assert.match(
  source,
  /"\\nAgents present on average: " \+ block\.agents\.toFixed\(2\)/,
  "the tooltip states the count the block may have had to clamp"
);
assert.doesNotMatch(
  source,
  /Math\.log2\(1 \+ average\)/,
  "the log-scaled height is gone, not merely bypassed"
);
assert.match(
  fs.readFileSync(path.resolve(__dirname, "../../wrkviz/static/style.css"), "utf8"),
  /\.activity-bin-overflow \{/,
  "the torn edge has a style, or it renders invisible"
);

console.log("aggregate team-block height tests passed");
