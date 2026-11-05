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

const names = [
  "text",
  "number",
  "hasField",
  "summaryFlagAvailable",
  "agentSummaryAvailable",
  "phaseSummaryAvailable",
  "phaseDetailSummaryAvailable",
  "rollupSummaryAvailable",
  "rollupAudienceAvailable",
  "lowerSearchText",
  "agentOfficialLeaf",
  "agentShortName",
  "agentOfficialName",
  "namesEqual",
  "agentSecondaryName",
  "agentAccessibleName",
  "agentLifetimeSummary",
  "frontEllipsize",
  "truncateLabel",
  "fitAgentSecondaryName",
  "agentTooltipIdentity",
  "agentSearchText",
  "phaseSearchText"
];
const context = {};
vm.createContext(context);
vm.runInContext(names.map(functionSource).join("\n"), context);

const agent = {
  id: "thread-7",
  team: "codex-widget",
  short_name: "Budget overlap audit",
  official_name: "/root/transcript_auditor/owner_turn_miner/plugin_layout_audit/budget_overlap_audit",
  official_leaf: "budget_overlap_audit",
  nickname: "overlap-checker",
  lifetime_summary: "Audited overlapping CPU and wall-clock budgets, then verified exact-head release receipts.",
  label: "legacy label",
  status: "complete"
};

assert.strictEqual(context.agentShortName(agent), "Budget overlap audit");
assert.strictEqual(context.agentOfficialName(agent), agent.official_name);
assert.match(context.agentSecondaryName(agent), /\/root\/transcript_auditor/);
assert.match(context.agentSecondaryName(agent), /coordinator: overlap-checker/);
assert.match(context.agentAccessibleName(agent), /Short name: Budget overlap audit/);
assert.match(context.agentAccessibleName(agent), /Official name: \/root\/transcript_auditor/);
assert.match(context.agentAccessibleName(agent), /Coordinator nickname: overlap-checker/);
assert.match(context.agentAccessibleName(agent), /Lifetime summary: Audited overlapping CPU/);

const tooltipIdentity = context.agentTooltipIdentity(agent, true);
assert.doesNotMatch(tooltipIdentity, /Short name:/);
assert.match(tooltipIdentity, /^Official: ….*budget_overlap_audit/m);
assert.match(tooltipIdentity, /Coordinator nickname: overlap-checker/);
assert.match(tooltipIdentity, /Audited overlapping CPU and wall-clock budgets/);

const searchText = context.agentSearchText(agent);
assert.ok(searchText.includes("budget overlap audit"), "short name must be searchable");
assert.ok(searchText.includes("plugin_layout_audit"), "full official path must be searchable");
assert.ok(searchText.includes("overlap-checker"), "coordinator nickname must be searchable");
assert.ok(searchText.includes("wall-clock budgets"), "lifetime summary must be searchable");

const unavailableSearchText = context.agentSearchText({
  ...agent,
  summary_available: false,
  lifetime_summary: "Summary unavailable normalized logs"
});
assert.ok(!unavailableSearchText.includes("normalized logs"));
assert.strictEqual(
  context.phaseSearchText({
    id: "phase-raw",
    phrase: "Summary unavailable",
    paragraph: "Inspect normalized logs",
    summary_available: false
  }),
  "phase-raw"
);

assert.strictEqual(
  context.truncateLabel(agent.official_name, 1000, true),
  agent.official_name,
  "wide labels retain the complete official path"
);
assert.match(
  context.truncateLabel(agent.official_name, 115, true),
  /^….*budget_overlap_audit$/,
  "narrow labels retain the identifying leaf"
);
assert.match(
  context.fitAgentSecondaryName(agent, 150),
  /^….*budget_overlap_audit$/,
  "a constrained secondary line prioritizes the official path over a nickname suffix"
);
assert.match(
  context.fitAgentSecondaryName(agent, 1000),
  /coordinator: overlap-checker$/,
  "a wide secondary line adds the coordinator nickname"
);

const legacyAgent = {
  id: "legacy-id",
  path: "/root/legacy_parent/legacy_leaf",
  nickname: "legacy-nick"
};
assert.strictEqual(context.agentShortName(legacyAgent), "legacy-nick");
assert.strictEqual(context.agentOfficialName(legacyAgent), legacyAgent.path);
assert.strictEqual(
  context.agentSummaryAvailable(legacyAgent),
  true,
  "archives written before availability flags remain summary-compatible"
);

const sparseAgent = Object.assign({}, agent, {
  summary_available: false,
  lifetime_summary: "This stale placeholder must not be presented as a summary."
});
assert.strictEqual(context.agentSummaryAvailable(sparseAgent), false);
assert.strictEqual(
  context.agentLifetimeSummary(sparseAgent),
  "Summary not generated for this agent lifetime."
);
assert.match(context.agentAccessibleName(sparseAgent), /Summary not generated/);
assert.doesNotMatch(context.agentAccessibleName(sparseAgent), /stale placeholder/);

assert.strictEqual(context.phaseSummaryAvailable({}), true);
assert.strictEqual(context.phaseSummaryAvailable({ summary_available: false }), false);
assert.strictEqual(
  context.phaseDetailSummaryAvailable({}, { summary_available: false }),
  false,
  "phase detail inherits the timeline record's availability"
);
assert.strictEqual(
  context.phaseDetailSummaryAvailable(
    { summary_available: false },
    { summary_available: true }
  ),
  false,
  "an explicit detail flag takes precedence"
);
assert.strictEqual(context.rollupSummaryAvailable({ path: "legacy.md" }), true);
assert.strictEqual(
  context.rollupSummaryAvailable({
    technical_summary_available: false,
    plain_language_summary_available: false
  }),
  false
);
assert.strictEqual(
  context.rollupAudienceAvailable(
    {
      summary_available: false,
      path: "legacy-path-must-not-be-used.md"
    },
    "technical_summary_available"
  ),
  false,
  "explicit unavailability overrides a legacy path"
);

assert.match(
  source,
  /truncateLabel\(shortName, textWidth, false\)/,
  "track primary label must render the hindsight short name"
);
assert.match(
  source,
  /"aria-label": agentAccessibleName\(agent\)/,
  "track labels must expose all names to assistive technology"
);
assert.match(
  source,
  /Hierarchy depth: " \+ depth/,
  "arbitrary nesting depth must remain explicit"
);
assert.match(
  source,
  /"aria-label": text\(edge\.phrase, kind \+ " interaction"\) \+ "\. " \+ edgeRouteDetail\(edge\)/,
  "edge accessibility must identify both endpoint agents"
);
assert.match(
  source,
  /showModalEdgeRoute\(edge\)/,
  "edge detail modals must render both endpoint identities"
);

console.log("agent naming UI tests passed");
